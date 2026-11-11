//! A local server's stdio with one size limit per message, where a message over it is reported, not lost in silence.

use std::{
    io,
    pin::Pin,
    process::Stdio,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::{Transport, async_rw::AsyncRwTransport};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::watch;

/// The most one message from a server may be, local or remote; images sent as base64 make large replies ordinary.
pub(crate) const MAX_MESSAGE: usize = 64 * 1024 * 1024;
/// How long a closing server is given to exit after its input closes, and again after it is asked to stop.
const EXIT_WAIT: Duration = Duration::from_secs(3);

/// Counts the messages a server sent over the limit.
pub(crate) type Oversized = watch::Sender<u64>;

pub(crate) fn oversized() -> Arc<Oversized> {
    Arc::new(watch::channel(0).0)
}

/// Resolves when the server sends a message over the limit after `seen` was taken; never without one.
pub(crate) async fn sent_oversized(seen: Option<&mut watch::Receiver<u64>>) {
    if let Some(seen) = seen
        && seen.changed().await.is_ok()
    {
        return;
    }
    std::future::pending().await
}

/// A server process spoken to over its stdin and stdout.
pub(crate) struct ChildStdio {
    child: Option<Child>,
    transport: AsyncRwTransport<RoleClient, Measured<ChildStdout>, ChildStdin>,
}

impl ChildStdio {
    pub(crate) fn spawn(mut command: Command, oversized: Arc<Oversized>) -> io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn()?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(io::Error::other("the server's stdio was not piped"));
        };
        let stdout = Measured {
            inner: stdout,
            line: 0,
            over: false,
            oversized,
        };
        let transport = AsyncRwTransport::new(stdout, stdin).with_max_line_length(MAX_MESSAGE);
        Ok(Self {
            child: Some(child),
            transport,
        })
    }

    pub(crate) fn id(&self) -> Option<u32> {
        self.child.as_ref()?.id()
    }

    /// Closes its input and waits for it to exit, then asks it to stop, then kills it.
    async fn shut_down(&mut self) -> io::Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        self.transport.close().await?;
        if tokio::time::timeout(EXIT_WAIT, child.wait()).await.is_err() {
            #[cfg(unix)]
            if let Some(pid) = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()) {
                // SAFETY: signals only the server this transport started, which has not been reaped.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
            if tokio::time::timeout(EXIT_WAIT, child.wait()).await.is_err() {
                child.kill().await?;
            }
        }
        self.child = None;
        Ok(())
    }
}

impl Transport<RoleClient> for ChildStdio {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.transport.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.transport.receive()
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.shut_down()
    }
}

/// The server's output, measured line by line with the same rule the transport drops a line by.
pub(crate) struct Measured<R> {
    inner: R,
    line: usize,
    over: bool,
    oversized: Arc<Oversized>,
}

impl<R> Measured<R> {
    fn note(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let end = bytes.iter().position(|byte| *byte == b'\n');
            let taken = end.map_or(bytes.len(), |at| at + 1);
            self.line = self.line.saturating_add(taken);
            if self.line > MAX_MESSAGE && !self.over {
                self.over = true;
                self.oversized.send_modify(|count| *count += 1);
            }
            if end.is_some() {
                (self.line, self.over) = (0, false);
            }
            bytes = &bytes[taken..];
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Measured<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) {
            this.note(&buf.filled()[before..]);
        }
        polled
    }
}
