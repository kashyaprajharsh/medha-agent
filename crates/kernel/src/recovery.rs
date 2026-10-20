//! What Medha does about a failed model call: one row per kind of failure.

use crate::provider::{ProviderError, ProviderFailure};

/// What is tried on the same model before anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum First {
    /// Shorten the chat and send it again.
    Compact,
    /// Ask for less output and send it again.
    LessOutput,
    /// Send images smaller, then fewer.
    LessImage,
    /// Send it once without the image, to learn whether the image was why.
    WithoutImage,
    /// Tell the model what was wrong with its tool call.
    Correct,
    /// Keep what was written and ask for the rest, or for the call with more room.
    Continue,
    /// Wait, then send it again.
    Retry,
    /// Open the stream again, fewer times than a retry.
    Reconnect,
    /// Send it once more with the server's own reasoning setting, not ours.
    DefaultReasoning,
    /// Send it once more with tool descriptions free of pattern rules.
    PlainSchemas,
    /// Send it once more without earlier reasoning the server could not open.
    StaleReasoning,
    /// Nothing: the same request would get the same answer.
    Nothing,
}

/// One row of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recovery {
    pub first: First,
    /// Said when a model the user listed takes over; `None` when another
    /// model would be sent the same fault.
    pub stand_in: Option<&'static str>,
    /// With another model listed, how many retries come before it takes over;
    /// `None` is every retry. A rate limit is not waited out at all.
    pub hand_over_after: Option<u32>,
    /// A limit that will not lift at once: the model is rested, not tried next message.
    pub rests: bool,
    /// The server is only away for now: once retries and other models are used
    /// up, it is waited for. A stream that goes silent keeps its own, smaller limit.
    pub comes_back: bool,
    /// The most tries this failure is ever given, the first one counted;
    /// `None` is as many as any failure gets.
    pub tries: Option<u32>,
}

/// How many times a server that is only away for now is waited for.
pub const WAITS: u32 = 5;

/// The wait before try `round` (from 1) for a server that is away: what it
/// asked for, up to two minutes, else 15, 30, then 60 seconds, plus up to a fifth.
pub fn wait_for_server(
    round: u32,
    asked: Option<std::time::Duration>,
    spread: u64,
) -> std::time::Duration {
    use std::time::Duration;
    if let Some(asked) = asked {
        return asked.min(Duration::from_secs(120));
    }
    let base = (15u64 << round.saturating_sub(1).min(4)).min(60) * 1_000;
    Duration::from_millis(base + spread % (base / 5 + 1))
}

/// How long a model that failed is left alone once another has taken over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rest {
    /// It is tried again on the next message.
    NextMessage,
    /// For the wait its server stated, or a minute when it stated none.
    Limited(Option<std::time::Duration>),
}

impl ProviderError {
    /// The row for this failure.
    pub fn recovery(&self) -> Recovery {
        let kind = self.classify();
        // What a refusal with no other recovery says it is about.
        let refusal = match (kind, self) {
            (
                ProviderFailure::Fatal,
                ProviderError::Status(_, said) | ProviderError::Response(said),
            ) => said.to_ascii_lowercase(),
            _ => String::new(),
        };
        let refused = "its server refused the request";
        let (first, stand_in) = match (kind, self) {
            _ if crate::rejections::stale_reasoning(&refusal) => {
                (First::StaleReasoning, Some(refused))
            }
            _ if crate::rejections::reasoning_setting(&refusal) => {
                (First::DefaultReasoning, Some(refused))
            }
            _ if crate::rejections::tool_grammar(&refusal) => (First::PlainSchemas, Some(refused)),
            _ if crate::rejections::content_policy(&refusal) => {
                (First::Nothing, Some("it declined to answer"))
            }
            _ if crate::rejections::blocked_on_the_way(&refusal) => (
                First::Nothing,
                Some("something in front of its server blocked the request"),
            ),
            _ if crate::rejections::data_rules(&refusal) => (
                First::Nothing,
                Some("the account's data rules leave no provider for it"),
            ),
            (ProviderFailure::InputContextOverflow { .. }, _) => (First::Compact, None),
            (ProviderFailure::OutputLimit { .. }, _) => (First::LessOutput, None),
            (ProviderFailure::PayloadTooLarge | ProviderFailure::ImageUnfit, _) => {
                (First::LessImage, None)
            }
            (ProviderFailure::UnsupportedImage, _) => (First::WithoutImage, None),
            (ProviderFailure::InvalidToolCall, _) | (_, ProviderError::InvalidToolCall(_)) => {
                (First::Correct, None)
            }
            (_, ProviderError::Truncated) => (First::Continue, None),
            (_, ProviderError::StreamStalled { .. }) => {
                (First::Reconnect, Some("it stopped sending"))
            }
            (
                _,
                ProviderError::NotSent(_) | ProviderError::Transport(_) | ProviderError::Stream(_),
            ) => (First::Retry, Some("its server could not be reached")),
            (_, ProviderError::Garbled(_)) => (First::Retry, Some("its reply could not be read")),
            (_, ProviderError::ResponseTimeout { .. } | ProviderError::ProgressTimeout { .. }) => {
                (First::Nothing, Some("it did not answer in time"))
            }
            // The plan's own limits and rules: a person decides on another model, so none stands in.
            (ProviderFailure::Transient, ProviderError::ChatGptPlan { .. }) => (First::Retry, None),
            (_, ProviderError::ChatGptPlan { .. }) => (First::Nothing, None),
            (_, ProviderError::Refused(_)) => (First::Nothing, Some("it declined to answer")),
            (_, ProviderError::Certificate(_)) => (
                First::Nothing,
                Some("its security certificate could not be verified"),
            ),
            (_, ProviderError::Decode(_)) => (First::Nothing, Some("its reply could not be read")),
            // An error sent inside a normal reply is retried only when it says to.
            (ProviderFailure::Transient, ProviderError::Response(_)) => {
                (First::Retry, Some("its server kept failing"))
            }
            (_, ProviderError::Response(_)) => {
                (First::Nothing, Some("its server answered with an error"))
            }
            (kind, ProviderError::Status(status, _) | ProviderError::Throttled { status, .. }) => {
                let first = match kind {
                    ProviderFailure::Transient => First::Retry,
                    _ => First::Nothing,
                };
                let why = match status {
                    401 | 403 => "its key was refused",
                    402 => "its account has no credit",
                    404 => "the model was not found",
                    429 => "it is still rate limited",
                    500..=599 => "its server kept failing",
                    _ => "its server refused the request",
                };
                (first, Some(why))
            }
        };
        let status = match self {
            ProviderError::Status(status, _) | ProviderError::Throttled { status, .. } => *status,
            _ => 0,
        };
        let hand_over_after = match (status, self) {
            (429, _) => Some(0),
            (503 | 529, _)
            | (
                _,
                ProviderError::NotSent(_) | ProviderError::Transport(_) | ProviderError::Stream(_),
            ) => Some(2),
            _ => None,
        };
        // A reply that arrives unreadable is not a server that is away.
        let garbled = matches!(self, ProviderError::Garbled(_));
        let comes_back = first == First::Retry && status != 429 && !garbled;
        Recovery {
            first,
            stand_in,
            hand_over_after: hand_over_after.filter(|_| stand_in.is_some()),
            rests: matches!(status, 402 | 429) && stand_in.is_some(),
            comes_back,
            tries: garbled.then_some(3),
        }
    }

    /// How long this failure's model is left alone after another takes over.
    pub fn rest(&self) -> Rest {
        match self.recovery().rests {
            true => Rest::Limited(self.retry_after()),
            false => Rest::NextMessage,
        }
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
