//! How the backend starts a chat: the same chat `medha --acp` runs, on a
//! channel of its own in place of the process's stdin and stdout.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use backend::{Chats, Opened};
use runtime::session::{Choices, Start};
use runtime::{Notices, Resume, SessionOptions, Workspace};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

/// Start-up notices have no terminal to go to; the client that asked gets them.
#[derive(Default)]
struct Collected(Mutex<Vec<String>>);

impl Notices for Collected {
    fn say(&self, line: &str) {
        self.lines().push(line.to_string());
    }
}

impl Collected {
    fn lines(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// How long a folder nobody holds stays open: a window that keeps asking about
/// it does not have its whole log checked again each time.
const IDLE: Duration = Duration::from_secs(5 * 60);
/// How many folders nobody holds stay open at most, however lately they were used.
const UNHELD: usize = 16;
const SWEEP: Duration = Duration::from_secs(60);

/// What is open per folder. A folder is held by whoever was handed it, a chat
/// for as long as it runs; once nobody holds it and `idle` has passed it closes,
/// so a backend that lives for days keeps only the folders in use.
struct Kept<T> {
    open: HashMap<PathBuf, (Arc<T>, Instant)>,
    idle: Duration,
}

impl<T> Kept<T> {
    fn new(idle: Duration) -> Self {
        Self {
            open: HashMap::new(),
            idle,
        }
    }

    /// Closes what nobody holds and nobody has used lately, and all but the
    /// most recently used few of what nobody holds. Run on every use and on a
    /// timer, so a backend with nothing to do still lets go.
    fn sweep(&mut self, now: Instant) {
        let idle = self.idle;
        self.open.retain(|_, (held, used)| {
            let in_use = Arc::strong_count(held) > 1;
            if in_use {
                *used = now;
            }
            in_use || now.duration_since(*used) < idle
        });
        let mut unheld: Vec<(Instant, PathBuf)> = self
            .open
            .iter()
            .filter(|(_, (held, _))| Arc::strong_count(held) == 1)
            .map(|(folder, (_, used))| (*used, folder.clone()))
            .collect();
        unheld.sort();
        for (_, folder) in unheld.iter().rev().skip(UNHELD) {
            self.open.remove(folder);
        }
    }

    fn get(
        &mut self,
        folder: &Path,
        open: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<Arc<T>> {
        let now = Instant::now();
        self.sweep(now);
        if let Some((held, used)) = self.open.get_mut(folder) {
            *used = now;
            return Ok(Arc::clone(held));
        }
        let opened = Arc::new(open()?);
        self.open
            .insert(folder.to_path_buf(), (Arc::clone(&opened), now));
        Ok(opened)
    }
}

/// A folder's chats, and what is asked about the folder, share its one opened
/// workspace and so its one event log.
pub(crate) struct ServeChats {
    folders: Arc<Mutex<Kept<Workspace>>>,
    mcp_host: mcp::hub::Endpoint,
    mcp_changed: Arc<tokio::sync::Notify>,
    requests: Lane,
    starts: Lane,
}

const BUSY: &str = "Medha is busy with other requests. Try again in a moment.";
/// How many folder requests are answered at once, and how many chats start at once.
const ANSWERING: usize = 8;
const STARTING: usize = 4;

/// Work that may wait on a lock, the keychain or the disk runs on threads of
/// its own, a few at a time, so it never takes the threads every chat and
/// client is served on. A bounded number wait their turn; past that a request
/// is refused at once, not left to pile up.
struct Lane {
    running: Arc<tokio::sync::Semaphore>,
    waiting: std::sync::atomic::AtomicUsize,
    room: usize,
}

impl Lane {
    fn new(at_once: usize, room: usize) -> Self {
        Self {
            running: Arc::new(tokio::sync::Semaphore::new(at_once)),
            waiting: std::sync::atomic::AtomicUsize::new(0),
            room,
        }
    }

    async fn admit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
        use std::sync::atomic::Ordering;
        if self.waiting.fetch_add(1, Ordering::Relaxed) >= self.room {
            self.waiting.fetch_sub(1, Ordering::Relaxed);
            return Err(BUSY.into());
        }
        let admitted = Arc::clone(&self.running).acquire_owned().await;
        self.waiting.fetch_sub(1, Ordering::Relaxed);
        admitted.map_err(|_| BUSY.to_string())
    }
}

/// Chats run on threads of their own. A chat reads keys, config and files as
/// it works, and any of those may wait on a lock or the keychain; that wait
/// must never take a thread a client is served on. There are more of these
/// threads than chats that may be starting at once, so chats waiting to start
/// do not hold up the chats that are running.
fn chats() -> &'static tokio::runtime::Runtime {
    static CHATS: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    CHATS.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(cores.max(2 * STARTING))
            .thread_name("medha-chat")
            .enable_all()
            .build()
            .expect("threads for chats to run on")
    })
}

