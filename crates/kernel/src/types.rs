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
    Workspace,
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
            TrustLabel::Workspace => "workspace",
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
            "workspace" => TrustLabel::Workspace,
            _ => return None,
        })
    }

    /// Ordering for taint propagation (§4.6/§4.10): higher = more trusted.
    /// Web is the floor — fetched content is presumed hostile.
    pub fn rank(&self) -> u8 {
        match self {
            TrustLabel::System => 5,
            TrustLabel::User => 4,
            TrustLabel::Skill => 3,
            TrustLabel::Workspace => 3,
            TrustLabel::Memory => 2,
            TrustLabel::Tool => 2,
            TrustLabel::Web => 0,
        }
    }

    /// The less-trusted of two labels — taint flows toward the floor.
    pub fn min(self, other: Self) -> Self {
        if other.rank() < self.rank() {
            other
        } else {
            self
        }
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
    /// Trust of content injected into the conversation from outside it — a
    /// sub-agent's report above all. Without it a background report enters as
    /// plain user text and the taint the child accumulated is lost at the door.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<TrustLabel>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            trust: None,
        }
    }

    pub fn carrying(mut self, trust: TrustLabel) -> Self {
        self.trust = Some(trust);
        self
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }
    /// An assistant turn that requested tool calls.
    pub fn assistant_calls(content: impl Into<String>, tool_calls: Vec<ToolIntent>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            trust: None,
        }
    }
    /// A tool result answering a specific call.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            trust: None,
        }
    }
}

/// Opaque replay metadata returned by one wire protocol. The value is
/// serialized unchanged but deliberately omitted from `Debug` so signed or
/// encrypted reasoning state cannot leak into diagnostics.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderState {
    pub protocol: crate::provider::Protocol,
    pub kind: String,
    pub value: serde_json::Value,
}

