//! What to do when a model's reply is cut off at its output limit.

/// How many cut-offs in one turn end it: a text reply is continued before each
/// but the last, and a tool call is retried this many times.
pub(crate) const ROUNDS: u32 = 4;

/// A prompt this close to the model's window leaves no room to continue into.
pub(crate) const HEADROOM: u64 = 512;

/// The output allowance assumed when neither the profile nor the request names one.
const ASSUMED: u64 = 4_096;

/// Said to the model after its text reply was cut off; never shown as the user's.
pub(crate) const CONTINUE: &str = "[System: Your previous response was truncated by the output \
    length limit. Continue exactly where you left off. Do not restart or repeat prior text. \
    Finish the answer directly.]";

/// Said to the person when a tool call stayed cut off after every retry.
pub(crate) const CALL_STAYED_CUT: &str = "The model's reply was cut off before it finished (it \
    hit its output length limit), so the incomplete action was not run. Nothing was changed. \
    Send `continue`, ask for the work in smaller steps, or raise max_output_tokens for this model.";

/// Said when a reply reached its limit as one piece of text repeated over and over.
pub(crate) const REPEATING: &str = "The model fell into repeating the same text until it hit \
    its output limit, so it was not asked to continue: that would only bring more of it. The \
    reply was discarded. Send your message again, or switch to another model.";

/// Said when the limit was reached before any visible text or tool call.
pub(crate) const NOTHING_WRITTEN: &str = "The model reached its output limit before writing \
    anything: its reasoning used the whole allowance. Raise max_output_tokens for this model, \
    or lower its reasoning effort.";

/// The most to ask for in one tool call after a stream died under a larger one.
const CALL_SIZE: u64 = 8_000;

/// Said to the model after its stream kept dying while it wrote a tool call.
/// The size named is never more than half of what it can write in one reply.
pub(crate) fn split_advice(tools: &[String], allowance: Option<u64>) -> String {
    let size = allowance.map_or(CALL_SIZE, |allowance| CALL_SIZE.min(allowance / 2).max(1));
    let named: Vec<&str> = tools.iter().take(3).map(String::as_str).collect();
    format!(
        "[System: Your previous tool call ({}) was too large and the stream timed out before \
         it could be delivered. Do NOT retry the same tool call with the same large content. \
         Instead, break the content into multiple smaller tool calls (for example several \
         edits, or smaller files). Each tool call's arguments must be under ~{size} tokens to \
         avoid stream timeouts. The cut was a transport interruption, not a capability \
         change: your tools remain fully available.]",
        named.join(", ")
    )
}

/// Said to the model after its stream died while it was writing text.
pub(crate) const RESUME: &str = "[System: The previous response was cut off by a network error \
    mid-stream. That was a transport interruption, NOT a change in your capabilities. Your \
    tools are still fully available; call them as normal and ignore any earlier claim that you \
    lack tool access. Continue the task from where you left off. Do not restart or repeat \
    prior text.]";

/// Why the model is asked to go on after a reply that stopped early.
pub(crate) enum GoOn {
    /// Cut off at the output limit; holds the size of the prompt that was sent.
    Cut(Option<u64>),
    /// The stream died while text was being written; holds the same.
    Dropped(Option<u64>),
    /// The stream died while these tool calls were being written.
    Stalled(Vec<String>),
}

/// What a reply had produced when it stopped early.
#[derive(Debug, Default)]
pub(crate) struct CutOff {
    pub text: String,
    pub tool_call: bool,
    /// Names of the tool calls that had started.
    pub tools: Vec<String>,
}

/// Output allowance for retry `round` (from 1) of a cut-off tool call: the
/// configured allowance doubled each round, never below what was just sent,
/// at most the larger of 32,768 and twice that, and never above the model's own.
pub(crate) fn more_room(
    configured: Option<u64>,
    sent: Option<u64>,
    round: u32,
    model_limit: Option<u64>,
) -> u64 {
    let base = configured.or(sent).unwrap_or(ASSUMED);
    let sent = sent.unwrap_or(base);
    if model_limit.is_some_and(|limit| sent >= limit) {
        return sent;
    }
    let doubled = base.saturating_mul(1u64 << round.min(32)).max(sent);
    let room = doubled.min(sent.saturating_mul(2).max(32_768));
    model_limit.map_or(room, |limit| room.min(limit))
}

#[cfg(test)]
#[path = "cut_off_tests.rs"]
mod tests;