/// Runs what may block away from the threads that serve everyone.
async fn apart<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| "the request stopped unexpectedly".to_string())?
}

fn opened(
    folders: &Mutex<Kept<Workspace>>,
    folder: &Path,
    notices: &dyn Notices,
) -> Result<Arc<Workspace>, String> {
    folders
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(folder, || Workspace::open(folder.to_path_buf(), notices))
        .map_err(|error| format!("{error:#}"))
}

impl ServeChats {
    pub(crate) fn new(mcp_host: mcp::hub::Endpoint, mcp_changed: Arc<tokio::sync::Notify>) -> Self {
        let folders = Arc::new(Mutex::new(Kept::new(IDLE)));
        let swept = Arc::downgrade(&folders);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP);
            while let Some(folders) = swept.upgrade() {
                tick.tick().await;
                folders
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .sweep(Instant::now());
            }
        });
        Self {
            folders,
            mcp_host,
            mcp_changed,
            requests: Lane::new(ANSWERING, 64),
            starts: Lane::new(STARTING, 32),
        }
    }
}

/// What a client may choose for a chat, laid over what the backend's own
/// environment asks of every chat, as a flag is laid over it in a terminal.
/// Keys are never a client's to choose: the backend reads those from its
/// environment and the saved configuration.
fn options(params: &Value) -> Result<SessionOptions, String> {
    let process = SessionOptions::from_process().map_err(|error| format!("{error:#}"))?;
    let text = |key: &str| params[key].as_str().map(str::to_owned);
    let autonomy = text("mode")
        .map(|mode| kernel::AutonomyLevel::parse(&mode))
        .transpose()?;
    let reasoning = text("reasoning")
        .map(|effort| kernel::ReasoningConfig::from_effort_text(&effort))
        .transpose()?;
    Ok(SessionOptions {
        model: text("model"),
        reasoning: reasoning.or_else(|| process.reasoning.clone()),
        autonomy: autonomy.or(process.autonomy),
        resume: text("resume").map_or(Resume::None, Resume::Id),
        ..process
    })
}

fn folder(named: &Value) -> Result<PathBuf, String> {
    let named = named.as_str().ok_or("a folder must be named")?;
    let path = Path::new(named);
    if !path.is_absolute() {
        return Err("the folder must be an absolute path".into());
    }
    path.canonicalize()
        .map_err(|error| format!("the folder cannot be opened: {error}"))
}

#[async_trait::async_trait]
impl Chats for ServeChats {
    async fn about_folder(&self, request: &Value) -> Result<Value, String> {
        let _admitted = self.requests.admit().await?;
        let (folders, asked) = (Arc::clone(&self.folders), request.clone());
        let serving = tokio::runtime::Handle::current();
        let answer = apart(move || {
            let folder = folder(&asked["folder"])?;
            let workspace = opened(&folders, &folder, &Collected::default())?;
            let store = workspace
                .open_store()
                .map_err(|error| format!("{error:#}"))?;
            serving.block_on(crate::desktop_service::answer(&store.log, &folder, &asked))
        })
        .await;
        // A saved key leaves the config file as it was, so the host is told rather than left to notice.
        let shared = request["method"].as_str().is_some_and(|method| {
            method.starts_with("settings.mcp.") || method.starts_with("settings.keys.")
        });
        if shared && answer.is_ok() {
            self.mcp_changed.notify_one();
        }
        answer
    }

