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
