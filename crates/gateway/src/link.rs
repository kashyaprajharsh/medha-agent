//! One platform chat's link to its Medha chat, as one open terminal window is: the chat itself
//! lives in the backend. The link follows it, relays replies, approvals and questions, and lets
//! go when idle.

use crate::attachments;
use crate::chats::Chats;
use crate::platform::{Incoming, Platform, split};
use crate::prompts::{Ask, Prompts, Step};
use crate::reply::Reply;
use medha_client::{Backend, Said, View, ViewLimits};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// A chat nobody has used for this long is let go; the next message resumes it.
const IDLE: Duration = Duration::from_secs(30 * 60);
/// Platforms drop a typing sign after about five seconds.
const TYPING: Duration = Duration::from_secs(4);
/// Platforms take files up to about this size from a bot.
const OUTPUT_BYTES: u64 = 50 * 1024 * 1024;

pub(crate) enum Input {
    Message(Incoming),
    Pressed(String),
    /// Follow a chat left running when the gateway last stopped, to deliver what it says.
    Resume,
}

pub(crate) struct Shared {
    pub platform: Arc<dyn Platform>,
    pub backend: Arc<Backend>,
    pub chats: Arc<Chats>,
    pub folder: PathBuf,
    /// The chat that gets notices, set with `/sethome`.
    pub home: std::sync::Mutex<Option<String>>,
}

pub(crate) struct ChatLink {
    shared: Arc<Shared>,
    key: String,
    chat: String,
    view: Option<View>,
    reply: Reply,
    prompts: Prompts,
    running: bool,
    last: Instant,
    /// Files the agent made in this turn, sent once it ends.
    outputs: Vec<PathBuf>,
}

impl ChatLink {
    pub fn new(shared: Arc<Shared>, key: String, chat: String) -> Self {
        Self {
            shared,
            key,
            chat,
            view: None,
            reply: Reply::default(),
            prompts: Prompts::default(),
            running: false,
            outputs: Vec::new(),
            last: Instant::now(),
        }
    }

    /// Ends when idle, so the gateway can forget it; a later message starts a new one.
    pub async fn run(mut self, mut inbox: mpsc::Receiver<Input>) {
        let mut typing = tokio::time::interval(TYPING);
        loop {
            let idle = self.last + IDLE;
            tokio::select! {
                input = inbox.recv() => match input {
                    Some(input) => { self.last = Instant::now(); self.input(input).await }
                    None => break,
                },
                said = next(&mut self.view) => self.heard(said).await,
                _ = typing.tick(), if self.running => { let _ = self.shared.platform.typing(&self.chat).await; }
                _ = tokio::time::sleep_until(idle), if !self.running && !self.prompts.waiting() => {
                    self.let_go().await;
                    break;
                }
            }
        }
    }

    async fn input(&mut self, input: Input) {
        match input {
            Input::Resume => {
                if self.view.is_none()
                    && let Err(error) = self.open().await
                {
                    tracing::warn!(%error, "gateway: following a running chat");
                }
            }
            Input::Pressed(choice) => match self.prompts.pressed(&choice) {
                Some(step) => self.step(step).await,
                None => self.say("That choice is no longer waiting.").await,
            },
            Input::Message(message) => {
                let text = message.text.trim();
                if let Some(command) = text.strip_prefix('/') {
                    let command = command.split(['@', ' ']).next().unwrap_or_default();
                    if self.command(command).await {
                        return;
                    }
                }
                if message.files.is_empty()
                    && let Some(step) = self.prompts.reply(text)
                {
                    return self.step(step).await;
                }
                self.send(message).await;
            }
        }
    }

    /// Whether `command` was one of the gateway's own; anything else goes to the model.
    async fn command(&mut self, command: &str) -> bool {
        match command {
            "start" | "help" => self.say(HELP).await,
            "new" if self.running => {
                self.say("A task is running. Stop it with /stop first.")
                    .await
            }
            "new" => {
                self.let_go().await;
                if let Err(error) = self.shared.chats.forget(&self.key) {
                    tracing::warn!(%error, "gateway: forgetting a chat");
                }
                self.say("Your next message starts a new conversation.")
                    .await;
            }
            "stop" => match &self.view {
                Some(view) if self.running => match view.call(&protocol::Cancel {}).await {
                    Ok(_) => self.say("Stopping.").await,
                    Err(error) => self.say(&format!("Could not stop: {error}")).await,
                },
                _ => self.say("Nothing is running.").await,
            },
            "status" => self.status().await,
            "sethome" => {
                let platform = self.shared.platform.name();
                match runtime::messaging::set_home(platform, &self.chat) {
                    Ok(()) => {
                        *self
                            .shared
                            .home
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(self.chat.clone());
                        self.say(
                            "This chat now gets Medha's notices, such as who asked to message it.",
                        )
                        .await;
                    }
                    Err(error) => {
                        self.say(&format!("Could not save this chat as home: {error:#}"))
                            .await
                    }
                }
            }
            _ => return false,
        }
        true
    }

