//! The phone's commands beyond the basics: mode, model, usage, going back to an earlier chat,
//! starting over in another project, skills and memories. Each asks the backend with the same
//! command the terminal uses.

use super::{Asked, ChatLink};
use crate::platform::Button;
use crate::telegram_format::literal;
use protocol::{Mode, ResourceResult, SessionRead, SessionResult};

const MODES: [(&str, Mode, &str); 4] = [
    ("plan", Mode::Plan, "reads and plans, changes nothing"),
    (
        "careful",
        Mode::Careful,
        "asks before every edit and command",
    ),
    ("normal", Mode::Normal, "edits run, commands ask"),
    (
        "autopilot",
        Mode::Autopilot,
        "everything inside the folder runs; a reviewer may allow network or outside folders one action at a time",
    ),
];
/// Choices offered at once; a phone screen holds about this many buttons.
const OFFERED: usize = 8;
const LABEL_CHARS: usize = 40;

impl ChatLink {
    /// Whether `command` is one of these; `args` is what followed it.
    pub(super) async fn more(&mut self, command: &str, args: &str) -> bool {
        match command {
            "mode" => self.mode(args).await,
            "model" => self.model(args).await,
            "usage" => self.usage().await,
            "resume" => self.recent().await,
            "skill" => self.skill(args).await,
            "memory" => self.memories().await,
            _ => return false,
        }
        true
    }

    /// A button from one of these commands, `c.<command>.<number>`, among what was last offered.
    pub(super) async fn chosen(&mut self, choice: &str) {
        let mut parts = choice.splitn(3, '.');
        let (Some("c"), Some(command), Some(index)) = (parts.next(), parts.next(), parts.next())
        else {
            return self.say("That choice is no longer waiting.").await;
        };
        let offered = index.parse::<usize>().ok().and_then(|index| {
            let (command, offered) = self.offered.get_key_value(command)?;
            Some((*command, offered.get(index)?.clone()))
        });
        let Some((command, (label, value))) = offered else {
            return self.say("That choice is no longer waiting.").await;
        };
        // One press per list, as for an approval.
        self.offered.remove(command);
        let chosen = format!("You chose {}.", literal(&label));
        self.settle(Asked::Command(command), &chosen).await;
        match command {
            "mode" => self.mode(&value).await,
            "model" => self.model(&value).await,
            "resume" => self.switch_to(&value).await,
            "new" => self.move_to(std::path::Path::new(&value)).await,
            "skill" => self.skill(&value).await,
            _ => self.say("That choice is no longer waiting.").await,
        }
    }

    async fn mode(&mut self, name: &str) {
        if !self.ready().await {
            return;
        }
        let Some(view) = &self.view else { return };
        if name.is_empty() {
            let current = match view.call(&protocol::GetPresentation {}).await {
                Ok(snapshot) => snapshot.settings.map(|settings| settings.mode),
                Err(_) => None,
            };
            let mut text = "Choose how much Medha may do without asking:".to_owned();
            for (name, mode, meaning) in MODES {
                let now = if current == Some(mode) { " (now)" } else { "" };
                text.push_str(&format!("\n{name}{now}: {meaning}"));
            }
            let names = MODES.map(|(name, ..)| name.to_owned()).to_vec();
            return self.offer("mode", &text, names.clone(), names).await;
        }
        // Autopilot was called yolo, and a phone may still say so.
        let name = if name.eq_ignore_ascii_case("yolo") {
            "autopilot"
        } else {
            name
        };
        let Some((name, mode, meaning)) = MODES
            .into_iter()
            .find(|(known, ..)| known.eq_ignore_ascii_case(name))
        else {
            return self
                .say("Modes are plan, careful, normal and autopilot.")
                .await;
        };
        let reply = match view.call(&protocol::Configure::Mode(mode)).await {
            Ok(_) => format!("Mode is now {name}: {meaning}."),
            Err(error) => format!("The mode did not change: {error}"),
        };
        self.say(&reply).await;
    }

