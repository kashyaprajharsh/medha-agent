//! Canonical, vendor-neutral types used by the kernel.
//! Providers translate to/from these — the core is never vendor-shaped.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use ulid::Ulid;

/// Provenance/trust label carried by every span of context and every event.
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

    /// Ordering for taint propagation: higher = more trusted.
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

/// Blast radius drives authorization and verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlastRadius {
    Read,
    ReversibleLocal,
    IrreversibleLocal,
    External,
}

/// How strongly a tool's execution is contained. The kernel's trust-flow
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
    pub fn assistant_calls(content: impl Into<String>, tool_calls: Vec<ToolIntent>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            trust: None,
        }
    }
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
    /// Internal provenance carried through canonical replay and compaction.
    /// Protocol adapters deliberately ignore this field; it exists so moving
    /// from the legacy view to ordered content cannot upgrade injected
    /// Web/Tool/Memory text into an operator-authored user message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<TrustLabel>,
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
            trust: message.trust,
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
            trust: message.trust,
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
    fn ordered_bridge_preserves_user_input_trust() {
        let legacy = Message::user("background report").carrying(TrustLabel::Web);
        let ordered = legacy.ordered();
        assert_eq!(ordered.trust, Some(TrustLabel::Web));
        assert_eq!(Message::try_from(&ordered).unwrap().trust, legacy.trust);
    }

    #[test]
    fn ordered_trust_field_is_wire_compatible_with_older_payloads() {
        let unlabelled = Message::user("operator input").ordered();
        let encoded = serde_json::to_value(&unlabelled).unwrap();
        assert!(
            encoded.get("trust").is_none(),
            "the common unlabelled form must retain its pre-field encoding"
        );

        let decoded: ModelMessage = serde_json::from_value(serde_json::json!({
            "role": "user",
            "parts": []
        }))
        .unwrap();
        assert_eq!(decoded.trust, None);
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
            trust: None,
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

/// Presentation category, distinct from authorization [`BlastRadius`].
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

/// Tool schema and metadata exposed to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub schema: serde_json::Value,
    /// Authorization and verification driver.
    pub blast_radius: BlastRadius,
    /// Presentation driver for a surface's colour and verb.
    pub category: ToolCategory,
    /// The tool's own display glyph (a single grapheme). Declared by the tool so
    /// each keeps a distinct icon without any surface holding a name→glyph table.
    pub icon: String,
}

/// Portable function-name form used by providers whose tool identifiers only
/// admit ASCII letters, digits, `_`, and `-`.
///
/// Canonical tool names remain dotted inside MEDHA (`web.search`). Providers
/// expose this form on the wire (`web_search`). Model-generated *arguments* can
/// nevertheless copy that visible spelling, so structured fields which refer
/// to tools use the same conversion when accepting an unambiguous legacy alias.
pub fn portable_tool_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Deterministic provider wire name → canonical name map.
///
/// Colliding sanitized names receive trailing underscores. Canonical names are
/// sorted internally so providers and semantic nested-reference resolvers make
/// the same choice even when one starts from an unordered catalogue.
pub fn portable_tool_name_map(available: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut ordered: Vec<&String> = available.iter().collect();
    ordered.sort();
    ordered.dedup();
    // Reserve canonical names that are already wire-safe before sanitizing any
    // dotted name. This guarantees a persisted canonical reference remains an
    // exact reference on follow-up instead of becoming another tool's alias.
    for canonical in &ordered {
        if portable_tool_name(canonical) == canonical.as_str() {
            map.insert((*canonical).clone(), (*canonical).clone());
        }
    }
    for canonical in ordered {
        if portable_tool_name(canonical) == canonical.as_str() {
            continue;
        }
        let mut wire = portable_tool_name(canonical);
        while map.contains_key(&wire) {
            wire.push('_');
        }
        map.insert(wire, canonical.clone());
    }
    map
}

/// Why a model-supplied tool reference could not be made canonical.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ToolNameError {
    #[error("unknown tool name '{name}'; use a canonical dotted tool name")]
    Unknown { name: String },
    #[error(
        "tool alias '{name}' is ambiguous ({candidates:?}); use one of the canonical dotted names"
    )]
    Ambiguous {
        name: String,
        candidates: Vec<String>,
    },
}