    async fn status(&mut self) {
        let folder = self.shared.folder.display().to_string();
        let Some(view) = &self.view else {
            return self.say(&format!("Idle. Working in {folder}.")).await;
        };
        let text = match view.call(&protocol::GetPresentation {}).await {
            Ok(snapshot) => {
                let state = if snapshot.running { "Working" } else { "Idle" };
                match snapshot.settings {
                    Some(settings) => format!(
                        "{state}. Model {}, mode {:?}, in {folder}.",
                        settings.model, settings.mode
                    ),
                    None => format!("{state}. Working in {folder}."),
                }
            }
            Err(error) => format!("Could not read the chat: {error}"),
        };
        self.say(&text).await;
    }

    async fn send(&mut self, message: Incoming) {
        let received = attachments::receive(
            self.shared.platform.as_ref(),
            &self.shared.folder,
            &message.files,
        )
        .await;
        let mut content = message.text.trim().to_owned();
        for note in received.notes {
            content.push_str(&format!("\n{note}"));
        }
        if content.trim().is_empty() {
            if received.images.is_empty() {
                return;
            }
            content = runtime::attachments::IMAGE_ONLY_PROMPT.into();
        }
        if self.view.is_none()
            && let Err(error) = self.open().await
        {
            return self.say(&format!("Could not reach Medha: {error}")).await;
        }
        let Some(view) = &self.view else { return };
        self.reply.sent(&content);
        let message = protocol::SendMessage {
            content,
            images: received.images,
            intent: None,
            about: None,
        };
        match view.call(&message).await {
            Ok(accepted) if accepted.steered => self.say("Added to the task in progress.").await,
            Ok(_) => {}
            Err(error) => self.say(&format!("Not sent: {error}")).await,
        }
    }

    /// Follows the chat this platform chat continues, or starts it, in the gateway's folder.
    async fn open(&mut self) -> Result<(), String> {
        let resume = self
            .shared
            .chats
            .conversation(&self.key)
            .map_err(|error| error.to_string())?;
        let backend = Arc::clone(&self.shared.backend);
        let connection = tokio::task::spawn_blocking(move || backend.connection())
            .await
            .map_err(|_| "the backend connection stopped".to_owned())??;
        let folder = self.shared.folder.clone();
        let params = protocol::CreateSession {
            folder: folder.clone(),
            ends_with_client: false,
            resume,
            model: None,
            mode: None,
            reasoning: None,
            settings: None,
            startup: None,
        };
        let (view, _) = View::open(connection, params, ViewLimits::default(), |live| match live
            .about
            .as_ref()
        {
            Some(about) if about.folder != folder => Err(
                "This conversation works in another folder. Send /new to start one here.".into(),
            ),
            _ => Ok(()),
        })
        .await?;
        let snapshot = view.call(&protocol::GetPresentation {}).await?;
        if let Err(error) = self
            .shared
            .chats
            .remember(&self.key, &snapshot.conversation)
        {
            tracing::warn!(%error, "gateway: remembering a chat");
        }
        self.running = snapshot.running;
        self.view = Some(view);
        for prompt in &snapshot.approvals {
            self.approval(prompt).await;
        }
        for prompt in snapshot.questions {
            self.question(prompt).await;
        }
        Ok(())
    }

    async fn heard(&mut self, said: Option<Said>) {
        let frame = match said {
            Some(Said::Frame(frame) | Said::Event { frame, .. }) => frame,
            Some(Said::Ended(reason)) => return self.ended(reason).await,
            None => return self.ended(None).await,
        };
        let params = frame["params"].clone();
        match frame["method"].as_str() {
            Some("event") => {
                let Ok(event) = serde_json::from_value::<protocol::TurnEvent>(params) else {
                    return;
                };
                let settled = matches!(event, protocol::TurnEvent::Settled { .. });
                match &event {
                    protocol::TurnEvent::Started { .. } => self.running = true,
                    protocol::TurnEvent::Settled { .. } => self.running = false,
                    protocol::TurnEvent::Outputs { revisions } => {
                        self.outputs
                            .extend(revisions.iter().filter_map(output_file));
                    }
                    _ => {}
                }
                for message in self.reply.event(event) {
                    self.say(&message).await;
                }
                if settled {
                    self.send_outputs().await;
                }
            }
            Some("approval") => {
                if let Ok(prompt) = serde_json::from_value(params) {
                    self.approval(&prompt).await;
                }
            }
            Some("approval.resolved") => {
                if let Some(gate) = params["gate_id"].as_u64()
                    && self.prompts.resolved(gate)
                {
                    let how = if params["approved"] == true {
                        "approved"
                    } else {
                        "denied"
                    };
                    self.say(&format!("That request was {how} on another screen."))
                        .await;
                }
            }
            Some("question") => {
                if let Ok(prompt) = serde_json::from_value(params) {
                    self.question(prompt).await;
                }
            }
            Some("question.answered") => {
                if let Some(id) = params["question_id"].as_u64() {
                    self.prompts.question_answered(id);
                }
            }
            _ => {}
        }
    }

