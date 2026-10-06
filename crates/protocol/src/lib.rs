//! Application commands and presentation events shared by backend frontends.
//! No runtime, database, terminal, credential store or process ownership lives here.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub trait Command: Serialize + DeserializeOwned {
    const METHOD: &'static str;
    const SCOPE: ScopeKind;
    type Output: DeserializeOwned;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Service,
    Folder,
    Chat,
}

#[derive(Clone, Copy, Debug)]
pub enum Scope<'a> {
    Service,
    Folder(&'a Path),
    Chat(&'a str),
}

/// Numbering belongs to the transport. A caller cannot override a request id,
/// a method name, or an envelope's scope through command parameters.
#[derive(Serialize)]
pub struct Call<'a, T: Command> {
    jsonrpc: &'static str,
    method: &'static str,
    params: &'a T,
    #[serde(skip_serializing_if = "Option::is_none")]
    folder: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<&'a str>,
}

impl<'a, T: Command> Call<'a, T> {
    pub fn new(scope: Scope<'a>, params: &'a T) -> Result<Self, &'static str> {
        let (kind, folder, session) = match scope {
            Scope::Service => (ScopeKind::Service, None, None),
            Scope::Folder(folder) => (ScopeKind::Folder, Some(folder), None),
            Scope::Chat(session) => (ScopeKind::Chat, None, Some(session)),
        };
        if kind != T::SCOPE {
            return Err("the command has the wrong scope");
        }
        Ok(Self {
            jsonrpc: "2.0",
            method: T::METHOD,
            params,
            folder,
            session,
        })
    }
}

macro_rules! command {
    ($ty:ty, $method:literal, $scope:ident, $output:ty) => {
        impl Command for $ty {
            const METHOD: &'static str = $method;
            const SCOPE: ScopeKind = ScopeKind::$scope;
            type Output = $output;
        }
    };
}

