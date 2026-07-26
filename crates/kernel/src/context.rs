//! The context-engine interface (§4.3). Each turn the kernel compiles the
//! outbound model context *fresh* from the full message history, delegating to
//! a `ContextEngine` that compacts it to fit the model's window. The engine
//! lives in its own crate (P8); the kernel knows only this trait. The full
//! history is retained by the kernel/log — compaction shrinks the *view sent to
//! the model*, never the truth (P3).

use crate::provider::InputTokenCount;
use crate::types::{Message, ToolSpec};
use async_trait::async_trait;
use std::path::Path;

pub struct CompileResult {
    /// The (possibly compacted) messages to send to the provider this turn.
    pub messages: Vec<Message>,
    /// Whether compaction actually fired (for logging a `context.compaction`).
    pub compacted: bool,
    /// True if a summarize pass ran (Full); false for a prune-only pass.
    pub summarized: bool,
    pub before_tokens: u32,
    pub after_tokens: u32,
    /// True if, even after the engine's best compaction effort, the result is
    /// still projected to exceed the hard safety ceiling (§4.3 emergency_ratio).
    /// The kernel must not send this turn — send would risk a provider
    /// context-length-exceeded error rather than a controlled, graceful stop.
    pub overflow: bool,
    /// Summary text produced by a Full compaction, for the kernel to persist in
    /// the `context.compaction` event so resume/replay reconstructs the working
    /// set. `None` for prune-only or no-op passes.
    pub summary: Option<String>,
}

#[async_trait]
pub trait ContextEngine: Send + Sync {
    /// Real usage from the last response — authoritative; includes tool defs.
    fn update_usage(&self, _prompt_tokens: u32, _total_tokens: u32) {}

    /// Clear any count associated with the previous request candidate. The
    /// kernel calls this before preparing the next exact body, preventing stale
    /// counts from authorizing a changed request.
    fn clear_preflight(&self) {}

    /// Count of the complete prepared request, including tools and protocol
    /// lowering. The request fingerprint is retained by production engines for
    /// diagnostics and anti-staleness checks.
    fn update_preflight(&self, _count: &InputTokenCount) {}

    /// The provider rejected the current input. Force one bounded compaction
    /// pass without fabricating or permanently lowering a model limit.
    fn force_next_compaction(&self) {}

    /// Note the session's tool set so tool-def overhead is sized once (P1-9).
    fn note_tools(&self, _tools: &[ToolSpec]) {}

    /// Compile outbound context from the working history. `max_input_tokens`
    /// is the resolved input-only allowance for this request. `None` means the
    /// allowance is unknown, so proactive compaction must not guess one.
    async fn compile(&self, messages: &[Message], max_input_tokens: Option<u32>) -> CompileResult;

    /// An engine configured the same way but with no conversation state.
    ///
    /// Token counts, compaction latches and the last summary all describe *one*
    /// conversation. A concurrent sub-agent sharing them reads another's usage
    /// as its own and can be handed another's summary; returning `None` keeps
    /// the shared engine for engines that hold no such state.
    fn fork(&self) -> Option<std::sync::Arc<dyn ContextEngine>> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredContext {
    pub path: String,
    pub content: String,
    pub blocked: bool,
}

#[async_trait]
pub trait ProgressiveContext: Send + Sync {
    async fn discover(&self, touched_path: &Path) -> Option<DiscoveredContext>;
}
