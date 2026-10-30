//! Hears one platform and hands each allowed message to its chat's link. Nobody
//! reaches the agent, or presses its buttons, before the owner has paired them.

use crate::chats::{self, Chats};
use crate::link::{ChatLink, Input, Settings, Shared};
use crate::pairing::Pairing;
use crate::platform::{Heard, Incoming, Platform};
use crate::store;
use anyhow::Result;
use medha_client::Backend;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

/// Messages one chat may have waiting while its link is busy.
const WAITING: usize = 32;

pub struct Router {
    shared: Arc<Shared>,
    pairing: Pairing,
    live: HashMap<String, mpsc::Sender<Input>>,
}

impl Router {
    /// Chats follow Medha's saved settings, read again on each use.
    pub fn new(
        platform: Arc<dyn Platform>,
        backend: Arc<Backend>,
        folder: PathBuf,
    ) -> Result<Self> {
        Ok(Self::in_home(
            platform,
            backend,
            folder,
            Settings::Saved,
            &store::home()?,
        ))
    }

    /// Keeps pairing and the chat map in `home` instead of Medha's own gateway folder, with the
    /// settings given here rather than read from Medha's.
    pub fn in_home(
        platform: Arc<dyn Platform>,
        backend: Arc<Backend>,
        folder: PathBuf,
        settings: Settings,
        home: &Path,
    ) -> Self {
        let chats = Arc::new(Chats::at(home.join("chats.json")));
        Self {
            shared: Arc::new(Shared {
                platform,
                backend,
                chats,
                folder,
                settings,
                home: std::sync::Mutex::new(None),
            }),
            pairing: Pairing::at(home.join("pairing.json")),
            live: HashMap::new(),
        }
    }

    /// The chat that gets notices, from the saved settings.
    pub fn home(self, chat: Option<String>) -> Self {
        *self
            .shared
            .home
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = chat;
        self
    }

    /// Runs until the platform cannot be reached at all; a failed poll is retried with a growing wait.
    pub async fn run(mut self) -> Result<()> {
        // A missing menu only loses the hint after `/`; the commands still work.
        let menu: Vec<(&str, &str)> = protocol::commands::CHAT_COMMANDS
            .iter()
            .map(|(name, description)| (name.trim_start_matches('/'), *description))
            .collect();
        if let Err(error) = self.shared.platform.offer_commands(&menu).await {
            tracing::warn!(error = %format!("{error:#}"), "gateway: listing commands");
        }
        self.resume_running().await;
        let mut failures: u32 = 0;
        loop {
            match self.shared.platform.hear().await {
                Ok(heard) => {
                    failures = 0;
                    for item in heard {
                        self.route(item).await;
                    }
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    let wait = Duration::from_secs(2u64.saturating_pow(failures.min(6)));
                    tracing::warn!(error = %format!("{error:#}"), ?wait, "gateway: hearing {}", self.shared.platform.name());
                    tokio::time::sleep(wait).await;
                }
            }
            self.live.retain(|_, inbox| !inbox.is_closed());
        }
    }

    pub(crate) async fn route(&mut self, heard: Heard) {
        let platform = self.shared.platform.name();
        match heard {
            Heard::Message(message) => {
                if message.group && !message.mentioned {
                    return;
                }
                match self.pairing.is_approved(platform, &message.user) {
                    Ok(true) => {
                        let chat = message.chat.clone();
                        self.deliver(&chat, Input::Message(message)).await;
                    }
                    Ok(false) => self.stranger(message).await,
                    Err(error) => {
                        tracing::warn!(error = %format!("{error:#}"), "gateway: reading pairing")
                    }
                }
            }
            Heard::Pressed { chat, user, choice } => {
                if self.pairing.is_approved(platform, &user).unwrap_or(false) {
                    self.deliver(&chat, Input::Pressed(choice)).await;
                }
            }
        }
    }

    /// A private message from someone unknown gets a code for the owner; groups get nothing.
    async fn stranger(&self, message: Incoming) {
        if message.group {
            return;
        }
        let platform = self.shared.platform.name();
        let name = if message.name.is_empty() {
            &message.user
        } else {
            &message.name
        };
        match self.pairing.request(platform, &message.user, name) {
            Ok(Some(code)) => {
                let text = format!(
                    "Medha does not know you yet. To let you in, the owner of this Medha runs this on their computer:\n\nmedha pairing approve {platform} {code}\n\nThe code works for one hour."
                );
                if let Err(error) = self.shared.platform.send(&message.chat, &text).await {
                    tracing::warn!(error = %format!("{error:#}"), "gateway: sending a pairing code");
                }
                self.tell_home(&message, name).await;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "gateway: making a pairing code")
            }
        }
    }

    /// The owner hears who asked; the code itself stays with the person who asked for it.
    async fn tell_home(&self, message: &Incoming, name: &str) {
        let home = self
            .shared
            .home
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(home) = home.filter(|home| *home != message.chat) else {
            return;
        };
        let platform = self.shared.platform.name();
        let text = format!(
            "{name} ({}) asked to message Medha on {platform}. If you know them, ask for the code they were sent and run: medha pairing approve {platform} <code>",
            message.user
        );
        if let Err(error) = self.shared.platform.send(&home, &text).await {
            tracing::warn!(error = %format!("{error:#}"), "gateway: telling the home chat");
        }
    }

    async fn deliver(&mut self, chat: &str, input: Input) {
        let key = chats::key(self.shared.platform.name(), chat);
        let input = match self.live.get(&key) {
            Some(inbox) => match inbox.try_send(input) {
                Ok(()) => return,
                Err(TrySendError::Full(_)) => {
                    let _ = self
                        .shared
                        .platform
                        .send(chat, "Too many messages at once. Wait for the reply.")
                        .await;
                    return;
                }
                // The link let go when idle; a new one resumes the same chat.
                Err(TrySendError::Closed(input)) => input,
            },
            None => input,
        };
        let (inbox, heard) = mpsc::channel(WAITING);
        let _ = inbox.try_send(input);
        tokio::spawn(
            ChatLink::new(Arc::clone(&self.shared), key.clone(), chat.to_owned()).run(heard),
        );
        self.live.insert(key, inbox);
    }

    /// Follows chats this platform left running, so a task that outlived a restart still reports back.
    async fn resume_running(&mut self) {
        let prefix = chats::key(self.shared.platform.name(), "");
        let Ok(known) = self.shared.chats.all() else {
            return;
        };
        let backend = Arc::clone(&self.shared.backend);
        let Ok(Ok(connection)) = tokio::task::spawn_blocking(move || backend.connection()).await
        else {
            return;
        };
        let Ok(listed) = connection
            .call_async(protocol::Scope::Service, &protocol::ListSessions {})
            .await
        else {
            return;
        };
        for (key, conversation) in known {
            let Some(chat) = key.strip_prefix(&prefix) else {
                continue;
            };
            let live = listed
                .sessions
                .iter()
                .any(|live| live.conversation.as_ref() == Some(&conversation));
            if live {
                self.deliver(chat, Input::Resume).await;
            }
        }
    }
}
