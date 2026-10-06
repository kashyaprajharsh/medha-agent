//! One connected client: its requests to the backend, and its requests to the
//! chats it has attached to. It may only speak to a chat it is attached to.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::session::{KEPT_FRAMES, Session};
use crate::{Backend, Chats, Failure, PROTOCOL, refused};

/// Room for a full replay and then some; a client further behind than this is dropped.
const QUEUE: usize = KEPT_FRAMES + 1024;

/// What may wait for one client, in bytes: a full replay, the largest answer, and room beside them.
pub(crate) const QUEUED_BYTES: usize = 4 * wire::MAX_FRAME;
pub(crate) const TOO_LARGE: &str = "the answer is too large to send at once";

#[derive(Clone)]
pub(crate) struct Client {
    pub(crate) id: u64,
    out: mpsc::Sender<Arc<str>>,
    waiting: Arc<AtomicUsize>,
    dropped: CancellationToken,
}

impl Client {
    /// Never waits: a client that cannot keep up, by frames or by bytes, is
    /// disconnected, and may come back with its cursor.
    pub(crate) fn send(&self, line: Arc<str>) -> bool {
        let bytes = line.len();
        let within = self.waiting.fetch_add(bytes, Ordering::Relaxed) + bytes <= QUEUED_BYTES;
        let sent = within && self.out.try_send(line).is_ok();
        if !sent {
            self.waiting.fetch_sub(bytes, Ordering::Relaxed);
            self.dropped.cancel();
        }
        sent
    }

    /// An answer no client could read is refused in a few words, not sent to break its connection.
    pub(crate) fn reply(&self, id: Option<Value>, outcome: Result<Value, Failure>) {
        let Some(id) = id else { return };
        let failed = |(code, message): Failure| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
        let frame = match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(failure) => failed(failure),
        };
        let mut line = frame.to_string();
        if line.len() >= wire::MAX_FRAME {
            line = failed(refused(TOO_LARGE)).to_string();
        }
        line.push('\n');
        self.send(line.into());
    }
}

pub(crate) async fn serve<C, R, W>(backend: &Arc<Backend<C>>, reader: R, mut writer: W)
where
    C: Chats,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut queued) = mpsc::channel::<Arc<str>>(QUEUE);
    let me = Client {
        id: backend.clients.fetch_add(1, Ordering::Relaxed),
        out,
        waiting: Arc::default(),
        dropped: CancellationToken::new(),
    };
    let writing = tokio::spawn({
        let (dropped, waiting) = (me.dropped.clone(), Arc::clone(&me.waiting));
        async move {
            let writes = async {
                while let Some(line) = queued.recv().await {
                    waiting.fetch_sub(line.len(), Ordering::Relaxed);
                    if writer.write_all(line.as_bytes()).await.is_err()
                        || writer.flush().await.is_err()
                    {
                        break;
                    }
                }
            };
            // A write to a client that stopped reading must not outlive its being dropped.
            tokio::select! {
                () = writes => {}
                () = dropped.cancelled() => {}
            }
            dropped.cancel();
        }
    });
    let mut reader = BufReader::new(reader);
    let mut leaving = Leaving {
        backend: Arc::clone(backend),
        me: me.clone(),
        attached: HashSet::new(),
        tied: Tied::default(),
    };
    loop {
        let frame = tokio::select! {
            () = me.dropped.cancelled() => break,
            frame = wire::read_frame(&mut reader) => frame,
        };
        let Some(frame) = frame else { break };
        let tied = leaving.tied.clone();
        handle(backend, &me, &mut leaving.attached, &tied, frame).await;
    }
    drop(leaving);
    let _ = writing.await;
}

/// What a client leaves behind is cleared however its serving ends, a request that panics included.
struct Leaving<C: Chats> {
    backend: Arc<Backend<C>>,
    me: Client,
    attached: HashSet<String>,
    tied: Tied,
}

impl<C: Chats> Drop for Leaving<C> {
    fn drop(&mut self) {
        for id in self.attached.drain() {
            if let Ok(session) = self.backend.find(&id) {
                session.detach(self.me.id);
            }
        }
        // Chats may still hold this client for answers it will never read.
        self.me.dropped.cancel();
        self.tied.close();
    }
}

/// Chats a client started tied: each ends once nobody watches it. This is the
/// client's list of them, for the one case the chat cannot see for itself: its
/// starter going before anyone watched it at all.
#[derive(Clone, Default)]
struct Tied(Arc<Mutex<Vec<Weak<Session>>>>);

impl Tied {
    fn add(&self, session: &Arc<Session>) {
        session.tie();
        let mut tied = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        tied.retain(|chat| chat.strong_count() > 0);
        tied.push(Arc::downgrade(session));
    }

    fn close(&self) {
        let tied = std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner));
        for session in tied.iter().filter_map(Weak::upgrade) {
            session.close_if_unwatched();
        }
    }
}

async fn handle<C: Chats>(
    backend: &Arc<Backend<C>>,
    me: &Client,
    attached: &mut HashSet<String>,
    tied: &Tied,
    mut frame: Value,
) {
    let id = frame.get("id").cloned();
    let Some(method) = frame
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let named = frame
        .as_object_mut()
        .and_then(|frame| frame.remove("session"))
        .and_then(|session| session.as_str().map(str::to_owned));
    let session = |needed: &str| {
        let id = named
            .as_deref()
            .ok_or_else(|| (-32602, format!("{needed} needs a session")))?;
        backend.find(id)
    };
    // These can take long, so the client's other requests are not held behind them.
    let about_folder = named.is_none() && frame.get("folder").is_some();
    if method == "session.create" || about_folder {
        let (backend, me, tied) = (Arc::clone(backend), me.clone(), tied.clone());
        tokio::spawn(async move {
            let outcome = match about_folder {
                true => backend.chats.about_folder(&frame).await.map_err(refused),
                false => backend.create(&frame["params"]).await,
            };
            let made = outcome
                .as_ref()
                .ok()
                .and_then(|made| made["session"].as_str());
            if frame["params"]["ends_with_client"] == true
                && let Some(session) = made.and_then(|id| backend.find(id).ok())
            {
                tied.add(&session);
                // The client may have gone while its chat was starting.
                if me.dropped.is_cancelled() {
                    tied.close();
                }
            }
            me.reply(id, outcome);
        });
        return;
    }
    // A request that names a chat is the chat's, whatever it is called, unless it is about attaching.
    let outcome = match method.as_str() {
        "hello" if named.is_none() => {
            Ok(json!({ "backend": backend.version, "protocol": PROTOCOL }))
        }
        "session.list" if named.is_none() => Ok(backend.list()),
        "session.attach" => session("session.attach").map(|session| {
            attached.insert(session.id.clone());
            session.attach(me, frame["params"]["after"].as_u64())
        }),
        "session.detach" => session("session.detach").map(|session| {
            attached.remove(&session.id);
            session.detach(me.id);
            json!({ "detached": true })
        }),
        "session.close" => match session("session.close") {
            Ok(session) if session.heard_by(me.id) => {
                session.close();
                Ok(json!({ "closing": true }))
            }
            Ok(_) => Err(refused("attach to the session first")),
            Err(error) => Err(error),
        },
        _ => match session("a request to a chat") {
            Ok(session) if session.heard_by(me.id) => {
                match session.forward(me, frame) {
                    // The chat answers; its answer is routed back to this client.
                    Ok(()) => return,
                    Err(error) => Err(refused(error)),
                }
            }
            Ok(_) => Err(refused("attach to the session first")),
            Err(error) => Err(error),
        },
    };
    me.reply(id, outcome);
}
