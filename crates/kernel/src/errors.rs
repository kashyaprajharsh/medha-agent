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
    /// harder instead of surfacing a fatal error.
    #[error("provider context-length exceeded")]
    ContextOverflow { reported_limit: Option<u64> },
}
