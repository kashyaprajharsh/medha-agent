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
const STOP_GRACE: std::time::Duration = wire::SHUTDOWN_GRACE;
const IDLE: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Where a client finds the backend: `address` names the channel, `token` opens it.
pub(crate) fn directory(home: &Path) -> PathBuf {
    home.join("serve")
}

/// Names this user's own serve folder in a few characters.
fn tag(directory: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(directory.to_string_lossy().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// A socket's path may be only about a hundred bytes. One that would be longer
/// beside the token, under a deep home, goes in a short private folder; the
/// `address` file says where, so a client never works the path out for itself.
#[cfg(unix)]
fn address(directory: &Path, channel: &str) -> String {
    const SOCKET_PATH: usize = 100;
    let beside = directory.join(format!("{channel}.sock"));
    let place = if beside.as_os_str().len() < SOCKET_PATH {
        beside
    } else {
        // TMPDIR itself can be deeply nested. This user-owned private folder
        // under the system's short temporary root has a bounded socket path.
        std::path::PathBuf::from("/tmp")
            .join(format!("medha-{}", tag(directory)))
            .join(format!("{channel}.sock"))
    };
    place.display().to_string()
}

/// Pipe names are machine-wide, so the name is taken from this user's own folder.
#[cfg(windows)]
fn address(directory: &Path, channel: &str) -> String {
    format!(r"\\.\pipe\medha-serve-{channel}-{}", tag(directory))
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
    /// Closes the remote connections and gives the host's address back. Says whether it had finished in time.
    async fn stop(self) -> bool {
        drop(self.stop);
        matches!(
            tokio::time::timeout(STOP_GRACE, self.task).await,
            Ok(Ok(()))
        )
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
    let admitted = tokio::select! {
        () = backend.stopping() => return,
        admitted = wire::admit(&mut reader, &mut write, &token, ROLES) => admitted,
    };
    let Some(admitted) = admitted else {
        return;
    };
    let welcome = json!({ "id": admitted, "result": backend.identity() });
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
    let positive = |name: &str, default: u64| -> anyhow::Result<u64> {
        let value = std::env::var(name)
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()
            .with_context(|| format!("{name} must be a positive integer"))?
            .unwrap_or(default);
        anyhow::ensure!(value > 0, "{name} must be a positive integer");
        Ok(value)
    };
    let idle =
        std::time::Duration::from_secs(positive("MEDHA_SERVE_IDLE_SECONDS", IDLE.as_secs())?);
    let max_chats = usize::try_from(positive("MEDHA_SERVE_MAX_CHATS", 64)?)
        .context("MEDHA_SERVE_MAX_CHATS is too large")?;
    let home = config::medha_home()?;
    let directory = directory(&home);
    #[cfg(unix)]
    wire::private_folder(&directory)?;
    #[cfg(not(unix))]
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
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
    // A backend lives for days: its log is a file a day, and the last week of them is kept.
    let daily = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("serve")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&logs)
        .context("could not open the backend's log")?;
    let (log_writer, log_guard) = tracing_appender::non_blocking(daily);
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
    let backend = backend::Backend::with_chat_limit(chats, env!("CARGO_PKG_VERSION"), max_chats);
    // Written once the address is taken: a client that finds these is not refused.
    private(&directory.join("token"), &token)?;
    private(&directory.join("address"), &address)?;
    tracing::info!(address = %address, "medha backend listening");
    println!("medha backend listening");

    let clients = tokio_util::task::TaskTracker::new();
    let guests = Arc::new(tokio::sync::Semaphore::new(128));
    let serving = listener.serve({
        let backend = Arc::clone(&backend);
        let clients = clients.clone();
        move |stream| {
            if let Ok(guest) = Arc::clone(&guests).try_acquire_owned() {
                let serving = client(Arc::clone(&backend), stream, Arc::clone(&token));
                clients.spawn(async move {
                    let _guest = guest;
                    serving.await;
                });
            }
        }
    });
    let expiry = async {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            tick.tick().await;
            if backend.stop_if_idle(idle) {
                break;
            }
        }
    };
    let outcome = tokio::select! {
        served = serving => served.with_context(|| format!("could not listen on {address}")),
        () = asked_to_stop() => Ok(()),
        () = backend.stopping() => Ok(()),
        () = expiry => Ok(()),
    };
    backend.begin_stop();
    clients.close();
    for name in ["token", "address"] {
        let _ = std::fs::remove_file(directory.join(name));
    }
    #[cfg(unix)]
    let _ = std::fs::remove_file(&address);

    // Nothing new begins; every chat is told to finish, and what was already
    // begun is given a moment to.
    let finishing = async {
        while !(backend.chats().has_stopped() && backend.is_drained()) {
            backend.close_all();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        clients.wait().await;
    };
    let (finished, host_finished) =
        tokio::join!(tokio::time::timeout(STOP_GRACE, finishing), mcp_host.stop());
    if finished.is_err() || !host_finished {
        // Work waiting on a lock or on the keychain cannot be told to stop, and
        // may yet write. The lock is not given up while it could: it goes with
        // this process, so the backend that comes next never runs beside it.
        tracing::warn!("stopped with work that would not finish");
        drop(log_guard);
        if let Err(error) = &outcome {
            eprintln!("Error: {error:#}");
        }
        std::process::exit(i32::from(outcome.is_err()));
    }
    // Ownership lasts through root runtime teardown too: no replacement may
    // overlap a late blocking task not yet released by Tokio. The OS closes
    // this single descriptor when the serving process exits.
    std::mem::forget(lock);
    outcome
}
