//! Error taxonomy. Every failure maps to something the model can reason about
//! (Vol 3 §8). Phase 0 keeps a small set; it grows with the dispatch pipeline.

#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("provider error: {0}")]
    Provider(String),

    #[error("event log error: {0}")]
    Log(String),

    #[error("task interrupted")]
    Interrupted,

    #[error("budget stopped: {}", .0.label())]
    Budget(crate::budgets::BudgetStop),

    /// The provider rejected the request as too long for the model's context
    /// window. Distinct from `Provider` so the loop can respond by compacting
    /// harder and retrying once (P0-6) instead of surfacing a fatal error.
    #[error("provider context-length exceeded")]
    ContextOverflow { reported_limit: Option<u64> },
}