/// Resolve model-supplied references against the canonical tool catalog.
///
/// Exact canonical names win. A provider-facing alias such as `web_search` is
/// accepted only when it identifies exactly one canonical name. Unknown or
/// ambiguous names are errors rather than silently removing a capability from
/// a delegated task. The result is canonical and de-duplicated in request order.
pub fn canonical_tool_names(
    requested: &[String],
    available: &[String],
) -> Result<Vec<String>, ToolNameError> {
    let wire_names = portable_tool_name_map(available);
    let mut resolved = Vec::with_capacity(requested.len());
    for name in requested {
        let exact = available.iter().find(|candidate| *candidate == name);
        let wire = wire_names.get(name);
        let canonical = match (exact, wire) {
            // The spelling is simultaneously a canonical identifier and the
            // provider-visible alias of a different tool. Guessing here can
            // grant the wrong capability, so require the collision-suffixed
            // wire spelling for the latter instead.
            (Some(exact), Some(wire)) if exact != wire => {
                let mut candidates = vec![(*exact).clone(), wire.clone()];
                candidates.sort();
                candidates.dedup();
                return Err(ToolNameError::Ambiguous {
                    name: name.clone(),
                    candidates,
                });
            }
            (Some(exact), _) => (*exact).clone(),
            (None, Some(wire)) => wire.clone(),
            (None, None) => return Err(ToolNameError::Unknown { name: name.clone() }),
        };
        if !resolved.contains(&canonical) {
            resolved.push(canonical);
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tool_name_tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn nested_tool_references_accept_canonical_and_unambiguous_wire_names() {
        let available = names(&["fs.read", "web.fetch", "web.search"]);
        assert_eq!(
            canonical_tool_names(&names(&["web_search", "fs.read", "web_search"]), &available)
                .unwrap(),
            names(&["web.search", "fs.read"])
        );
    }

    #[test]
    fn collision_suffixes_match_the_provider_wire_map() {
        let available = names(&["a.b", "a:b"]);
        let reversed = names(&["a:b", "a.b"]);
        assert_eq!(
            canonical_tool_names(&names(&["a_b", "a_b_"]), &available).unwrap(),
            available
        );
        assert_eq!(
            portable_tool_name_map(&available),
            portable_tool_name_map(&reversed),
            "collision aliases must not depend on registry iteration order"
        );
    }

    #[test]
    fn wire_safe_canonical_names_are_reserved_before_sanitized_aliases() {
        let available = names(&["a.b", "a_b"]);
        let normalized = canonical_tool_names(&names(&["a_b", "a_b_"]), &available).unwrap();
        assert_eq!(
            normalized,
            names(&["a_b", "a.b"]),
            "canonical replay and the provider's collision alias both remain usable"
        );
        assert_eq!(
            canonical_tool_names(&normalized, &available).unwrap(),
            normalized,
            "persisted canonical names must survive follow-up normalization"
        );
    }

    #[test]
    fn unknown_nested_tool_references_are_errors() {
        assert!(matches!(
            canonical_tool_names(&names(&["web_search"]), &names(&["fs.read"])),
            Err(ToolNameError::Unknown { name }) if name == "web_search"
        ));
    }
}

/// A model-proposed tool call.
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

/// Model pricing in USD per million tokens. `indicative` marks a list price
/// that may differ from the route's actual billing.
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

/// The canonical streaming unit emitted by any provider.
#[derive(Debug, Clone)]
pub enum Block {
    Text(String),
    ToolIntent(ToolIntent),
    /// A tool-call name and optional target available before its arguments finish.
    ToolStarted {
        name: String,
        target: Option<String>,
    },
    /// End-of-response token accounting (from the provider's `usage`).
    Usage(Usage),
    /// Reasoning delta shown live but excluded from subsequent-turn history.
    Reasoning(String),
    /// Canonical assistant message preserving ordered parts and provider state.
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

/// Structured result of dispatching an intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub intent_id: String,
    pub status: ObsStatus,
    pub payload: serde_json::Value,
    /// Provenance for relayed content; the kernel applies the weaker trust label.
    pub relayed_trust: Option<TrustLabel>,
    /// The command failed because the sandbox denied network; the kernel may
    /// offer a network grant and retry.
    #[serde(default)]
    pub net_denied: bool,
}

impl Observation {
    pub fn ok(intent_id: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            intent_id: intent_id.into(),
            status: ObsStatus::Ok,
            payload,
            relayed_trust: None,
            net_denied: false,
        }
    }
    pub fn denial(intent_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            intent_id: intent_id.into(),
            status: ObsStatus::Denied,
            payload: serde_json::json!({ "reason": reason.into() }),
            relayed_trust: None,
            net_denied: false,
        }
    }
    pub fn error(intent_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            intent_id: intent_id.into(),
            status: ObsStatus::Error,
            payload: serde_json::json!({ "error": message.into() }),
            relayed_trust: None,
            net_denied: false,
        }
    }
    pub fn relaying(mut self, trust: TrustLabel) -> Self {
        self.relayed_trust = Some(trust);
        self
    }
    pub fn net_denied(mut self) -> Self {
        self.net_denied = true;
        self
    }
}

/// Authorization outcome from the policy engine.
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

/// The output of the context compiler and input to a provider.
#[derive(Debug, Clone)]
pub struct CompiledContext {
    pub model: String,
    pub messages: Vec<Message>,
    /// Exact ordered history when available.
    pub ordered: Option<Vec<ModelMessage>>,
    pub tools: Vec<ToolSpec>,
}

impl CompiledContext {
    /// Return ordered history, deriving it from flat messages when absent.
    pub fn ordered_messages(&self) -> Vec<ModelMessage> {
        self.ordered
            .clone()
            .unwrap_or_else(|| self.messages.iter().map(Message::ordered).collect())
    }
}

/// Controls escalation of otherwise-allowed actions; it never weakens a
/// `Human` or `Deny` decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutonomyLevel {
    /// Read-only investigation. Mutating/unknown tools cannot be approved.
    Plan,
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
            AutonomyLevel::Plan => "plan",
            AutonomyLevel::Careful => "careful",
            AutonomyLevel::Normal => "normal",
            AutonomyLevel::Yolo => "yolo",
        }
    }
    /// Strict parsing for user configuration; typos must be visible.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "plan" => Ok(Self::Plan),
            "careful" => Ok(Self::Careful),
            "normal" => Ok(Self::Normal),
            "yolo" => Ok(Self::Yolo),
            _ => Err(format!(
                "unknown mode '{s}'; choose plan, careful, normal, or yolo"
            )),
        }
    }
    /// Parse a level id; unknown → `Careful` (the safe default).
    pub fn from_id(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "plan" => AutonomyLevel::Plan,
            "normal" => AutonomyLevel::Normal,
            "yolo" => AutonomyLevel::Yolo,
            _ => AutonomyLevel::Careful,
        }
    }
}

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
