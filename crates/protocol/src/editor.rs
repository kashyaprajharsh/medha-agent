//! Session-scoped inputs and exact turn completion used by external adapters.
use super::{Command, ScopeKind};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMcpServer {
    pub name: String,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl std::fmt::Debug for SessionMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionMcpServer")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("arguments_and_environment", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompletion {
    pub turn: u64,
    pub outcome: TurnOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnOutcome {
    Finished,
    Cancelled,
    Refused,
    TokenLimit,
    RequestLimit,
    Failed { message: String },
}

/// A cancellation following asynchronous prompt admission must never stop a
/// newer turn started by another viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelTurn {
    pub turn: u64,
}
impl Command for CancelTurn {
    const METHOD: &'static str = "turn.cancel";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = super::Cancelled;
}

/// A verified durable event delivered in byte-bounded fragments. A load fixes
/// `through` on its first read; concurrent appends are delivered by the live
/// stream instead. Offsets count UTF-8 bytes in the serialized event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadHistory {
    pub after: u64,
    pub through: Option<u64>,
    pub offset: usize,
    pub conversation: Option<String>,
}
impl Command for ReadHistory {
    const METHOD: &'static str = "session.history";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = HistoryFragment;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryFragment {
    pub conversation: String,
    pub through: u64,
    pub after: u64,
    pub offset: usize,
    pub data: String,
    pub event_finished: bool,
    pub finished: bool,
    pub cursor: Option<super::Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<Vec<super::PresentationItem>>,
}

/// Visible durable history. Provider replay state and private tool metadata
/// are excluded before crossing the application boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub id: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub trust: String,
    pub source: String,
}

/// Live presentation keeps references, so image bytes do not fill replay and
/// viewer queues. Only the backend reads its workspace artifact store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedImage {
    pub hash: String,
    pub mime: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadHistoryImage {
    pub hash: String,
}
impl Command for ReadHistoryImage {
    const METHOD: &'static str = "session.history.image";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = super::Image;
}
