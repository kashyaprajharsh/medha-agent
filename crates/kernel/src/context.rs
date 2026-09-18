//! Context compilation from full durable history. Compaction changes only the
//! provider view, not the event log.

use crate::provider::{InputTokenCount, TokenCountQuality};
use crate::types::{Message, ToolSpec, TrustLabel};
use async_trait::async_trait;
use std::future::Future;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// The same input budget used by the compiler and every output surface.
/// Unknown limits remain unknown; estimated counts are explicitly labelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPressure {
    pub input_tokens: u64,
    pub input_limit: Option<u32>,
    pub usable_input_tokens: Option<u32>,
    pub quality: TokenCountQuality,
}

impl ContextPressure {
    pub fn new(input_tokens: u64, input_limit: Option<u32>, quality: TokenCountQuality) -> Self {
        let margin_bps = match quality {
            TokenCountQuality::Authoritative => 0,
            TokenCountQuality::ProviderEstimate => 200,
            TokenCountQuality::LocalEstimate => 1_000,
        };
        Self {
            input_tokens,
            input_limit,
            usable_input_tokens: input_limit
                .map(|limit| limit - (u64::from(limit) * margin_bps / 10_000) as u32),
            quality,
        }
    }

    pub fn percent(self) -> Option<u32> {
        self.usable_input_tokens.map(|limit| {
            (self
                .input_tokens
                .saturating_mul(100)
                .saturating_add(u64::from(limit) / 2)
                / u64::from(limit.max(1)))
            .min(u64::from(u32::MAX)) as u32
        })
    }
}

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
    /// True when the request still exceeds the hard ceiling after compaction.
    pub overflow: bool,
    /// Summary text produced by a Full compaction, retained for diagnostics and
    /// UI notices alongside the event's exact post-compaction snapshot. `None`
    /// for prune-only or no-op passes.
    pub summary: Option<String>,
}

#[async_trait]
pub trait ContextEngine: Send + Sync {
    /// Reset usage calibration when the session, deployment, model or limits
    /// change. This does not discard the conversation or its checkpoints.
    fn begin_request(&self, _scope: &str) {}

    fn pressure(&self) -> Option<ContextPressure> {
        None
    }

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

    /// Note the session's tool set so tool-definition overhead is sized once.
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

    #[test]
    fn pressure_uses_the_compiler_margin_without_integer_overflow() {
        let p = ContextPressure::new(6_480, Some(8_000), TokenCountQuality::LocalEstimate);
        assert_eq!(p.usable_input_tokens, Some(7_200));
        assert_eq!(p.percent(), Some(90));
        let p = ContextPressure::new(0, Some(u32::MAX), TokenCountQuality::LocalEstimate);
        assert_eq!(
            p.usable_input_tokens,
            Some(u32::MAX - (u64::from(u32::MAX) / 10) as u32)
        );
        assert_eq!(
            ContextPressure::new(100, None, TokenCountQuality::LocalEstimate).percent(),
            None
        );
    }

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
