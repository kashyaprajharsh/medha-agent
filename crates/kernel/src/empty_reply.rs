//! What to do when a model replies with no words and no action.

use std::time::Duration;

/// How often the same request is sent again after an empty reply.
pub(crate) const TRIES: u32 = 3;

/// One try costing this much or more is sent again only once.
pub(crate) const COSTLY_USD: f64 = 0.25;

/// Said to the model, once, when it answers its tools' results with nothing.
pub(crate) const NUDGE: &str = "You just executed tool calls but returned an empty response. \
    Please process the tool results above and continue with the task.";

/// Said to the person when every try came back empty.
pub(crate) const GAVE_UP: &str = "The model kept answering with nothing: no text and no \
    action. Send your message again, or switch to another model.";

/// Tools that tidy up after an answer. Text followed only by these, and then an
/// empty reply, is a finished answer and not a model that lost its way.
pub(crate) fn tidies_up(tool: &str) -> bool {
    tool == "memory"
        || tool.starts_with("memory.")
        || matches!(tool, "update_plan" | "skill.save" | "sessions.search")
}

/// The wait before try `round` (from 1): 5, 10, then 20 seconds, plus up to half again.
pub(crate) fn wait(round: u32, spread: u64) -> Duration {
    let base = (5u64 << round.saturating_sub(1).min(4)).min(60) * 1_000;
    Duration::from_millis(base + spread % (base / 2 + 1))
}
