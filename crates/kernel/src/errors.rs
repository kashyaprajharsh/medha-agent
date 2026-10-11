#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("provider error: {0}")]
    Provider(String),

    #[error(transparent)]
    InvalidToolCall(crate::provider::InvalidToolCall),

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

    /// An automatic native image attempt was rejected before any output. The
    /// caller can rebuild the canonical request through auxiliary vision once.
    #[error("provider does not support image input")]
    UnsupportedImage,

    /// A request with an image was refused without saying why; the caller may
    /// try once without it. Holds the server's words, reported if that fails too.
    #[error("{0}")]
    ImageSuspected(String),

    /// The server named an image as too large or unreadable; the caller may send
    /// it smaller or leave it out. Holds the server's words.
    #[error("{0}")]
    ImageUnfit(String),
}