macro_rules! empty_command {
    ($ty:ident, $method:literal, $scope:ident, $output:ty) => {
        #[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
        pub struct $ty {}
        command!($ty, $method, $scope, $output);
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Plan,
    Careful,
    Normal,
    Yolo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reasoning {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Auto,
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningSupport {
    #[serde(alias = "unknown")]
    Unverified,
    Unsupported,
    Effort,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoreSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSession {
    pub folder: PathBuf,
    #[serde(default)]
    pub ends_with_client: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<RestoreSettings>,
}
command!(CreateSession, "session.create", Service, LiveSession);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatAbout {
    pub folder: PathBuf,
    pub model: String,
    #[serde(default)]
    pub notices: Vec<String>,
}

/// The live routing id is distinct from the durable conversation id that may
/// change when a rewind creates a branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveSession {
    pub session: String,
    #[serde(default)]
    pub about: Option<ChatAbout>,
    #[serde(default)]
    pub head: u64,
    #[serde(default)]
    pub clients: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionList {
    pub sessions: Vec<LiveSession>,
}
empty_command!(ListSessions, "session.list", Service, SessionList);

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Attach {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
}
command!(Attach, "session.attach", Chat, Attached);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attached {
    pub session: String,
    pub head: u64,
    pub replayed: usize,
    pub gap: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Closing {
    pub closing: bool,
}
empty_command!(Close, "session.close", Chat, Closing);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detached {
    pub detached: bool,
}
empty_command!(Detach, "session.detach", Chat, Detached);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub model: String,
    pub protocol: String,
    pub default: bool,
    pub reasoning_support: ReasoningSupport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub profiles: Vec<Profile>,
    pub profile: String,
    pub model: String,
    pub mode: Mode,
    pub reasoning: Reasoning,
    pub effort: Effort,
    pub efforts: Vec<Effort>,
    pub reasoning_support: ReasoningSupport,
    pub streaming: bool,
    pub context_limit: Option<u32>,
    // Older compatible backends did not report these. A client must treat
    // their absence as unknown, never as confirmed lack of support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_support: Option<String>,
}
empty_command!(GetSettings, "session.settings", Chat, Settings);

/// An externally tagged enum produces the existing one-setting JSON object,
/// and refuses malformed/multiple changes instead of selecting one silently.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Configure {
    Profile(String),
    Mode(Mode),
    Reasoning(Reasoning),
    Effort(Effort),
    Streaming(bool),
}
command!(Configure, "session.configure", Chat, Settings);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessage {
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
}
command!(SendMessage, "message.send", Chat, MessageAccepted);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAccepted {
    pub accepted: bool,
    pub steered: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cancelled {
    pub cancelled: bool,
}
empty_command!(Cancel, "cancel", Chat, Cancelled);
empty_command!(Interrupt, "interrupt", Chat, Cancelled);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Accepted {
    pub accepted: bool,
}

/// The existing permission contract. Additional access-grant tiers must be
/// negotiated explicitly before a new client presents them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerApproval {
    pub gate_id: u64,
    pub decision: ApprovalDecision,
}
command!(AnswerApproval, "approval.respond", Chat, Accepted);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Answer {
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerQuestion {
    pub question_id: u64,
    #[serde(default)]
    pub dismiss: bool,
    #[serde(default)]
    pub answers: Vec<Answer>,
}
command!(AnswerQuestion, "question.respond", Chat, Accepted);

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CountQuality {
    Authoritative,
    ProviderEstimate,
    LocalEstimate,
}

/// The `event` notification payload, preserving today's wire names. Dynamic
/// tool arguments/results are data inside a typed event, never RPC dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum TurnEvent {
    #[serde(rename = "model.waiting")]
    Waiting,
    #[serde(rename = "notice")]
    Notice { text: String },
    #[serde(rename = "model.text")]
    Text { delta: String },
    #[serde(rename = "model.reasoning")]
    Reasoning { delta: String },
    #[serde(rename = "tool.started")]
    ToolStarted {
        tool: String,
        target: Option<String>,
    },
    #[serde(rename = "tool.call")]
    ToolCall {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        tool: String,
        args: Value,
    },
    #[serde(rename = "tool.observation")]
    ToolResult {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        tool: String,
        ok: bool,
        payload: Value,
    },
    #[serde(rename = "tool.input")]
    ToolInput {
        id: String,
        tool: String,
        delta: String,
    },
    #[serde(rename = "tool.screen")]
    ToolScreen { id: String, screen: Value },
    #[serde(rename = "usage")]
    Usage {
        prompt_tokens: u32,
        total_tokens: u32,
        cached_prompt_tokens: Option<u32>,
    },
    #[serde(rename = "cost")]
    Cost { total_usd: f64, indicative: bool },
    #[serde(rename = "context_pressure")]
    ContextPressure {
        input_tokens: u64,
        input_limit: Option<u32>,
        usable_input_tokens: Option<u32>,
        quality: CountQuality,
        percent: Option<u32>,
    },
    #[serde(rename = "verify")]
    Verify { ok: bool, summary: String },
    #[serde(rename = "compacting")]
    Compacting { active: bool },
    #[serde(rename = "compaction")]
    Compaction {
        before: u32,
        after: u32,
        summarized: bool,
        summary: Option<String>,
    },
    #[serde(rename = "message.queued")]
    Queued,
    #[serde(rename = "message.steered")]
    Steered { content: String },
    #[serde(rename = "message.returned")]
    Returned { contents: Vec<String> },
    #[serde(rename = "model.restarted")]
    Restarted,
    #[serde(rename = "turn.done")]
    Done { stopped: Option<String> },
    #[serde(rename = "turn.cancelled")]
    Cancelled,
    #[serde(rename = "turn.continued")]
    Continued { stopped: String },
    #[serde(rename = "turn.error")]
    Error { message: String },
    // Future events may be ignored, but malformed known events still fail.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests;
