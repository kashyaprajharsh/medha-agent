//! Typed terminal-facing operations over the owning application runtime.
use super::{Command, ResourceScope, ScopeKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub command: String,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Completed,
    Exhausted,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentPhase {
    Generating,
    InTool {
        tool: String,
        target: Option<String>,
    },
    AwaitingApproval {
        action: String,
    },
    Idle,
    Settled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub path: String,
    pub session: String,
    pub objective: String,
    pub started_ms: u64,
    pub write: bool,
    /// None means running; settled outcomes remain separate from phase.
    pub status: Option<AgentStatus>,
    pub phase: Option<AgentPhase>,
    pub tool_calls: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveState {
    pub agents: Vec<AgentState>,
    pub tasks: Vec<BackgroundTask>,
    pub pending_patches: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub name: String,
    pub claim: String,
    pub description: String,
    pub kind: String,
    pub scope: ResourceScope,
    pub trust: String,
    pub confidence: String,
    pub provenance: Vec<String>,
    pub sessions: Vec<String>,
    pub version: u32,
    pub pinned: bool,
    pub links: Vec<String>,
    pub created: f64,
    pub updated: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEvidence {
    pub entry: MemoryEntry,
    pub session: Option<String>,
    pub event: Option<String>,
    pub kind: Option<String>,
    pub timestamp: Option<f64>,
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "query",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SessionRead {
    Bootstrap,
    Live,
    Lsp,
    Usage,
    Memories,
    MemoryEvidence { scope: ResourceScope, name: String },
    AgentTranscript { session: String },
    AgentPatches,
}
impl Command for SessionRead {
    const METHOD: &'static str = "session.inspect";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = SessionResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPatch {
    pub id: String,
    pub agent: String,
    pub session: String,
    pub files: Vec<String>,
    pub diff: String,
    pub verification: Option<Verification>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    pub passed: bool,
    pub command: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
pub enum SessionResult {
    Bootstrap(Bootstrap),
    Live(LiveState),
    Text(String),
    Lsp(Value),
    Memories(Vec<MemoryEntry>),
    MemoryEvidence(Box<MemoryEvidence>),
    Transcript {
        items: Vec<super::PresentationItem>,
        omitted: u64,
    },
    Patches(Vec<AgentPatch>),
    Changed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPresentation {
    pub name: String,
    pub icon: String,
    pub category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bootstrap {
    pub tools: Vec<ToolPresentation>,
    pub show_thinking: bool,
    pub full_transparency: bool,
    pub execution_backend: String,
    pub memory_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SessionChange {
    PinMemory {
        scope: ResourceScope,
        name: String,
        pinned: bool,
    },
    ForgetMemory {
        scope: ResourceScope,
        name: String,
    },
    ApplyPatch {
        id: String,
        override_verification: bool,
    },
    StopAgents,
}
impl Command for SessionChange {
    const METHOD: &'static str = "session.change";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = SessionResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateSavedProfile {
    pub profile: String,
}
impl Command for ActivateSavedProfile {
    const METHOD: &'static str = "session.profile.saved";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = super::Settings;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpDefinition {
    pub id: String,
    pub url: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub disabled: bool,
    pub remote: bool,
    pub running: bool,
    pub tools: usize,
    /// Safe hash for an explicit review; no credential fields are exposed.
    pub hash: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub exposed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum McpCommand {
    List,
    Tools {
        id: String,
    },
    Expose {
        id: String,
        tool: String,
        exposed: bool,
    },
    Add {
        definition: super::Secret,
    },
    Disable {
        id: String,
        disabled: bool,
    },
    Remove {
        id: String,
    },
    Connect {
        id: String,
        hash: String,
    },
    Authorize {
        id: String,
    },
    Authenticate {
        id: String,
        oauth: bool,
    },
    Connectors {
        query: String,
    },
    ConnectConnector {
        id: String,
    },
    Credential {
        id: String,
        key: super::Secret,
    },
    Catalogue {
        query: String,
    },
}
impl Command for McpCommand {
    const METHOD: &'static str = "session.mcp";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = McpResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CataloguePick {
    pub label: String,
    pub command: String,
    pub cursor: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorPick {
    pub id: String,
    pub name: String,
    pub description: String,
    pub configured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
pub enum AgentCommand {
    Stop { agent: String },
    Steer { agent: String, text: String },
    Followup { agent: String, text: String },
}
impl Command for AgentCommand {
    const METHOD: &'static str = "agent.control";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = super::Accepted;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindPoint {
    pub id: String,
    pub text: String,
    pub files: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindPoints {
    pub points: Vec<RewindPoint>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetRewindPoints {}
impl Command for GetRewindPoints {
    const METHOD: &'static str = "session.rewind.points";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = RewindPoints;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RewindScope {
    Conversation,
    Code,
    Both,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rewind {
    pub at: String,
    pub scope: RewindScope,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rewound {
    pub source: String,
    pub session: String,
    pub code_only: bool,
    pub restored: usize,
    pub prefill: String,
    pub images: Vec<super::Image>,
}
impl Command for Rewind {
    const METHOD: &'static str = "session.rewind";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = Rewound;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
pub enum McpResult {
    Servers(Vec<McpDefinition>),
    Tools { id: String, tools: Vec<McpTool> },
    Catalogue(Vec<CataloguePick>),
    Connectors(Vec<ConnectorPick>),
    Changed,
    Authorization { id: String, url: String },
    // Tool status payloads belong to the registered MCP tool, not dispatch.
    Status(Value),
}