    async fn open(&self, params: &Value) -> Result<Opened, String> {
        // Held until the chat is ready, so only a few chats start at once.
        let _admitted = self.starts.admit().await?;
        let restore = params
            .get("settings")
            .filter(|saved| saved.is_object())
            .cloned();
        let notices = Arc::new(Collected::default());
        let (folders, asked, host) = (
            Arc::clone(&self.folders),
            params.clone(),
            self.mcp_host.clone(),
        );
        let noted = Arc::clone(&notices);
        let (folder, options, workspace, lock, choices) = apart(move || {
            let failed = |error: anyhow::Error| format!("{error:#}");
            let folder = folder(&asked["folder"])?;
            let mut options = options(&asked)?;
            options.mcp_host = Some(host);
            let workspace = opened(&folders, &folder, noted.as_ref())?;
            let lock = runtime::workspace::load_lock(&folder, noted.as_ref()).map_err(failed)?;
            let choices = Choices::of(&lock, &options).map_err(failed)?;
            Ok((folder, options, workspace, lock, choices))
        })
        .await?;

        let (to_chat, chat_input) = tokio::io::duplex(64 * 1024);
        let (chat_output, from_chat) = tokio::io::duplex(256 * 1024);
        let mut chat = chats().spawn({
            let notices = Arc::clone(&notices);
            async move {
                let model = runtime::model::resolve(&lock, &options, notices.as_ref()).await?;
                let start = Start {
                    lock: &lock,
                    options: &options,
                    workspace: &workspace,
                    model,
                    autonomy: choices.autonomy,
                    verify_command: choices.verify_command,
                    verify_required: choices.verify_required,
                    verify_timeout: choices.verify_timeout,
                    notices: notices.as_ref(),
                };
                crate::acp_chat::run(start, chat_input, chat_output, restore).await
            }
        });
        let ended = |outcome: Result<anyhow::Result<()>, tokio::task::JoinError>| match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(format!("{error:#}")),
            Err(_) => Err("the chat stopped unexpectedly".to_string()),
        };

        // A chat is open once it says it is ready; what it said until then is kept for its clients.
        let mut output = BufReader::new(from_chat);
        let mut said = Vec::new();
        let ready = loop {
            let mut line = Vec::new();
            let read = tokio::select! {
                read = output.read_until(b'\n', &mut line) => read.unwrap_or(0),
                outcome = &mut chat => {
                    let reason = ended(outcome).err().unwrap_or_else(|| "the chat ended before it was ready".into());
                    return Err(reason);
                }
            };
            if read == 0 {
                let reason = ended(chat.await)
                    .err()
                    .unwrap_or_else(|| "the chat ended before it was ready".into());
                return Err(reason);
            }
            said.extend_from_slice(&line);
            match serde_json::from_slice::<Value>(&line) {
                Ok(frame) if frame["method"] == "ready" => break frame,
                _ => {}
            }
        };
        let session = ready["params"]["session"]
            .as_str()
            .ok_or("the chat did not name its session")?
            .to_string();
        let about = json!({
            "folder": folder,
            "model": ready["params"]["model"],
            "notices": notices.lines().clone(),
        });
        Ok(Opened {
            session,
            about,
            input: Box::new(to_chat),
            output: Box::new(std::io::Cursor::new(said).chain(output)),
            done: Box::pin(async move { ended(chat.await) }),
        })
    }
}

#[cfg(test)]
#[path = "serve_chats_tests.rs"]
mod tests;
