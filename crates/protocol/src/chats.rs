//! Deleting a chat from any app, and bringing it back while the backend still can.

use crate::{Command, ScopeKind};
use serde::{Deserialize, Serialize};

/// Hides the chat at once and deletes it, with its sub-agents, a few seconds later.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteChat {
    pub session: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatHeld {
    pub held: bool,
}
impl Command for DeleteChat {
    const METHOD: &'static str = "chats.delete";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ChatHeld;
}

/// Pins or unpins a chat for every app that lists it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinChat {
    pub session: String,
    pub pinned: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatPinned {
    pub pinned: bool,
}
impl Command for PinChat {
    const METHOD: &'static str = "chats.pin";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ChatPinned;
}

/// Brings back a chat deleted moments ago, from whichever app deleted it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreChat {
    pub session: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRestored {
    pub restored: bool,
}
impl Command for RestoreChat {
    const METHOD: &'static str = "chats.restore";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ChatRestored;
}
