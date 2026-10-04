//! How the backend starts a chat: the same chat `medha --acp` runs, on a
//! channel of its own in place of the process's stdin and stdout.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

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

/// A folder's chats share its one opened workspace, and so its one event log.
#[derive(Default)]
pub(crate) struct ServeChats {
    folders: Mutex<HashMap<PathBuf, Weak<Workspace>>>,
}

impl ServeChats {
    fn workspace(&self, folder: &Path, notices: &dyn Notices) -> anyhow::Result<Arc<Workspace>> {
        let mut folders = self
            .folders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        folders.retain(|_, held| held.strong_count() > 0);
        if let Some(open) = folders.get(folder).and_then(Weak::upgrade) {
            return Ok(open);
        }
        let opened = Arc::new(Workspace::open(folder.to_path_buf(), notices)?);
        folders.insert(folder.to_path_buf(), Arc::downgrade(&opened));
        Ok(opened)
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

fn folder(params: &Value) -> Result<PathBuf, String> {
    let named = params["folder"]
        .as_str()
        .ok_or("session.create needs a folder")?;
    let path = Path::new(named);
    if !path.is_absolute() {
        return Err("the folder must be an absolute path".into());
    }
    path.canonicalize()
        .map_err(|error| format!("the folder cannot be opened: {error}"))
}

#[async_trait::async_trait]
impl Chats for ServeChats {
    async fn open(&self, params: &Value) -> Result<Opened, String> {
        let folder = folder(params)?;
        let options = options(params)?;
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