    async fn model(&mut self, name: &str) {
        if !self.ready().await {
            return;
        }
        let Some(view) = &self.view else { return };
        let settings = match view.call(&protocol::GetPresentation {}).await {
            Ok(snapshot) => snapshot.settings,
            Err(error) => return self.say(&format!("Could not read the chat: {error}")).await,
        };
        let Some(settings) = settings else {
            return self.say("This chat has no model settings to change.").await;
        };
        if name.is_empty() {
            let names: Vec<String> = settings
                .profiles
                .iter()
                .map(|profile| profile.name.clone())
                .collect();
            let labels = settings
                .profiles
                .iter()
                .map(|profile| {
                    let now = if profile.name == settings.profile {
                        " (now)"
                    } else {
                        ""
                    };
                    format!("{}{now}", profile.name)
                })
                .collect();
            let text = format!(
                "This chat uses {}. Choose another saved model:",
                settings.model
            );
            return self.offer("model", &text, labels, names).await;
        }
        let Some(profile) = settings
            .profiles
            .iter()
            .find(|profile| profile.name == name)
        else {
            return self
                .say(&format!(
                    "No saved model is called {name}. Send /model to see them."
                ))
                .await;
        };
        let reply = match view
            .call(&protocol::Configure::Profile(profile.name.clone()))
            .await
        {
            Ok(changed) => format!(
                "This chat now uses {} ({}).",
                changed.profile, changed.model
            ),
            Err(error) => format!("The model did not change: {error}"),
        };
        self.say(&reply).await;
    }

    async fn usage(&mut self) {
        if !self.ready().await {
            return;
        }
        let Some(view) = &self.view else { return };
        let text = match view.call(&SessionRead::Usage).await {
            Ok(SessionResult::Text(text)) => text,
            Ok(_) => "The backend gave no usage.".to_owned(),
            Err(error) => format!("Could not read usage: {error}"),
        };
        self.say(&text).await;
    }

    async fn memories(&mut self) {
        if !self.ready().await {
            return;
        }
        let Some(view) = &self.view else { return };
        let text = match view.call(&SessionRead::Memories).await {
            Ok(SessionResult::Memories(memories)) if memories.is_empty() => {
                "Medha remembers nothing yet.".to_owned()
            }
            Ok(SessionResult::Memories(memories)) => {
                let lines: Vec<String> = memories
                    .iter()
                    .map(|memory| format!("- {}: {}", memory.name, memory.claim))
                    .collect();
                format!("What Medha remembers:\n{}", lines.join("\n"))
            }
            Ok(_) => "The backend gave no memories.".to_owned(),
            Err(error) => format!("Could not read memories: {error}"),
        };
        self.say(&text).await;
    }

    /// Recent chats in the gateway's folder, newest first, apart from the one in use.
    async fn recent(&mut self) {
        if self.running {
            return self
                .say("A task is running. Stop it with /stop first.")
                .await;
        }
        let current = self.shared.chats.conversation(&self.key).ok().flatten();
        let sessions = match self.folder_read(protocol::ReadResource::Sessions).await {
            Ok(ResourceResult::Sessions(sessions)) => sessions,
            Ok(_) => return self.say("The backend gave no chats.").await,
            Err(error) => return self.say(&format!("Could not list chats: {error}")).await,
        };
        let mut sessions: Vec<_> = sessions
            .into_iter()
            .filter(|session| Some(&session.id) != current.as_ref())
            .collect();
        sessions.sort_by(|a, b| b.updated.total_cmp(&a.updated));
        sessions.truncate(OFFERED);
        if sessions.is_empty() {
            return self.say("There is no other chat in this folder yet.").await;
        }
        let labels = sessions
            .iter()
            .map(|session| clip(session.title.as_deref().unwrap_or("Untitled chat")))
            .collect();
        let ids = sessions.into_iter().map(|session| session.id).collect();
        self.offer("resume", "Continue an earlier chat:", labels, ids)
            .await;
    }

    async fn switch_to(&mut self, conversation: &str) {
        if self.running {
            return self
                .say("A task is running. Stop it with /stop first.")
                .await;
        }
        self.let_go().await;
        if let Err(error) = self.shared.chats.remember(&self.key, conversation) {
            return self.say(&format!("Could not switch: {error:#}")).await;
        }
        self.say("Switched. Your next message continues that chat.")
            .await;
    }