impl std::fmt::Debug for ProviderState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderState")
            .field("protocol", &self.protocol)
            .field("kind", &self.kind)
            .field("value", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextPart {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_state: Vec<ProviderState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallPart {
    pub id: String,
    pub tool: String,
    pub args: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_state: Vec<ProviderState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultPart {
    pub tool_call_id: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_state: Vec<ProviderState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningPart {
    /// Optional visible summary. Opaque replay-only state remains in
    /// `provider_state` and is never substituted into this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_state: Vec<ProviderState>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum MediaSource {
    Url(String),
    Base64(String),
}

impl std::fmt::Debug for MediaSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Url(url) => formatter.debug_tuple("Url").field(url).finish(),
            Self::Base64(_) => formatter
                .debug_tuple("Base64")
                .field(&"<redacted>")
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaPart {
    pub mime_type: String,
    pub source: MediaSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_state: Vec<ProviderState>,
}

/// Ordered canonical content. Protocol adapters must preserve this order and
/// may consume provider state only when its protocol tag matches their own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ContentPart {
    Text(TextPart),
    ToolCall(ToolCallPart),
    ToolResult(ToolResultPart),
    Reasoning(ReasoningPart),
    Media(MediaPart),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: Role,
    pub parts: Vec<ContentPart>,
}

impl ModelMessage {
    /// True when any part carries opaque state which only its owning protocol
    /// may consume. Used by compatibility bridges to avoid lossy rewrites.
    pub fn has_provider_state(&self) -> bool {
        self.parts.iter().any(|part| match part {
            ContentPart::Text(part) => !part.provider_state.is_empty(),
            ContentPart::ToolCall(part) => !part.provider_state.is_empty(),
            ContentPart::ToolResult(part) => !part.provider_state.is_empty(),
            ContentPart::Reasoning(part) => !part.provider_state.is_empty(),
            ContentPart::Media(part) => !part.provider_state.is_empty(),
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LegacyMessageError {
    #[error("ordered message contains content the legacy Message type cannot represent")]
    NotRepresentable,
}

impl From<&Message> for ModelMessage {
    fn from(message: &Message) -> Self {
        let mut parts = Vec::new();
        match message.role {
            Role::Tool => {
                if let Some(tool_call_id) = &message.tool_call_id {
                    parts.push(ContentPart::ToolResult(ToolResultPart {
                        tool_call_id: tool_call_id.clone(),
                        content: message.content.clone(),
                        provider_state: Vec::new(),
                    }));
                } else if !message.content.is_empty() {
                    parts.push(ContentPart::Text(TextPart {
                        text: message.content.clone(),
                        provider_state: Vec::new(),
                    }));
                }
            }
            _ => {
                if !message.content.is_empty() {
                    parts.push(ContentPart::Text(TextPart {
                        text: message.content.clone(),
                        provider_state: Vec::new(),
                    }));
                }
                parts.extend(message.tool_calls.iter().map(|call| {
                    ContentPart::ToolCall(ToolCallPart {
                        id: call.id.clone(),
                        tool: call.tool.clone(),
                        args: call.args.clone(),
                        provider_state: Vec::new(),
                    })
                }));
            }
        }
        Self {
            role: message.role.clone(),
            parts,
        }
    }
}

impl TryFrom<&ModelMessage> for Message {
    type Error = LegacyMessageError;

    fn try_from(message: &ModelMessage) -> Result<Self, Self::Error> {
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut tool_call_id = None;
        let mut saw_tool_call = false;
        for part in &message.parts {
            match (&message.role, part) {
                (Role::System | Role::User | Role::Assistant, ContentPart::Text(part))
                    if part.provider_state.is_empty() && !saw_tool_call =>
                {
                    content.push_str(&part.text);
                }
                (Role::Assistant, ContentPart::ToolCall(part))
                    if part.provider_state.is_empty() =>
                {
                    saw_tool_call = true;
                    tool_calls.push(ToolIntent {
                        id: part.id.clone(),
                        tool: part.tool.clone(),
                        args: part.args.clone(),
                    });
                }
                (Role::Tool, ContentPart::ToolResult(part))
                    if part.provider_state.is_empty() && tool_call_id.is_none() =>
                {
                    tool_call_id = Some(part.tool_call_id.clone());
                    content.push_str(&part.content);
                }
                (Role::Tool, ContentPart::Text(part))
                    if part.provider_state.is_empty() && tool_call_id.is_none() =>
                {
                    content.push_str(&part.text);
                }
                _ => return Err(LegacyMessageError::NotRepresentable),
            }
        }
        Ok(Message {
            role: message.role.clone(),
            content,
            tool_calls,
            tool_call_id,
            trust: None,
        })
    }
}

impl Message {
    pub fn ordered(&self) -> ModelMessage {
        ModelMessage::from(self)
    }
}

#[cfg(test)]
mod ordered_message_tests {
    use super::*;

    #[test]
    fn legacy_assistant_bridge_has_one_deterministic_order_and_round_trips() {
        let legacy = Message::assistant_calls(
            "before tools",
            vec![
                ToolIntent {
                    id: "one".into(),
                    tool: "fs.read".into(),
                    args: serde_json::json!({"path": "a"}),
                },
                ToolIntent {
                    id: "two".into(),
                    tool: "fs.read".into(),
                    args: serde_json::json!({"path": "b"}),
                },
            ],
        );

        let ordered = legacy.ordered();
        assert!(matches!(ordered.parts[0], ContentPart::Text(_)));
        assert!(matches!(ordered.parts[1], ContentPart::ToolCall(_)));
        assert!(matches!(ordered.parts[2], ContentPart::ToolCall(_)));

        let restored = Message::try_from(&ordered).unwrap();
        assert_eq!(restored.role, Role::Assistant);
        assert_eq!(restored.content, legacy.content);
        assert_eq!(restored.tool_calls.len(), 2);
        assert_eq!(restored.tool_calls[1].id, "two");
    }

    #[test]
    fn compatibility_bridge_refuses_to_destroy_interleaving_or_provider_state() {
        let state = ProviderState {
            protocol: crate::provider::Protocol::GeminiInteractions,
            kind: "thought-signature".into(),
            value: serde_json::json!({"signature": "signed-value"}),
        };
        let interleaved = ModelMessage {
            role: Role::Assistant,
            parts: vec![
                ContentPart::ToolCall(ToolCallPart {
                    id: "call".into(),
                    tool: "tool".into(),
                    args: serde_json::json!({}),
                    provider_state: vec![state.clone()],
                }),
                ContentPart::Text(TextPart {
                    text: "after".into(),
                    provider_state: Vec::new(),
                }),
            ],
        };
        assert!(matches!(
            Message::try_from(&interleaved),
            Err(LegacyMessageError::NotRepresentable)
        ));

        let encoded = serde_json::to_value(&state).unwrap();
        let decoded: ProviderState = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded, state, "opaque state must round-trip unchanged");
        assert_eq!(encoded["value"]["signature"], "signed-value");
        assert!(!format!("{state:?}").contains("signed-value"));
    }

    #[test]
    fn embedded_media_is_redacted_from_debug_output() {
        let source = MediaSource::Base64("private-image-data".into());
        assert!(!format!("{source:?}").contains("private-image-data"));
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
    ToolStarted {
        name: String,
        target: Option<String>,
    },
    /// End-of-response token accounting (from the provider's `usage`).
    Usage(Usage),
    /// A reasoning/thinking-token delta (the `reasoning_content` field some
    /// servers stream — vLLM/DeepSeek-R1-style reasoning models). Kept
    /// distinct from `Text`: reasoning is shown live for transparency but is
    /// scratch content, never echoed back into subsequent-turn history.
    Reasoning(String),
    /// Complete canonical assistant message for replay. Native protocol
    /// decoders emit ordinary delta blocks for live display and exactly one of
    /// these at completion so ordered parts and opaque provider state reach the
    /// kernel without being flattened.
    CompletedMessage(ModelMessage),
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
    /// Trust of content this tool is *relaying* rather than producing, when it
    /// knows the provenance. The kernel takes the weaker of this and the label
    /// its category implies, so a tool handing back a sub-agent's web-derived
    /// findings cannot launder them into ordinary tool output.
    pub relayed_trust: Option<TrustLabel>,
}

impl Observation {
    pub fn ok(intent_id: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            intent_id: intent_id.into(),
            status: ObsStatus::Ok,
            payload,
            relayed_trust: None,
        }
    }
    pub fn denial(intent_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            intent_id: intent_id.into(),
            status: ObsStatus::Denied,
            payload: serde_json::json!({ "reason": reason.into() }),
            relayed_trust: None,
        }
    }
    pub fn error(intent_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            intent_id: intent_id.into(),
            status: ObsStatus::Error,
            payload: serde_json::json!({ "error": message.into() }),
            relayed_trust: None,
        }
    }
    pub fn relaying(mut self, trust: TrustLabel) -> Self {
        self.relayed_trust = Some(trust);
        self
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
    /// Exact ordered history when the caller has it. The flat `messages` view
    /// remains during migration for context engines and existing surfaces.
    pub ordered: Option<Vec<ModelMessage>>,
    pub tools: Vec<ToolSpec>,
}

impl CompiledContext {
    /// Compatibility view used while the kernel and context compiler still
    /// store flat messages. Protocol adapters can consume ordered parts now,
    /// without forcing a flag-day rewrite of every existing caller.
    pub fn ordered_messages(&self) -> Vec<ModelMessage> {
        self.ordered
            .clone()
            .unwrap_or_else(|| self.messages.iter().map(Message::ordered).collect())
    }
}

/// How much the agent may do without asking (§4.6 autonomy dial). This only ever
/// controls whether *otherwise-allowed* reversible/shell actions are escalated to
/// the human gate — it can never loosen a base `Human`/`Deny` (the safety floor:
/// dangerous-command scanner, external actions, out-of-workspace access,
/// web-taint escalation all sit outside this dial).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutonomyLevel {
    /// Edits and shell both ask for approval (safest).
    #[default]
    Careful,
    /// Reversible edits auto-run; shell still asks.
    Normal,
    /// Everything in-workspace auto-runs; the floor still gates catastrophe.
    Yolo,
}

impl AutonomyLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            AutonomyLevel::Careful => "careful",
            AutonomyLevel::Normal => "normal",
            AutonomyLevel::Yolo => "yolo",
        }
    }
    /// Parse a level id; unknown → `Careful` (the safe default).
    pub fn from_id(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "normal" => AutonomyLevel::Normal,
            "yolo" => AutonomyLevel::Yolo,
            _ => AutonomyLevel::Careful,
        }
    }
}

/// Minimal session handle. Grows with budgets/interrupts in later phases.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: Ulid,
    pub done: bool,
    /// The session's autonomy dial (default `Careful`). Read by the policy to
    /// decide how much to escalate to the human gate.
    pub autonomy: AutonomyLevel,
}

impl Session {
    pub fn new() -> Self {
        Self {
            id: Ulid::new(),
            done: false,
            autonomy: AutonomyLevel::Careful,
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
