//! The one interface the kernel sees for any model (§4.4). Open-first: the
//! OpenAI-compatible adapter is the baseline impl; Anthropic/Gemini are opt-in
//! native upgrades. All translate to/from the canonical `Block`.

use crate::types::{Block, CompiledContext, Message};
use async_trait::async_trait;
use futures::stream::BoxStream;

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

impl ProviderError {
    /// Transient failures worth retrying with backoff: network/transport drops,
    /// rate limits (429), and server errors (5xx). A context-length 400 is NOT
    /// retryable as-is (retrying sends the same over-long request) — it's handled
    /// separately by compaction; see [`is_context_overflow`](Self::is_context_overflow).
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Transport(_) => true,
            ProviderError::Status(code, _) => *code == 429 || (500..600).contains(code),
            // A mid-stream cutoff often shows up here; treat as transient too.
            ProviderError::Stream(_) => true,
            ProviderError::Decode(_) => false,
        }
    }

    /// The provider rejected the request for exceeding the model's context
    /// window (usually a 400/413 whose message mentions context/token length).
    /// The fix is to compact and retry, not to back off — so this is classified
    /// apart from [`is_retryable`](Self::is_retryable).
    pub fn is_context_overflow(&self) -> bool {
        let msg = match self {
            ProviderError::Status(code, m) if *code == 400 || *code == 413 => m,
            _ => return false,
        };
        let m = msg.to_lowercase();
        (m.contains("context") && (m.contains("length") || m.contains("window")))
            || m.contains("context_length_exceeded")
            || m.contains("maximum context")
            || m.contains("too many tokens")
            || (m.contains("token") && m.contains("exceed"))
    }
}

#[cfg(test)]
mod error_class_tests {
    use super::ProviderError;

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
}

/// How hard the model should think before answering. Maps onto whatever knob
/// the adapter's server actually exposes (e.g. vLLM/SGLang's `medium_effort`
/// flag) — canonical here so the kernel/surfaces never see vendor JSON shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
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

    /// Stream canonical blocks for one model call. Phase 0 impls may buffer the
    /// response and yield owned blocks; SSE token streaming lands in Phase 1.
    async fn stream(
        &self,
        ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError>;

    /// Adjust reasoning/thinking behavior for calls made after this returns.
    /// Default no-op — providers/models that don't support the concept simply
    /// ignore it rather than erroring.
    fn set_reasoning(&self, _config: ReasoningConfig) {}

    /// Current reasoning config, for `/think status` and similar UIs.
    fn reasoning(&self) -> ReasoningConfig {
        ReasoningConfig::default()
    }

    /// Exact server-side token count for the given prompt messages, when the
    /// host exposes a tokenization route (e.g. vLLM's `/tokenize`, or an
    /// Anthropic-style `/messages/count_tokens`). Returns `None` when no such
    /// route exists or the call fails — the caller then uses its local estimate.
    /// Best-effort and never required: the authoritative post-turn count still
    /// comes from the response `usage`. Providers that can't offer it inherit
    /// this default and simply return `None`.
    async fn count_tokens(&self, _messages: &[Message]) -> Option<u32> {
        None
    }
}