    async fn approval(&mut self, prompt: &protocol::ApprovalPrompt) {
        if let Some(ask) = self.prompts.approval(prompt) {
            self.flush().await;
            self.ask(ask).await;
        }
    }

    async fn question(&mut self, prompt: protocol::QuestionPrompt) {
        if let Some(ask) = self.prompts.question(prompt) {
            self.flush().await;
            self.ask(ask).await;
        }
    }

    async fn step(&mut self, step: Step) {
        let Some(view) = &self.view else {
            return self.say("That choice is no longer waiting.").await;
        };
        let sent = match step {
            Step::Ask(ask) => return self.ask(ask).await,
            Step::Approve { gate, decision } => view
                .call(&protocol::AnswerApproval {
                    gate_id: gate,
                    decision,
                })
                .await
                .map(|_| ()),
            Step::Answer(answer) => view.call(&answer).await.map(|_| ()),
        };
        if let Err(error) = sent {
            self.say(&format!("The answer was not taken: {error}"))
                .await;
        }
    }

    async fn ended(&mut self, reason: Option<String>) {
        self.flush().await;
        self.view = None;
        self.prompts = Prompts::default();
        if std::mem::take(&mut self.running) {
            let reason = reason.unwrap_or_else(|| "the chat ended".into());
            self.say(&format!(
                "The task stopped: {reason}. Send a message to continue."
            ))
            .await;
        }
    }

    /// Closes a chat only this gateway follows; one open on another screen is left running.
    async fn let_go(&mut self) {
        let Some(view) = self.view.take() else { return };
        let session = view.chat().id().to_owned();
        let watched_elsewhere = view
            .connection()
            .call_async(protocol::Scope::Service, &protocol::ListSessions {})
            .await
            .is_ok_and(|listed| {
                listed
                    .sessions
                    .iter()
                    .any(|live| live.session == session && live.clients > 1)
            });
        if !watched_elsewhere {
            let _ = view.call(&protocol::Close {}).await;
        }
        self.prompts = Prompts::default();
    }

    /// Only files inside the gateway's folder are sent, so a reply cannot mail out other files.
    async fn send_outputs(&mut self) {
        let mut sent = Vec::new();
        for path in std::mem::take(&mut self.outputs) {
            let Ok(path) = path.canonicalize() else {
                continue;
            };
            let small = std::fs::metadata(&path)
                .is_ok_and(|meta| meta.is_file() && meta.len() <= OUTPUT_BYTES);
            if !path.starts_with(&self.shared.folder) || !small || sent.contains(&path) {
                continue;
            }
            if let Err(error) = self.shared.platform.send_file(&self.chat, &path).await {
                tracing::warn!(error = %format!("{error:#}"), "gateway: sending a file");
                self.say(&format!(
                    "{} is ready on the computer but could not be sent.",
                    path.display()
                ))
                .await;
            }
            sent.push(path);
        }
    }

    async fn flush(&mut self) {
        for message in self.reply.flush() {
            self.say(&message).await;
        }
    }

    async fn ask(&self, ask: Ask) {
        let platform = &self.shared.platform;
        let mut parts = split(&ask.text, platform.limit());
        let last = parts.pop().unwrap_or_default();
        for part in parts {
            self.send_text(&part).await;
        }
        if let Err(error) = platform.ask(&self.chat, &last, &ask.buttons).await {
            tracing::warn!(error = %format!("{error:#}"), "gateway: offering choices");
        }
    }

    async fn say(&self, text: &str) {
        for part in split(text, self.shared.platform.limit()) {
            self.send_text(&part).await;
        }
    }

    async fn send_text(&self, text: &str) {
        if let Err(error) = self.shared.platform.send(&self.chat, text).await {
            tracing::warn!(error = %format!("{error:#}"), "gateway: sending a message");
        }
    }
}

/// A file output, by where it lies; an inline output such as a diagram has no file.
fn output_file(revision: &protocol::ArtifactRevision) -> Option<PathBuf> {
    if revision.inline {
        return None;
    }
    let path = revision
        .path
        .as_ref()
        .or(revision.snapshot.as_ref().map(|snapshot| &snapshot.path))?;
    Some(PathBuf::from(path))
}

async fn next(view: &mut Option<View>) -> Option<Said> {
    match view {
        Some(view) => view.recv().await,
        None => std::future::pending().await,
    }
}

const HELP: &str = "This chat talks to Medha on your computer. Write what you want done.\n\
/stop stops the running task\n/new starts a new conversation\n/status shows what Medha is doing\n\
/sethome sends Medha's notices to this chat";
