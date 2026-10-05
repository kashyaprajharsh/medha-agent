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

use crate::config;

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

    fn get(
        &mut self,
        folder: &Path,
        open: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<Arc<T>> {
        let now = Instant::now();
        let idle = self.idle;
        self.open.retain(|_, (held, used)| {
            let in_use = Arc::strong_count(held) > 1;
            if in_use {
                *used = now;
            }
            in_use || now.duration_since(*used) < idle
        });
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
    folders: Mutex<Kept<Workspace>>,
    mcp_host: mcp::hub::Endpoint,
}

impl ServeChats {
    pub(crate) fn new(mcp_host: mcp::hub::Endpoint) -> Self {
        Self {
            folders: Mutex::new(Kept::new(IDLE)),
            mcp_host,
        }
    }

    fn workspace(&self, folder: &Path, notices: &dyn Notices) -> anyhow::Result<Arc<Workspace>> {
        self.folders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(folder, || Workspace::open(folder.to_path_buf(), notices))
    }
}

/// What a client may choose for a chat. Keys are never among them: the backend
/// reads those from its own environment and the saved configuration.
fn options(params: &Value) -> Result<SessionOptions, String> {
    let text = |key: &str| params[key].as_str().map(str::to_owned);
    let autonomy = text("mode")
        .map(|mode| kernel::AutonomyLevel::parse(&mode))
        .transpose()?;
    let reasoning = text("reasoning")
        .map(|effort| kernel::ReasoningConfig::from_effort_text(&effort))
        .transpose()?;
    Ok(SessionOptions {
        model: text("model"),
        reasoning,
        autonomy,
        resume: text("resume").map_or(Resume::None, Resume::Id),
        model_env: config::ModelEnv::from_process(),
        search_env: config::SearchEnv::from_process(),
        budget: runtime::budget::BudgetLimits::from_env().map_err(|error| format!("{error:#}"))?,
        ..Default::default()
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
        let folder = folder(&request["folder"])?;
        let failed = |error: anyhow::Error| format!("{error:#}");
        let workspace = self
            .workspace(&folder, &Collected::default())
            .map_err(failed)?;
        let store = workspace.open_store().map_err(failed)?;
        crate::desktop_service::answer(&store.log, &folder, request).await
    }

    async fn open(&self, params: &Value) -> Result<Opened, String> {
        let folder = folder(&params["folder"])?;
        let mut options = options(params)?;
        options.mcp_host = Some(self.mcp_host.clone());
        let restore = params
            .get("settings")
            .filter(|saved| saved.is_object())
            .cloned();
        let notices = Arc::new(Collected::default());
        let workspace = self
            .workspace(&folder, notices.as_ref())
            .map_err(|error| format!("{error:#}"))?;

        let (to_chat, chat_input) = tokio::io::duplex(64 * 1024);
        let (chat_output, from_chat) = tokio::io::duplex(256 * 1024);
        let mut chat = tokio::spawn({
            let (notices, folder) = (Arc::clone(&notices), folder.clone());
            async move {
                let lock = runtime::workspace::load_lock(&folder, notices.as_ref())?;
                let choices = Choices::of(&lock, &options)?;
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
