//! The context-engine interface (§4.3). Each turn the kernel compiles the
//! outbound model context *fresh* from the full message history, delegating to
//! a `ContextEngine` that compacts it to fit the model's window. The engine
//! lives in its own crate (P8); the kernel knows only this trait. The full
//! history is retained by the kernel/log — compaction shrinks the *view sent to
//! the model*, never the truth (P3).

use crate::types::Message;
use async_trait::async_trait;

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
    /// Record real token usage from the provider's last response. This is the
    /// authoritative token count (never an estimate); the engine uses it to
    /// drive compaction decisions instead of guessing.
    fn update_usage(&self, _prompt_tokens: u32, _total_tokens: u32) {}

    /// Compile outbound context from the working history. `max_ctx` is the
    /// model's window (`None` = unknown → no compaction; never guess a window).
    async fn compile(&self, messages: &[Message], max_ctx: Option<u32>) -> CompileResult;
}