    /// `/new`. With one folder, the next message starts over in it; with more, buttons ask which
    /// project the new chat works in, and a name typed after `/new` picks one. A path is never
    /// taken from the phone: only folders added on the computer are offered.
    pub(super) async fn fresh(&mut self, typed: &str) {
        if self.running {
            return self
                .say("A task is running. Stop it with /stop first.")
                .await;
        }
        let (_, allowed) = self.shared.settings.folders();
        let name = |folder: &std::path::Path| {
            folder.file_name().map_or_else(
                || folder.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
        };
        if !typed.is_empty() {
            return match allowed
                .iter()
                .find(|folder| name(folder).eq_ignore_ascii_case(typed))
            {
                Some(folder) => self.move_to(&folder.clone()).await,
                None => {
                    self.say(&format!(
                        "There is no project called {typed} here. Send /new to see the ones you can use."
                    ))
                    .await
                }
            };
        }
        if allowed.len() < 2 {
            self.let_go().await;
            if let Err(error) = self.shared.chats.forget(&self.key) {
                tracing::warn!(%error, "gateway: forgetting a chat");
            }
            return self
                .say("Your next message starts a new conversation.")
                .await;
        }
        let (now, _) = self.place();
        let mut text = "Which project should the new chat work in?".to_owned();
        let shown: Vec<_> = allowed.into_iter().take(OFFERED).collect();
        for folder in &shown {
            let here = if *folder == now { " (now)" } else { "" };
            text.push_str(&format!("\n{}{here}: {}", name(folder), folder.display()));
        }
        let labels = shown.iter().map(|folder| clip(&name(folder))).collect();
        let values = shown
            .iter()
            .map(|folder| folder.display().to_string())
            .collect();
        self.offer("new", &text, labels, values).await;
    }

    /// Checked again here: the list may have changed on the computer since the buttons were sent.
    async fn move_to(&mut self, folder: &std::path::Path) {
        if self.running {
            return self
                .say("A task is running. Stop it with /stop first.")
                .await;
        }
        if !folder.is_dir()
            || !self
                .shared
                .settings
                .folders()
                .1
                .iter()
                .any(|allowed| allowed == folder)
        {
            return self
                .say("That folder is no longer open to messages. Send /new to see the ones you can use.")
                .await;
        }
        self.let_go().await;
        if let Err(error) = self.shared.chats.choose_folder(&self.key, folder) {
            return self.say(&format!("Could not switch: {error:#}")).await;
        }
        self.say(&format!(
            "Now working in {}. Your next message starts a new chat there.",
            folder.display()
        ))
        .await;
    }

    async fn skills(&mut self) {
        let catalogue = match self.folder_read(protocol::ReadResource::Skills).await {
            Ok(ResourceResult::Skills(catalogue)) => catalogue,
            Ok(_) => return self.say("The backend gave no skills.").await,
            Err(error) => return self.say(&format!("Could not list skills: {error}")).await,
        };
        let usable: Vec<_> = catalogue
            .skills
            .into_iter()
            .filter(|skill| skill.available && skill.enabled)
            .collect();
        if usable.is_empty() {
            return self
                .say("No skill is installed. Install skills at the computer.")
                .await;
        }
        let mut text =
            "Skills Medha can use. Tap one to use it in this chat, or send /skill <name>:"
                .to_owned();
        for skill in &usable {
            text.push_str(&format!("\n- {}: {}", skill.name, clip(&skill.description)));
        }
        let names: Vec<String> = usable
            .into_iter()
            .map(|skill| skill.name)
            .take(OFFERED)
            .collect();
        self.offer("skill", &text, names.clone(), names).await;
    }

    async fn skill(&mut self, name: &str) {
        if name.is_empty() {
            return self.skills().await;
        }
        if !self.ready().await {
            return;
        }
        let Some(view) = &self.view else { return };
        let reply = match view
            .call(&protocol::UseSkill {
                name: name.to_owned(),
            })
            .await
        {
            Ok(_) => {
                format!("The {name} skill is on for this chat. Say what you want done with it.")
            }
            Err(error) => format!("Could not use {name}: {error}"),
        };
        self.say(&reply).await;
    }

    /// Offers `values` as buttons labelled `labels`; a press comes back to [`Self::chosen`].
    async fn offer(
        &mut self,
        command: &'static str,
        text: &str,
        labels: Vec<String>,
        values: Vec<String>,
    ) {
        let buttons = labels
            .into_iter()
            .enumerate()
            .map(|(index, label)| Button {
                label,
                choice: format!("c.{command}.{index}"),
            })
            .collect::<Vec<_>>();
        let offered = buttons
            .iter()
            .map(|button| button.label.clone())
            .zip(values)
            .collect();
        self.offered.insert(command, offered);
        self.ask(
            crate::prompts::Ask {
                text: text.to_owned(),
                buttons,
            },
            Some(Asked::Command(command)),
        )
        .await;
    }

    async fn folder_read(&self, read: protocol::ReadResource) -> Result<ResourceResult, String> {
        let connection = self.connection().await?;
        connection
            .call_async(protocol::Scope::Folder(&self.place().0), &read)
            .await
    }

    /// Follows or starts this chat's Medha chat before a command that needs it.
    async fn ready(&mut self) -> bool {
        if self.view.is_some() {
            return true;
        }
        match self.open().await {
            Ok(()) => true,
            Err(error) => {
                self.say(&format!("Could not reach Medha: {error}")).await;
                false
            }
        }
    }
}

fn clip(text: &str) -> String {
    transcript_view::clip(text, LABEL_CHARS)
}
