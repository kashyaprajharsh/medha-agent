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

    /// A text reply was cut off at the output limit. Holds what was written,
    /// which the caller keeps and may ask the model to continue.
    #[error("model response was truncated at its output limit")]
    Truncated(String),

    /// The stream died while a text reply was being written. Holds what was
    /// written, which the caller keeps and asks the model to continue from.
    #[error("the model's stream ended before its reply was complete")]
    Dropped(String),

    /// The model call failed for good; the caller may try the next model the
    /// user listed. `shown` is whether part of a reply had reached the screen.
    #[error("{said}")]
    ModelUnavailable {
        /// The model the failed request was sent to.
        model: String,
        why: &'static str,
        said: String,
        shown: bool,
        rest: crate::recovery::Rest,
        /// Set when the server is only away for now: the wait it asked for, if any.
        back: Option<Option<std::time::Duration>>,
    },

    /// The stream kept dying while the model wrote a tool call. Holds the text
    /// before the call, the tools named, and the error if this cannot be recovered.
    #[error("{said}")]
    CallStalled {
        text: String,
        tools: Vec<String>,
        said: String,
    },
}
