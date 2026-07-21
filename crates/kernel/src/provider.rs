//! The one interface the kernel sees for any model (§4.4). Open-first: the
//! OpenAI-compatible adapter is the baseline impl; Anthropic/Gemini are opt-in
//! native upgrades. All translate to/from the canonical `Block`.

use crate::types::{Block, CompiledContext};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Stable wire contracts supported by Medha. A protocol is a real HTTP/event
/// contract, not a vendor or deployment name: vLLM and compatible gateways use
/// `OpenAiChat`, while native APIs use their own variants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    #[default]
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
    GeminiInteractions,
}

impl Protocol {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "open-ai-chat",
            Self::OpenAiResponses => "open-ai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GeminiInteractions => "gemini-interactions",
        }
    }
}

impl std::str::FromStr for Protocol {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "open-ai-chat" | "openai-chat" => Ok(Self::OpenAiChat),
            "open-ai-responses" | "openai-responses" => Ok(Self::OpenAiResponses),
            "anthropic-messages" | "anthropic" => Ok(Self::AnthropicMessages),
            "gemini-interactions" | "gemini" => Ok(Self::GeminiInteractions),
            other => Err(format!("unsupported protocol '{other}'")),
        }
    }
}

/// How trustworthy a preflight input-token count is. Estimates must remain
/// visibly distinct from values produced by the provider's inference pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenCountQuality {
    Authoritative,
    ProviderEstimate,
    LocalEstimate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputTokenCount {
    pub tokens: u64,
    pub quality: TokenCountQuality,
    /// Hash of the exact prepared body this count describes. It prevents a
    /// count from being reused after tools, messages, reasoning, or model state
    /// changes the request.
    pub request_fingerprint: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenCountError {
    #[error("token-count transport error: {0}")]
    Transport(String),
    #[error("token-count endpoint returned status {0}: {1}")]
    Status(u16, String),
    #[error("token-count response decode error: {0}")]
    Decode(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelLimits {
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_combined_tokens: Option<u64>,
}

impl ModelLimits {
    /// Maximum input for this request. A combined window reserves only the
    /// explicitly requested output allowance; it does not invent a percentage.
    /// Without a requested output cap, a combined limit cannot safely be
    /// converted into an input-only allowance and therefore remains unknown.
    pub fn input_allowance(self, requested_output: Option<u64>) -> Option<u64> {
        let combined = self
            .max_combined_tokens
            .zip(requested_output)
            .map(|(limit, output)| limit.saturating_sub(output));
        match (self.max_input_tokens, combined) {
            (Some(input), Some(combined)) => Some(input.min(combined)),
            (Some(input), None) => Some(input),
            (None, Some(combined)) => Some(combined),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod model_limit_tests {
    use super::ModelLimits;

    #[test]
    fn combined_limit_requires_an_explicit_output_allowance() {
        let limits = ModelLimits {
            max_input_tokens: None,
            max_output_tokens: Some(8_000),
            max_combined_tokens: Some(32_000),
        };
        assert_eq!(limits.input_allowance(Some(4_000)), Some(28_000));
        assert_eq!(limits.input_allowance(None), None);
    }

    #[test]
    fn independent_input_limit_does_not_require_an_output_allowance() {
        let limits = ModelLimits {
            max_input_tokens: Some(24_000),
            max_output_tokens: None,
            max_combined_tokens: None,
        };
        assert_eq!(limits.input_allowance(None), Some(24_000));
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenAccountingMode {
    #[default]
    Adaptive,
    Strict,
}

/// The exact provider request selected for a model call. The kernel treats the
/// body as opaque: it may fingerprint and pass it back to the provider, but all
/// protocol-specific construction remains in the adapter.
#[derive(Debug, Clone)]
pub struct PreparedModelRequest {
    pub protocol: Protocol,
    pub model: String,
    pub body: serde_json::Value,
    pub context: CompiledContext,
    pub request_fingerprint: String,
}

impl PreparedModelRequest {
    pub fn new(
        protocol: Protocol,
        model: impl Into<String>,
        body: serde_json::Value,
        context: CompiledContext,
    ) -> Self {
        let model = model.into();
        let request_fingerprint = request_fingerprint(protocol, &model, &body);
        Self {
            protocol,
            model,
            body,
            context,
            request_fingerprint,
        }
    }

    /// Provider adapters use this when applying a bounded retry adjustment,
    /// such as lowering an output cap. A changed body always gets a new hash.
    pub fn with_body(&self, body: serde_json::Value) -> Self {
        Self::new(
            self.protocol,
            self.model.clone(),
            body,
            self.context.clone(),
        )
    }
}

fn request_fingerprint(protocol: Protocol, model: &str, body: &serde_json::Value) -> String {
    let mut hash = Sha256::new();
    hash.update(protocol.as_str().as_bytes());
    hash.update([0]);
    hash.update(model.as_bytes());
    hash.update([0]);
    // Serialization of Value is deterministic with serde_json's default sorted
    // map representation. Failure is practically unreachable; hash a marker if
    // a future custom serializer makes it fallible rather than panicking.
    match serde_json::to_vec(body) {
        Ok(bytes) => hash.update(bytes),
        Err(error) => hash.update(error.to_string().as_bytes()),
    }
    format!("{:x}", hash.finalize())
}

/// How a provider produces schema-valid tool intents (§4.4). The kernel always
/// receives a valid `ToolIntent` or a structured parse failure, whichever rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStrategy {
    /// Server + model support native tool calls well.
    Native,
    /// Constrained/guided decoding (grammar / JSON-schema) forces valid intents
    /// from models with weak native support — what makes P1 hold on open models.
    Guided,
    /// Structured JSON-in-text + parser. Last resort for the weakest endpoints.
    PromptFallback,
}

#[derive(Debug, Clone)]
pub struct ProviderCaps {
    pub vision: bool,
    pub caching: bool,
    /// Context-window size in tokens. `None` = **unknown** — not a guess. The
    /// context compiler must never trust a fabricated number (it sizes
    /// compaction against this, §4.3). Resolved, in order: model-discovery
    /// response (OpenRouter `context_length`, vLLM `max_model_len`, Ollama
    /// `/api/show`) → config/`medha.lock` override → conservative fallback with
    /// a warning. `vision`/`caching` default `false` because that's a safe
    /// *capability* default; an unknown context window is not — hence `Option`.
    pub max_ctx: Option<u32>,
    pub tool_calls: ToolCallStrategy,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("provider returned status {0}: {1}")]
    Status(u16, String),
    #[error("provider stream error: {0}")]
    Stream(String),
}

/// Actionable provider failure classes. Keeping these separate prevents the
/// common but destructive mistake of compacting history for an output-cap or
/// raw HTTP-body-size error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFailure {
    InputContextOverflow { reported_limit: Option<u64> },
    OutputLimit { available_output: Option<u64> },
    PayloadTooLarge,
    Transient,
    Fatal,
}

impl ProviderError {
    /// Transient failures worth retrying with backoff: network/transport drops,
    /// rate limits (429), and server errors (5xx). A context-length 400 is NOT
    /// retryable as-is (retrying sends the same over-long request) — it's handled
    /// separately by compaction; see [`is_context_overflow`](Self::is_context_overflow).
    pub fn is_retryable(&self) -> bool {
        matches!(self.classify(), ProviderFailure::Transient)
    }

    /// The provider rejected the request for exceeding the model's context
    /// window (usually a 400/413 whose message mentions context/token length).
    /// The fix is to compact and retry, not to back off — so this is classified
    /// apart from [`is_retryable`](Self::is_retryable).
    pub fn is_context_overflow(&self) -> bool {
        matches!(
            self.classify(),
            ProviderFailure::InputContextOverflow { .. }
        )
    }

    pub fn classify(&self) -> ProviderFailure {
        match self {
            ProviderError::Transport(_) | ProviderError::Stream(_) => ProviderFailure::Transient,
            ProviderError::Decode(_) => ProviderFailure::Fatal,
            ProviderError::Status(code, message) => {
                if *code == 429 || (500..600).contains(code) {
                    return ProviderFailure::Transient;
                }
                let lower = message.to_ascii_lowercase();
                let output_shaped = (lower.contains("max_tokens")
                    || lower.contains("max output")
                    || lower.contains("output token"))
                    && (lower.contains("too large")
                        || lower.contains("exceed")
                        || lower.contains("maximum")
                        || lower.contains("available"));
                if output_shaped {
                    return ProviderFailure::OutputLimit {
                        available_output: number_after_any(
                            &lower,
                            &["available_tokens", "available tokens", "available output"],
                        ),
                    };
                }

                let input_shaped = lower.contains("context_length_exceeded")
                    || lower.contains("maximum context")
                    || (lower.contains("context")
                        && (lower.contains("length") || lower.contains("window"))
                        && (lower.contains("exceed")
                            || lower.contains("too long")
                            || lower.contains("maximum")))
                    || lower.contains("too many tokens in the prompt")
                    || lower.contains("input tokens exceed");
                if (*code == 400 || *code == 413) && input_shaped {
                    return ProviderFailure::InputContextOverflow {
                        reported_limit: number_after_any(
                            &lower,
                            &[
                                "maximum context length is",
                                "maximum context length:",
                                "max context length:",
                                "context window is",
                                "context window:",
                                "context_length:",
                            ],
                        ),
                    };
                }
                if *code == 413 {
                    return ProviderFailure::PayloadTooLarge;
                }
                ProviderFailure::Fatal
            }
        }
    }
}

fn number_after_any(text: &str, markers: &[&str]) -> Option<u64> {
    markers.iter().find_map(|marker| {
        let tail = text.split_once(marker)?.1.trim_start();
        let digits: String = tail
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '_')
            .filter(|c| c.is_ascii_digit())
            .collect();
        (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
    })
}

#[cfg(test)]
mod error_class_tests {
    use super::{ProviderError, ProviderFailure};

    #[test]
    fn transient_failures_are_retryable() {
        assert!(ProviderError::Transport("connection reset".into()).is_retryable());
        assert!(ProviderError::Status(429, "rate limited".into()).is_retryable());
        assert!(ProviderError::Status(500, "internal".into()).is_retryable());
        assert!(ProviderError::Status(503, "unavailable".into()).is_retryable());
        assert!(ProviderError::Stream("mid-stream cutoff".into()).is_retryable());
    }

    #[test]
    fn client_errors_and_decode_are_not_retryable() {
        assert!(!ProviderError::Status(400, "bad request".into()).is_retryable());
        assert!(!ProviderError::Status(401, "unauthorized".into()).is_retryable());
        assert!(!ProviderError::Status(404, "not found".into()).is_retryable());
        assert!(!ProviderError::Decode("bad json".into()).is_retryable());
    }

    #[test]
    fn context_overflow_is_detected_and_kept_separate_from_retry() {
        for msg in [
            "This model's maximum context length is 128000 tokens",
            "context_length_exceeded",
            "requested tokens exceed the context window",
            "too many tokens in the prompt",
        ] {
            let e = ProviderError::Status(400, msg.into());
            assert!(e.is_context_overflow(), "should flag: {msg}");
            assert!(!e.is_retryable(), "overflow is NOT a plain retry: {msg}");
        }
        // A generic 400 is neither.
        let generic = ProviderError::Status(400, "invalid parameter 'foo'".into());
        assert!(!generic.is_context_overflow());
        // A 429 is retryable but not an overflow.
        assert!(!ProviderError::Status(429, "slow down".into()).is_context_overflow());
    }

    #[test]
    fn failure_classes_keep_output_payload_and_input_recovery_separate() {
        let input = ProviderError::Status(
            400,
            "This model's maximum context length is 128,000 tokens".into(),
        );
        assert_eq!(
            input.classify(),
            ProviderFailure::InputContextOverflow {
                reported_limit: Some(128_000)
            }
        );

        let output = ProviderError::Status(
            400,
            "max_tokens: 32768 exceeds available_tokens: 10000".into(),
        );
        assert_eq!(
            output.classify(),
            ProviderFailure::OutputLimit {
                available_output: Some(10_000)
            }
        );
        assert!(!output.is_context_overflow());

        assert_eq!(
            ProviderError::Status(413, "request body too large".into()).classify(),
            ProviderFailure::PayloadTooLarge
        );
    }
}

/// How hard the model should think before answering. Maps onto whatever knob
/// the adapter's server actually exposes (e.g. vLLM/SGLang's `medium_effort`
/// flag) — canonical here so the kernel/surfaces never see vendor JSON shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

/// Profile/model-level reasoning controls known to be accepted. `Unknown`
/// permits an explicit effort request but surfaces that support is unverified;
/// it is never presented as a confirmed capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningSupport {
    #[default]
    Unknown,
    Unsupported,
    Effort,
}

impl ReasoningSupport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unverified",
            Self::Unsupported => "unsupported",
            Self::Effort => "effort",
        }
    }
}

/// Reasoning/thinking control for subsequent calls (§4.4). `enabled: None` /
/// `effort: None` mean "don't touch the server's own default" — this is a
/// request-side control (distinct from parsing reasoning back out of the
/// response, which is the `Block::Reasoning` / `<think>` path). Config-file
/// and slash-command control both go through this one canonical type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReasoningConfig {
    pub enabled: Option<bool>,
    pub effort: Option<ReasoningEffort>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn capabilities(&self) -> &ProviderCaps;

    /// Context window for the connection currently serving requests. Most
    /// providers are immutable and use their declared capabilities; adapters
    /// that support an explicit between-turn profile switch may override this
    /// with their active connection's value.
    fn context_window(&self) -> Option<u32> {
        self.capabilities().max_ctx
    }

    fn protocol(&self) -> Protocol {
        Protocol::OpenAiChat
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: None,
            max_output_tokens: None,
            max_combined_tokens: self.context_window().map(u64::from),
        }
    }

    /// Output cap requested on each call. `None` means the provider's default;
    /// no made-up reservation is subtracted from a combined context window.
    fn requested_output_tokens(&self) -> Option<u64> {
        None
    }

    /// Learn a concrete limit reported by the provider. Implementations must
    /// ignore guesses; the kernel only calls this with an explicitly parsed
    /// value from the rejected response.
    fn update_context_limit(&self, _tokens: u64) {}

    fn token_accounting_mode(&self) -> TokenAccountingMode {
        TokenAccountingMode::Adaptive
    }

    /// Lower canonical context into the actual request body once. Counting and
    /// generation receive this same value, eliminating parallel request builders.
    fn prepare_request(
        &self,
        ctx: &CompiledContext,
    ) -> Result<PreparedModelRequest, ProviderError> {
        let body = serde_json::json!({
            "model": ctx.model,
            "messages": ctx.messages,
            "tools": ctx.tools,
        });
        Ok(PreparedModelRequest::new(
            self.protocol(),
            ctx.model.clone(),
            body,
            ctx.clone(),
        ))
    }

    /// Stream canonical blocks for one model call. Phase 0 impls may buffer the
    /// response and yield owned blocks; SSE token streaming lands in Phase 1.
    async fn stream(
        &self,
        ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError>;

    /// Send a previously prepared request. Providers with an opaque/wire-level
    /// preparation override this; existing/test providers safely delegate to
    /// their canonical `stream` implementation.
    async fn stream_prepared(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        self.stream(&request.context).await
    }

    /// Apply an output-cap correction reported by the provider. The adapter
    /// owns the wire field; the kernel never inserts vendor JSON keys.
    fn with_output_limit(
        &self,
        _request: &PreparedModelRequest,
        _max_output_tokens: u64,
    ) -> Result<Option<PreparedModelRequest>, ProviderError> {
        Ok(None)
    }

    fn reasoning_support(&self) -> ReasoningSupport {
        ReasoningSupport::Unsupported
    }

    /// Adjust reasoning/thinking behavior for calls made after this returns.
    /// Unsupported controls must fail visibly; a provider may never silently
    /// keep a UI value which it will not lower onto the wire.
    fn set_reasoning(&self, config: ReasoningConfig) -> Result<(), ProviderError> {
        if config == ReasoningConfig::default() {
            Ok(())
        } else {
            Err(ProviderError::Decode(
                "reasoning controls are unsupported by this provider".into(),
            ))
        }
    }

    /// Current reasoning config, for `/think status` and similar UIs.
    fn reasoning(&self) -> ReasoningConfig {
        ReasoningConfig::default()
    }

    /// Toggle SSE streaming for calls made after this returns. With streaming
    /// off the provider makes one blocking request and yields the whole response
    /// as a single batch of blocks — the loop is unchanged, only the arrival
    /// shape differs. Some gateways only populate `reasoning_content` (or
    /// structured output) in the non-streaming response, so this is the escape
    /// hatch to see it. Default no-op for providers without the concept.
    fn set_streaming(&self, _on: bool) {}

    /// Whether SSE streaming is currently on, for status UIs. Default true —
    /// streaming is the norm.
    fn streaming(&self) -> bool {
        true
    }

    /// Count the complete prepared input when this explicit profile declares a
    /// supported counter. Generic compatible endpoints are never blind-probed.
    async fn count_input_tokens(
        &self,
        _request: &PreparedModelRequest,
    ) -> Result<Option<InputTokenCount>, TokenCountError> {
        Ok(None)
    }
}
