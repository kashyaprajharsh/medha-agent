//! One platform chat's link to its Medha chat, as one open terminal window is: the chat itself
//! lives in the backend. The link follows it, relays replies, approvals and questions, and lets
//! go when idle.

use crate::attachments;
use crate::chats::Chats;
use crate::platform::{Incoming, Platform, split};
use crate::prompts::{Prompts, Step};
use crate::reply::Reply;
use medha_client::{Backend, Said, View, ViewLimits};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

#[path = "link/commands.rs"]
mod commands;
#[path = "link/shown.rs"]
mod shown;

use shown::{Asked, Live};

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
    /// The default folder as the gateway started with it; `settings` is asked first.
    pub folder: PathBuf,
    pub settings: Settings,
    /// The chat that gets notices, set with `/sethome`.
    pub home: std::sync::Mutex<Option<String>>,
}

/// Where chats may work and what they show.
pub enum Settings {
    /// Medha's saved settings, read on each use, so a change on the computer applies at once.
    Saved,
    /// Given once, the default folder first, for a gateway run apart from Medha's settings.
    Fixed { folders: Vec<PathBuf>, steps: bool },
}

impl Settings {
    fn saved() -> runtime::messaging::Messaging {
        runtime::config::load()
            .ok()
            .flatten()
            .unwrap_or_default()
            .gateway
    }

    /// The default, then every folder a chat may pick.
    pub(crate) fn folders(&self) -> (Option<PathBuf>, Vec<PathBuf>) {
        match self {
            Settings::Saved => {
                let saved = Self::saved();
                (saved.folder.clone(), saved.allowed())
            }
            Settings::Fixed { folders, .. } => (folders.first().cloned(), folders.clone()),
        }
    }

    /// Whether tool steps are shown in the chat.
    pub(crate) fn steps(&self) -> bool {
        match self {
            Settings::Saved => Self::saved().steps,
            Settings::Fixed { steps, .. } => *steps,
        }
    }
}

pub(crate) struct ChatLink {
    shared: Arc<Shared>,
    key: String,
    chat: String,
    view: Option<View>,
    /// The folder the open conversation works in; files go in and out only here.
    folder: PathBuf,
    reply: Reply,
    prompts: Prompts,
    running: bool,
    last: Instant,
    /// Files the agent made in this turn, sent once it ends.
    outputs: Vec<PathBuf>,
    /// What each command's buttons last offered, label and value, so a press names one of them.
    offered: std::collections::HashMap<&'static str, Vec<(String, String)>>,
    live: Live,
}

impl ChatLink {
    pub fn new(shared: Arc<Shared>, key: String, chat: String) -> Self {
        Self {
            folder: shared.folder.clone(),
            shared,
            key,
            chat,
            view: None,
            reply: Reply::default(),
            prompts: Prompts::default(),
            running: false,
            outputs: Vec::new(),
            offered: std::collections::HashMap::new(),
            live: Live::default(),
            last: Instant::now(),
        }
    }

