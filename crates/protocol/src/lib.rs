//! Application commands and presentation events shared by backend frontends.
//! No runtime, database, terminal, credential store or process ownership lives here.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::path::{Path, PathBuf};

mod presentation;
pub use presentation::*;
mod resources;
pub use resources::*;
mod session_features;
pub use session_features::*;

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
    /// Resolved choices of this caller. Unlike legacy desktop requests, these
    /// do not inherit the daemon starter's environment. Secrets are ephemeral.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<StartupOptions>,
}
command!(CreateSession, "session.create", Service, LiveSession);

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sandbox {
    Host,
    Native,
    Container,
    Ssh,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReasoningChoice {
    pub enabled: Option<bool>,
    pub effort: Option<Effort>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resume {
    #[default]
    None,
    Latest,
    Id(String),
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct BudgetLimits {
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_wall_s: Option<u64>,
}

/// Only explicit model overrides cross the authenticated local connection;
/// arbitrary environment variables never do. Debug output redacts secrets.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOverrides {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub max_ctx: Option<String>,
    pub protocol: Option<String>,
    pub auth: Option<String>,
    pub headers_json: Option<String>,
    pub max_output_tokens: Option<String>,
    pub token_counter: Option<String>,
    pub token_accounting: Option<String>,
    pub reasoning_support: Option<String>,
    pub image_input: Option<String>,
}

impl std::fmt::Debug for ModelOverrides {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelOverrides")
            .field("model", &self.model)
            .field("credentials", &"[redacted]")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchOverrides {
    pub tavily: Option<String>,
    pub brave: Option<String>,
    pub searxng: Option<String>,
}

impl std::fmt::Debug for SearchOverrides {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SearchOverrides { credentials: [redacted] }")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupOptions {
    pub model: Option<String>,
    pub base_url: Option<String>,
    #[serde(default)]
    pub model_env: ModelOverrides,
    #[serde(default)]
    pub search_env: SearchOverrides,
    #[serde(default)]
    pub budget: BudgetLimits,
    pub approve: Option<String>,
    pub reasoning: Option<ReasoningChoice>,
    pub mode: Option<Mode>,
    pub verify_command: Option<String>,
    #[serde(default)]
    pub require_verify: bool,
    pub sandbox: Option<Sandbox>,
    pub tools_preset: Option<String>,
    pub max_parallel_tools: Option<usize>,
    #[serde(default)]
    pub resume: Resume,
    #[serde(default)]
    pub first_run_setup: bool,
    #[serde(default)]
    pub may_start_unconfigured: bool,
}

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
    pub conversation: Option<String>,
    #[serde(default)]
    pub stream: Option<String>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attach {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
}
command!(Attach, "session.attach", Chat, Attached);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attached {
    pub session: String,
    #[serde(default)]
    pub stream: Option<String>,
    pub head: u64,
    pub replayed: usize,
    pub gap: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    pub stream: String,
    pub after: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionsReloaded {
    pub warnings: Vec<String>,
}
empty_command!(
    ReloadExtensions,
    "extensions.reload",
    Chat,
    ExtensionsReloaded
);

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
    /// Explicit intent prevents a delayed steer from starting another turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<SendIntent>,
}
command!(SendMessage, "message.send", Chat, MessageAccepted);

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SendIntent {
    Start,
    Steer { turn: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAccepted {
    pub accepted: bool,
    pub steered: bool,
    #[serde(default)]
    pub turn: u64,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecision {
    Approve,
    Once,
    Always,
    Session,
    Persistent,
    Deny,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalKind {
    #[default]
    Action,
    Access,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalPrompt {
    pub gate_id: u64,
    pub action: String,
    pub detail: Option<String>,
    pub escalated: bool,
    #[serde(default)]
    pub kind: ApprovalKind,
    #[serde(default)]
    pub choices: Vec<ApprovalDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResolved {
    pub gate_id: u64,
    pub approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerApproval {
    pub gate_id: u64,
    pub decision: ApprovalDecision,
}
command!(AnswerApproval, "approval.respond", Chat, Accepted);
empty_command!(AbortTurn, "turn.abort", Chat, Accepted);

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    pub recommended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub prompt: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionPrompt {
    pub question_id: u64,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionAnswered {
    pub question_id: u64,
}

/// Child presentation data is shared independently of the agent executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AgentStep {
    Task {
        objective: String,
        contract: Option<String>,
    },
    Text(String),
    Reasoning(String),
    ToolCall {
        id: Option<String>,
        tool: String,
        args: Value,
    },
    ToolResult {
        id: Option<String>,
        tool: String,
        ok: bool,
        payload: Value,
    },
    Restarted,
    SteerQueued(String),
    Steered(String),
    SteersReturned(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub surface_session: Option<String>,
    pub path: String,
    pub step: AgentStep,
}

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
    #[serde(rename = "turn.started")]
    Started { turn: u64 },
    #[serde(rename = "presentation.reset")]
    PresentationReset { revision: u64 },
    #[serde(rename = "message.accepted")]
    User { content: String },
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        completion_tokens: Option<u32>,
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
    Queued {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
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
    #[serde(rename = "turn.abort_settled")]
    AbortSettled,
    #[serde(rename = "turn.abort_slow")]
    AbortSlow,
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
