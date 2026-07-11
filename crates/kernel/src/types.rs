//! Canonical, vendor-neutral types the kernel reasons over (Vol 1 §4.4, Vol 3).
//! Providers translate to/from these — the core is never vendor-shaped.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Provenance/trust label carried by every span of context and every event (P7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLabel {
    User,
    System,
    Tool,
    Web,
    Memory,
    Skill,
}

impl TrustLabel {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrustLabel::User => "user",
            TrustLabel::System => "system",
            TrustLabel::Tool => "tool",
            TrustLabel::Web => "web",
            TrustLabel::Memory => "memory",
            TrustLabel::Skill => "skill",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user" => TrustLabel::User,
            "system" => TrustLabel::System,
            "tool" => TrustLabel::Tool,
            "web" => TrustLabel::Web,
            "memory" => TrustLabel::Memory,
            "skill" => TrustLabel::Skill,
            _ => return None,
        })
    }
}

/// Blast radius drives the verification requirement (P5, §4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlastRadius {
    Read,
    ReversibleLocal,
    IrreversibleLocal,
    External,
}

/// How strongly a tool's execution is contained (§4.8). The kernel's trust-flow
/// escalation reads this: a web-tainted consequential action is gated unless the
/// containment can stop the command from exfiltrating what it touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containment {
    /// No OS isolation — the command runs directly on the host.
    None,
    /// OS filesystem jail, but network is reachable (can still exfiltrate).
    OsFsJail,
    /// OS filesystem jail with network denied (cannot phone home).
    OsFsJailNoNet,
}

impl Containment {
    /// True if a confined command cannot reach the network — i.e. it can't
    /// exfiltrate anything it read, so a web-tainted action is safe to run.
    pub fn confines_network(&self) -> bool {
        matches!(self, Containment::OsFsJailNoNet)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Assistant messages: the tool calls the model requested this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolIntent>,
    /// Tool messages: the id of the call this message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self { role, content: content.into(), tool_calls: Vec::new(), tool_call_id: None }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }
    /// An assistant turn that requested tool calls.
    pub fn assistant_calls(content: impl Into<String>, tool_calls: Vec<ToolIntent>) -> Self {
        Self { role: Role::Assistant, content: content.into(), tool_calls, tool_call_id: None }
    }
    /// A tool result answering a specific call.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// A tool's capability class — the single source of truth for *presentation*
/// (a surface's glyph/colour/verb). Distinct from [`BlastRadius`], which drives
/// *authorization*: several categories share a blast radius (grep and fs.read
/// are both `Read` but "search" vs "read" visually). Declared once by the tool
/// so surfaces read it instead of re-deriving from the tool name (P8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    Read,
    Write,
    Search,
    Web,
    Shell,
    Vcs,
    Diagnostic,
    Plan,
    Other,
}

/// A tool exposed to the model this turn — the K2 capability sheath (§4.3, §4.5).
/// Registration ≠ exposure: only specs compiled into context can be called.
/// Carries the metadata every consumer needs — schema (for the model),
/// `blast_radius` (for the policy), `category` (for surfaces) — so none re-derive
/// it from the name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub schema: serde_json::Value,
    /// Authorization/verification driver (§4.7).
    pub blast_radius: BlastRadius,
    /// Presentation driver for a surface's colour/verb (§4.13).
    pub category: ToolCategory,
    /// The tool's own display glyph (a single grapheme). Declared by the tool so
    /// each keeps a distinct icon without any surface holding a name→glyph table.
    pub icon: String,
}

/// A model-proposed tool call. The harness validates and disposes; the model
/// never executes anything itself (P1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolIntent {
    pub id: String,
    pub tool: String,
    pub args: serde_json::Value,
}

/// Real token usage reported by the provider (authoritative — never estimated).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Resolved per-token pricing for the session's model, USD per million tokens
/// (P1-12). `indicative` marks a list price (e.g. models.dev) applied to a
/// route that may not actually bill it — a self-hosted deployment — so
/// surfaces label the figure "est." instead of presenting it as an invoice.
#[derive(Debug, Clone, Copy)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub indicative: bool,
}

impl Pricing {
    pub fn cost(&self, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        (prompt_tokens as f64 * self.input_per_mtok
            + completion_tokens as f64 * self.output_per_mtok)
            / 1_000_000.0
    }
}

/// The canonical streaming unit emitted by any provider (§4.4).
#[derive(Debug, Clone)]
pub enum Block {
    Text(String),
    ToolIntent(ToolIntent),
    /// A tool call has started streaming — its NAME is known (and often the target
    /// path/command, sniffed from the partial arguments) while the rest of the
    /// arguments are still arriving. Lets the surface show "writing medha.html…"
    /// while a large tool call is generated, instead of a vague spinner.
    ToolStarted { name: String, target: Option<String> },
    /// End-of-response token accounting (from the provider's `usage`).
    Usage(Usage),
    /// A reasoning/thinking-token delta (the `reasoning_content` field some
    /// servers stream — vLLM/DeepSeek-R1-style reasoning models). Kept
    /// distinct from `Text`: reasoning is shown live for transparency but is
    /// scratch content, never echoed back into subsequent-turn history.
    Reasoning(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObsStatus {
    Ok,
    Denied,
    Rejected,
    SchemaInvalid,
    Error,
}

/// Structured result of dispatching an intent. Failure is a first-class
/// outcome, never a dangling promise or silent truncation (P10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub intent_id: String,
    pub status: ObsStatus,
    pub payload: serde_json::Value,
}

impl Observation {
    pub fn ok(intent_id: impl Into<String>, payload: serde_json::Value) -> Self {
        Self { intent_id: intent_id.into(), status: ObsStatus::Ok, payload }
    }
    pub fn denial(intent_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            intent_id: intent_id.into(),
            status: ObsStatus::Denied,
            payload: serde_json::json!({ "reason": reason.into() }),
        }
    }
    pub fn error(intent_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            intent_id: intent_id.into(),
            status: ObsStatus::Error,
            payload: serde_json::json!({ "error": message.into() }),
        }
    }
}

/// Authorization outcome from the Policy engine (§4.6). Deny-first by default.
/// A `Verify` variant (pre-execution verifier chain, §4.7) is deliberately
/// absent until it actually routes through a verifier — a variant that silently
/// behaves like `Allow` is worse than no variant.
#[derive(Debug, Clone)]
pub enum Decision {
    Allow,
    Deny { reason: String },
    Human,
}

#[derive(Debug, Clone)]
pub enum TurnResult {
    Continuing,
    Final,
}

/// The output of the context compiler; the input to a provider (§4.3).
/// Carries the messages (K3–K5) and the exposed tool specs (K2).
#[derive(Debug, Clone)]
pub struct CompiledContext {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

/// Minimal session handle. Grows with budgets/interrupts in later phases.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: Ulid,
    pub done: bool,
}

impl Session {
    pub fn new() -> Self {
        Self { id: Ulid::new(), done: false }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