    /// Ends when idle, so the gateway can forget it; a later message starts a new one.
    pub async fn run(mut self, mut inbox: mpsc::Receiver<Input>) {
        let mut typing = tokio::time::interval(TYPING);
        loop {
            let idle = self.last + IDLE;
            let steps_due = self.live.due;
            tokio::select! {
                input = inbox.recv() => match input {
                    Some(input) => { self.last = Instant::now(); self.input(input).await }
                    None => break,
                },
                said = next(&mut self.view) => self.heard(said).await,
                _ = typing.tick(), if self.running => { let _ = self.shared.platform.typing(&self.chat).await; }
                _ = tokio::time::sleep_until(steps_due.unwrap_or(idle)), if steps_due.is_some() => self.draw_steps().await,
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
            Input::Pressed(choice) if choice.starts_with("c.") => self.chosen(&choice).await,
            Input::Pressed(choice) => match self.prompts.pressed(&choice) {
                Some(step) => self.step(step).await,
                None => self.say("That choice is no longer waiting.").await,
            },
            Input::Message(mut message) => {
                let text = message.text.trim().to_owned();
                if let Some(typed) = text.strip_prefix('/') {
                    let (command, args) =
                        typed.split_once(char::is_whitespace).unwrap_or((typed, ""));
                    // Groups address a bot as `/new@SomeBot`.
                    let command = command.split('@').next().unwrap_or_default();
                    if self.command(command, args.trim()).await {
                        return;
                    }
                    // A plugin's command expands into its prompt, as at the terminal.
                    match self.expand(&text).await {
                        Some(prompt) => message.text = prompt,
                        None => {
                            return self
                                .say(&format!(
                                    "There is no /{command} here. Send /help to see what there is."
                                ))
                                .await;
                        }
                    }
                }
                if message.files.is_empty()
                    && let Some(step) = self.prompts.reply(&text)
                {
                    return self.step(step).await;
                }
                self.send(message).await;
            }
        }
    }

    /// Whether `command` was one of the chat's own; anything else goes to the model.
    async fn command(&mut self, command: &str, args: &str) -> bool {
        match command {
            "start" | "help" => self.say(&help()).await,
            "new" if self.running => {
                self.say("A task is running. Stop it with /stop first.")
                    .await
            }
            "new" => self.fresh(args).await,
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
            _ => return self.more(command, args).await,
        }
        true
    }

    /// The prompt a plugin command stands for, or `None` when `typed` is no command the backend knows.
    async fn expand(&mut self, typed: &str) -> Option<String> {
        if self.view.is_none() {
            self.open().await.ok()?;
        }
        let view = self.view.as_ref()?;
        let expanded = view
            .call(&protocol::ExpandCommand {
                typed: typed.to_owned(),
            })
            .await
            .ok()?;
        (expanded.prompt != typed).then_some(expanded.prompt)
    }

    async fn status(&mut self) {
        let folder = match &self.view {
            Some(_) => self.folder.display().to_string(),
            None => self.place().0.display().to_string(),
        };
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
        // Opened first, so files are saved in the folder this conversation works in.
        if self.view.is_none()
            && let Err(error) = self.open().await
        {
            return self.say(&format!("Could not reach Medha: {error}")).await;
        }
        let received =
            attachments::receive(self.shared.platform.as_ref(), &self.folder, &message.files).await;
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
    /// Joining may start the backend, which blocks, so it runs off the async threads.
    async fn connection(&self) -> Result<Arc<medha_client::Connection>, String> {
        let backend = Arc::clone(&self.shared.backend);
        tokio::task::spawn_blocking(move || backend.connection())
            .await
            .map_err(|_| "the backend connection stopped".to_owned())?
    }

    /// The folder this chat works in now: the project it chose at `/new` while still allowed,
    /// else the default. Read from the saved settings each time, so a folder removed on the
    /// computer is refused at once; the note says when a choice was dropped.
    fn place(&self) -> (PathBuf, Option<String>) {
        let (default, allowed) = self.shared.settings.folders();
        let default = default
            .filter(|folder| folder.is_dir())
            .unwrap_or_else(|| self.shared.folder.clone());
        match self.shared.chats.folder(&self.key) {
            Ok(Some(chosen)) if chosen.is_dir() && allowed.contains(&chosen) => (chosen, None),
            Ok(Some(chosen)) => {
                let note = format!(
                    "{} is no longer open to messages, so this chat works in {} again.",
                    chosen.display(),
                    default.display()
                );
                (default, Some(note))
            }
            _ => (default, None),
        }
    }

    async fn open(&mut self) -> Result<(), String> {
        let (folder, dropped) = self.place();
        if let Some(note) = dropped {
            // The old conversation was in the folder that was removed, so it is not continued.
            if let Err(error) = self.shared.chats.choose_folder(&self.key, &folder) {
                tracing::warn!(%error, "gateway: dropping a removed folder");
            }
            self.say(&note).await;
        }
        let resume = self
            .shared
            .chats
            .conversation(&self.key)
            .map_err(|error| error.to_string())?;
        let connection = self.connection().await?;
        let params = protocol::CreateSession {
            folder: folder.clone(),
            ends_with_client: false,
            resume,
            model: None,
            mode: None,
            reasoning: None,
            settings: None,
            startup: None,
            // The person is reached on their phone, not at a screen.
            answer: Some(protocol::Answerer::Notify),
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
        self.folder = folder;
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
                // Words said because of a step come before it, so it is noted after them.
                let step = matches!(
                    event,
                    protocol::TurnEvent::Started { .. }
                        | protocol::TurnEvent::ToolCall { .. }
                        | protocol::TurnEvent::ToolResult { .. }
                )
                .then(|| event.clone());
                let messages = self.reply.event(event);
                self.say_all(messages).await;
                if let Some(step) = step {
                    self.note_step(&step);
                }
                if settled {
                    self.close_steps().await;
                    self.send_outputs().await;
                }
            }
            Some("approval") => {
                if let Ok(prompt) = serde_json::from_value(params) {
                    self.approval(&prompt).await;
                }
            }
            Some("approval.resolved") => {
                if let Ok(resolved) = serde_json::from_value(params) {
                    self.approval_resolved(resolved).await;
                }
            }
            Some("question") => {
                if let Ok(prompt) = serde_json::from_value(params) {
                    self.question(prompt).await;
                }
            }
            Some("question.answered") => {
                if let Some(id) = params["question_id"].as_u64()
                    && self.prompts.question_answered(id)
                {
                    self.settle(
                        Asked::Question(id),
                        "No longer waiting: answered on another screen or timed out.",
                    )
                    .await;
                }
            }
            _ => {}
        }
    }

    async fn approval(&mut self, prompt: &protocol::ApprovalPrompt) {
        if let Some(ask) = self.prompts.approval(prompt) {
            self.flush().await;
            self.steps_now().await;
            self.ask(ask, Some(Asked::Approval(prompt.gate_id))).await;
        }
    }

    /// The request stays in the chat, its buttons gone and how it ended written under it.
    async fn approval_resolved(&mut self, resolved: protocol::ApprovalResolved) {
        use crate::prompts::Settled;
        use protocol::Resolution;
        let Some(settled) = self.prompts.resolved(resolved.gate_id) else {
            return;
        };
        let mark = if resolved.approved { "✓" } else { "✗" };
        let outcome = match (resolved.how, &settled) {
            (Resolution::TimedOut, _) => "✗ Denied: nobody answered in time.".to_owned(),
            (Resolution::Stopped, _) => "Not answered: the task stopped.".to_owned(),
            (Resolution::Answered, Settled::Here(label)) => format!("{mark} {label}"),
            (Resolution::Answered, Settled::Elsewhere) if resolved.approved => {
                "✓ Allowed on another screen.".to_owned()
            }
            (Resolution::Answered, Settled::Elsewhere) => "✗ Denied on another screen.".to_owned(),
        };
        let changed = self
            .settle(Asked::Approval(resolved.gate_id), &outcome)
            .await;
        // Where a message cannot change, only news from elsewhere is said: a time limit and a
        // stop are told by the chat already.
        if !changed && settled == Settled::Elsewhere && resolved.how == Resolution::Answered {
            self.say(&outcome).await;
        }
    }

    async fn question(&mut self, prompt: protocol::QuestionPrompt) {
        let id = prompt.question_id;
        if let Some(ask) = self.prompts.question(prompt) {
            self.flush().await;
            self.steps_now().await;
            self.ask(ask, Some(Asked::Question(id))).await;
        }
    }

    async fn step(&mut self, step: Step) {
        let Some(view) = &self.view else {
            return self.say("That choice is no longer waiting.").await;
        };
        let sent = match step {
            Step::Ask {
                question,
                said,
                ask,
            } => {
                self.settle(Asked::Question(question), &answered(&said))
                    .await;
                return self.ask(ask, Some(Asked::Question(question))).await;
            }
            Step::Approve { gate, decision } => view
                .call(&protocol::AnswerApproval {
                    gate_id: gate,
                    decision,
                })
                .await
                .map(|_| ()),
            Step::Answer { answer, said } => {
                let sent = view.call(&answer).await.map(|_| ());
                if sent.is_ok() {
                    self.settle(Asked::Question(answer.question_id), &answered(&said))
                        .await;
                }
                sent
            }
        };
        if let Err(error) = sent {
            self.say(&format!("The answer was not taken: {error}"))
                .await;
        }
    }

    async fn ended(&mut self, reason: Option<String>) {
        self.flush().await;
        self.close_steps().await;
        self.view = None;
        self.prompts = Prompts::default();
        self.settle_waiting("No longer waiting: the chat ended.")
            .await;
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
        self.settle_waiting("No longer waiting: this chat moved on.")
            .await;
    }

    /// Only files inside this chat's folder are sent, so a reply cannot mail out other files.
    async fn send_outputs(&mut self) {
        let mut sent = Vec::new();
        for path in std::mem::take(&mut self.outputs) {
            let Ok(path) = path.canonicalize() else {
                continue;
            };
            let small = std::fs::metadata(&path)
                .is_ok_and(|meta| meta.is_file() && meta.len() <= OUTPUT_BYTES);
            if !path.starts_with(&self.folder) || !small || sent.contains(&path) {
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
        let messages = self.reply.flush();
        self.say_all(messages).await;
    }

    async fn say_all(&mut self, messages: Vec<String>) {
        if messages.is_empty() {
            return;
        }
        self.close_steps().await;
        for message in messages {
            self.say(&message).await;
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

fn answered(said: &str) -> String {
    format!(
        "Answer: {}",
        crate::telegram_format::literal(&transcript_view::clip(said, 200))
    )
}

fn help() -> String {
    let mut text =
        "This chat talks to Medha on your computer. Write what you want done.".to_owned();
    for (name, description) in protocol::commands::CHAT_COMMANDS {
        text.push_str(&format!("\n{name}: {description}"));
    }
    text
}
