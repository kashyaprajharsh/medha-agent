//! Recoverable application presentation, separate from provider transcripts
//! and transport replay. A cursor covers exactly the events in this snapshot.

use super::{ApprovalPrompt, Command, Cursor, QuestionPrompt, ScopeKind, Settings};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PresentationItem {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Reasoning {
        text: String,
    },
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
    Notice {
        text: String,
    },
    Compaction {
        before: u32,
        after: u32,
        summarized: bool,
        summary: Option<String>,
    },
    Verify {
        ok: bool,
        summary: String,
    },
    PreviewOmitted {
        subject: String,
        id: Option<String>,
        bytes: usize,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub prompt_tokens: u32,
    pub completion_tokens: Option<u32>,
    pub total_tokens: u32,
    pub cached_prompt_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub input_tokens: u64,
    pub input_limit: Option<u32>,
    pub usable_input_tokens: Option<u32>,
    pub quality: super::CountQuality,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PresentationMetrics {
    pub prompt_tokens: u64,
    pub cached_prompt_tokens: Option<u64>,
    pub cache_prompt_tokens: Option<u64>,
    pub cache_unreported_attempts: u64,
    pub last_usage: Option<UsageSnapshot>,
    pub cost_usd: Option<f64>,
    pub cost_indicative: bool,
    pub context_pressure: Option<ContextSnapshot>,
    pub compacting: bool,
    pub current_tool: Option<(String, Option<String>)>,
    pub reasoning_received_this_turn: bool,
    pub last_turn_reasoning_received: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPresentation {
    pub surface_session: Option<String>,
    pub path: String,
    pub items: Vec<PresentationItem>,
    pub omitted_items: u64,
    pub pending_steers: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentDoing {
    Thinking,
    Tool {
        #[serde(default)]
        tool: Option<String>,
        verb: String,
        target: Option<String>,
    },
    Waiting {
        action: String,
    },
    Idle,
    Finished,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RosterAgent {
    pub name: String,
    pub path: String,
    pub write: bool,
    pub session: String,
    pub objective: String,
    pub started_ms: u64,
    pub status: String,
    pub doing: Option<AgentDoing>,
    pub tool_calls: Option<u64>,
    pub tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresentationSnapshot {
    pub revision: u64,
    pub conversation: String,
    pub running: bool,
    pub turn: u64,
    pub pending_steers: Vec<String>,
    pub force_aborting: bool,
    pub settings: Option<Settings>,
    pub items: Vec<PresentationItem>,
    /// First row of the active reply, excluding previously saved history.
    /// None when idle. Frontends with a separate history view use this tail.
    #[serde(default)]
    pub current_turn_from: Option<usize>,
    /// Older rows evicted by the presentation budget remain in durable history.
    pub omitted_items: u64,
    pub approvals: Vec<ApprovalPrompt>,
    pub questions: Vec<QuestionPrompt>,
    pub metrics: PresentationMetrics,
    pub agents: Vec<AgentPresentation>,
    #[serde(default)]
    pub roster: Vec<RosterAgent>,
    pub omitted_agent_views: u64,
    /// Added by the backend at the output-order barrier, never guessed by a UI.
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetPresentation {}

impl Command for GetPresentation {
    const METHOD: &'static str = "session.presentation";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = PresentationSnapshot;
}
