//! The context-engine interface (§4.3). Each turn the kernel compiles the
//! outbound model context *fresh* from the full message history, delegating to
//! a `ContextEngine` that compacts it to fit the model's window. The engine
//! lives in its own crate (P8); the kernel knows only this trait. The full
//! history is retained by the kernel/log — compaction shrinks the *view sent to
//! the model*, never the truth (P3).

use crate::provider::InputTokenCount;
use crate::types::{Message, ToolSpec, TrustLabel};
use async_trait::async_trait;
use std::future::Future;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ContextCompileError {
    #[error("context compilation was cancelled")]
    Cancelled,
    #[error("context compilation exceeded the task deadline")]
    Deadline,
}

/// Cancellation/deadline authority for work done while compiling a request.
/// The default controlled implementation races the *entire* compiler future,
/// including an LLM summarizer's connection and stream drain.
#[derive(Clone)]
pub struct CompileControl {
    cancel: CancellationToken,
    deadline: Option<tokio::time::Instant>,
}

impl CompileControl {
    pub fn new(cancel: CancellationToken, deadline: Option<tokio::time::Instant>) -> Self {
        Self { cancel, deadline }
    }

    pub fn unlimited() -> Self {
        Self::new(CancellationToken::new(), None)
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }

    pub fn check(&self) -> Result<(), ContextCompileError> {
        if self.cancel.is_cancelled() {
            return Err(ContextCompileError::Cancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            return Err(ContextCompileError::Deadline);
        }
        Ok(())
    }

    pub async fn run<F: Future>(&self, future: F) -> Result<F::Output, ContextCompileError> {
        self.check()?;
        tokio::pin!(future);
        match self.deadline {
            Some(deadline) => {
                tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => Err(ContextCompileError::Cancelled),
                    _ = tokio::time::sleep_until(deadline) => Err(ContextCompileError::Deadline),
                    result = &mut future => Ok(result),
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => Err(ContextCompileError::Cancelled),
                    result = &mut future => Ok(result),
                }
            }
        }
    }
}

pub struct CompileResult {
    /// The (possibly compacted) messages to send to the provider this turn.
    pub messages: Vec<Message>,
    /// Exact provenance for every entry in [`Self::messages`].
    ///
    /// `Some(i)` means this output is the byte-for-byte retained input message
    /// at index `i`; `None` means the compiler generated or rewrote it. The
    /// kernel uses this identity map to retain opaque canonical provider state
    /// without guessing between duplicate legacy messages. Its length must
    /// equal `messages.len()`.
    pub source_indices: Vec<Option<usize>>,
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
    /// Summary text produced by a Full compaction, retained for diagnostics and
    /// UI notices alongside the event's exact post-compaction snapshot. `None`
    /// for prune-only or no-op passes.
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

    /// Compile under the task's cancellation and wall-clock authority.
    ///
    /// Existing stateless/test engines only need to implement `compile`; this
    /// wrapper still makes every await in that future externally cancellable.
    async fn compile_controlled(
        &self,
        messages: &[Message],
        max_input_tokens: Option<u32>,
        control: &CompileControl,
    ) -> Result<CompileResult, ContextCompileError> {
        control.run(self.compile(messages, max_input_tokens)).await
    }

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
    pub trust: TrustLabel,
}

/// A path that the active sandbox has canonicalized and authorized without
/// opening a new prompt. Context discovery must use this pinned spelling for
/// its read, so a symlink/raw-path alias cannot escape the approved roots.
#[derive(Debug, Clone)]
pub struct AuthorizedContextPath {
    pub path: PathBuf,
    pub trust: TrustLabel,
}

#[async_trait]
pub trait ProgressiveContextPathAuthorizer: Send + Sync {
    async fn authorize_context_path(&self, path: &Path) -> Option<AuthorizedContextPath>;
}

#[async_trait]
pub trait ProgressiveContext: Send + Sync {
    async fn discover(&self, touched_path: &Path) -> Option<DiscoveredContext>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct NeverCompile(AtomicBool);

    #[async_trait]
    impl ContextEngine for NeverCompile {
        async fn compile(
            &self,
            _messages: &[Message],
            _max_input_tokens: Option<u32>,
        ) -> CompileResult {
            self.0.store(true, Ordering::SeqCst);
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_pre_cancelled_control_never_polls_the_compiler() {
        let engine = NeverCompile(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = engine
            .compile_controlled(
                &[],
                None,
                &CompileControl::new(cancel, Some(tokio::time::Instant::now())),
            )
            .await;
        assert!(matches!(result, Err(ContextCompileError::Cancelled)));
        assert!(!engine.0.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn compiler_deadline_interrupts_a_hung_future() {
        let engine = NeverCompile(AtomicBool::new(false));
        let result = engine
            .compile_controlled(
                &[],
                None,
                &CompileControl::new(
                    CancellationToken::new(),
                    Some(tokio::time::Instant::now() + std::time::Duration::from_millis(10)),
                ),
            )
            .await;
        assert!(matches!(result, Err(ContextCompileError::Deadline)));
        assert!(engine.0.load(Ordering::SeqCst));
    }
}
