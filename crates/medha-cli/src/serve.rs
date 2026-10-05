//! `medha serve`: one backend for this user. It holds every chat and serves any
//! number of clients over a private local channel; a client proves it holds the
//! token before it is heard. Nothing listens on the network.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

use crate::config;
use crate::serve_chats::ServeChats;

const ROLES: wire::Roles = wire::Roles {
    host: "backend",
    guest: "client",
};

/// Longer than the MCP manager gives a connection to close.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Where a client finds the backend: `address` names the channel, `token` opens it.
pub(crate) fn directory(home: &Path) -> PathBuf {
    home.join("serve")
}

#[cfg(unix)]
fn address(directory: &Path, channel: &str) -> String {
    directory
        .join(format!("{channel}.sock"))
        .display()
        .to_string()
}

/// Pipe names are machine-wide, so the name is taken from this user's own folder.
#[cfg(windows)]
fn address(directory: &Path, channel: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(directory.to_string_lossy().as_bytes());
    let tag: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!(r"\\.\pipe\medha-serve-{channel}-{tag}")
}

/// The shared MCP host, running until the backend stops it.
struct McpHost {
    endpoint: mcp::hub::Endpoint,
    /// Told when a server or a key was saved, so the host follows at once.
    changed: Arc<tokio::sync::Notify>,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl McpHost {
    /// Closes the remote connections and gives the host's address back.
    async fn stop(self) {
        drop(self.stop);
        let _ = tokio::time::timeout(STOP_GRACE, self.task).await;
    }
}

/// The user's remote MCP servers connect once, here, and every chat attaches.
/// A chat that finds no host answering runs its own, as it does without a backend.
fn shared_mcp(directory: &Path) -> anyhow::Result<McpHost> {
    let endpoint = mcp::hub::Endpoint {
        address: address(directory, "mcp"),
        token: new_token()?,
    };
    let (address, token) = (endpoint.address.clone(), endpoint.token.as_str().into());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let changed = Arc::new(tokio::sync::Notify::new());
    let task = tokio::spawn({
        let changed = Arc::clone(&changed);
        async move {
            let stopped = async move {
                let _ = stopped.await;
            };
            let served = crate::mcp_host::run_shared(&address, token, changed, stopped).await;
            if let Err(error) = served {
                tracing::warn!("the shared MCP host stopped: {error:#}");
            }
        }
    });
    Ok(McpHost {
        endpoint,
        changed,
        stop,
        task,
    })
}

/// Ctrl-C, or the signal a process is stopped with: both end the backend the same way.
async fn asked_to_stop() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let (Ok(mut ended), Ok(mut hung_up)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = ended.recv() => {}
                _ = hung_up.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())
}

fn new_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!("no random source: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn client<S>(backend: Arc<backend::Backend<ServeChats>>, stream: S, token: Arc<str>)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let Some(admitted) = wire::admit(&mut reader, &mut write, &token, ROLES).await else {
        return;
    };
    let welcome = json!({ "id": admitted, "result": {
        "backend": env!("CARGO_PKG_VERSION"), "protocol": backend::PROTOCOL,
    }});
    if wire::write_frame(&mut write, &welcome).await {
        backend.serve(reader, write).await;
    }
}

/// A chat holds about eleven open files. An app started from the desktop is
/// allowed 256 at first, which is some twenty chats, so the backend takes what
/// the system lets it have.
#[cfg(unix)]
fn allow_many_open_files() {
    const WANTED: libc::rlim_t = 10_240;
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid rlimit for both calls.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 && limit.rlim_cur < WANTED {
            limit.rlim_cur = WANTED.min(limit.rlim_max);
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}

pub async fn run(_args: &[String]) -> anyhow::Result<()> {
    #[cfg(unix)]
    allow_many_open_files();
    let home = config::medha_home()?;
    let directory = directory(&home);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    // Held for life: a second backend would take the first one's address from under it.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("lock"))?;
    if lock.try_lock().is_err() {
        anyhow::bail!("a Medha backend is already running for {}", home.display());
    }

    let logs = home.join("logs");
    std::fs::create_dir_all(&logs).ok();
    let (log_writer, _log_guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(&logs, "serve.log"));
    tracing_subscriber::fmt()
        .with_writer(log_writer)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let token: Arc<str> = new_token()?.into();
    let address = address(&directory, "backend");
    let listener =
        wire::bind(&address).with_context(|| format!("could not listen on {address}"))?;
    let mcp_host = shared_mcp(&directory)?;
    let chats = ServeChats::new(mcp_host.endpoint.clone(), Arc::clone(&mcp_host.changed));
    let backend = backend::Backend::new(chats, env!("CARGO_PKG_VERSION"));
    // Written once the address is taken: a client that finds these is not refused.
    private(&directory.join("token"), &token)?;
    private(&directory.join("address"), &address)?;
    tracing::info!(address = %address, "medha backend listening");
    println!("medha backend listening");

    let serving = listener.serve({
        let backend = Arc::clone(&backend);
        move |stream| {
            tokio::spawn(client(Arc::clone(&backend), stream, Arc::clone(&token)));
        }
    });
    let outcome = tokio::select! {
        served = serving => served.with_context(|| format!("could not listen on {address}")),
        () = asked_to_stop() => Ok(()),
    };
    for name in ["token", "address"] {
        let _ = std::fs::remove_file(directory.join(name));
    }
    #[cfg(unix)]
    let _ = std::fs::remove_file(&address);
    mcp_host.stop().await;
    drop(lock);
    outcome
}
