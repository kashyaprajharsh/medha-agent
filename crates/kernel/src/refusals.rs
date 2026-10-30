//! Repeats that go nowhere: a refusal seen again, and the same call failing again.

use crate::types::{ObsStatus, Observation, ToolIntent};
use std::collections::HashMap;

const TWICE: &str = "This was refused twice for the same reason. Do not try another \
    variation of it: change the approach, or tell the person what is blocked and why.";
const THRICE: &str = "Refused three times for the same reason. Stop using this tool for \
    it and tell the person what was blocked, why, and what they can do about it.";
const FAILED_TWICE: &str = "This exact call failed twice. Do not repeat it unchanged: read \
    the error, then change the arguments or the approach.";
const LAST_TRY: &str = "This exact call has now failed four times. Medha will not run it \
    again until the person's next message: change the approach, or tell the person what is \
    blocking it.";
const NOT_RUN: &str = "Not run: this exact call already failed four times since the person's \
    last message. Change the approach, or tell the person what is blocking it and why.";
/// The call that does not run; the warning comes at the second failure.
const BLOCK_AT: usize = 5;
const NOT_RUN_CODE: &str = "repeated_failure";

/// Refusals by tool and reason, and failures by exact call, counted across one run.
#[derive(Default)]
pub(crate) struct Refusals {
    reasons: HashMap<String, usize>,
    failures: HashMap<String, usize>,
}

/// One call by tool and arguments; JSON objects keep their keys sorted, so equal calls match.
fn signature(intent: &ToolIntent) -> String {
    format!("{}\n{}", intent.tool, intent.args)
}

impl Refusals {
    /// A call that already failed `BLOCK_AT - 1` times settles without running.
    pub(crate) fn blocked(&self, intent: &ToolIntent) -> Option<Observation> {
        let failures = self.failures.get(&signature(intent)).copied().unwrap_or(0);
        (failures + 1 >= BLOCK_AT).then(|| {
            let mut observation = Observation::denial(&intent.id, NOT_RUN);
            observation.payload["error_code"] = serde_json::json!(NOT_RUN_CODE);
            observation
        })
    }

    /// Counts the result and adds advice to it; returns a notice for the person when there is one.
    pub(crate) fn note(
        &mut self,
        intent: &ToolIntent,
        failed: bool,
        observation: &mut Observation,
    ) -> Option<String> {
        if observation.payload["error_code"] == NOT_RUN_CODE {
            return Some(format!(
                "Medha did not run {} again: the same call failed {} times.",
                intent.tool,
                BLOCK_AT - 1
            ));
        }
        let call = signature(intent);
        let advice = if failed {
            let count = self.failures.entry(call).or_default();
            *count += 1;
            match *count {
                2 => Some(FAILED_TWICE),
                n if n + 1 == BLOCK_AT => Some(LAST_TRY),
                _ => None,
            }
        } else {
            if observation.status == ObsStatus::Ok {
                self.failures.remove(&call);
            }
            None
        };
        let notice = self.refused(&intent.tool, observation);
        let advice = advice.or(notice.as_ref().map(|(note, _)| *note));
        if let Some(note) = advice {
            observation.payload["repeated"] = serde_json::json!(note);
            if let Some(text) = &mut observation.model_text {
                text.push_str("\n\n");
                text.push_str(note);
            }
        }
        notice.and_then(|(_, notice)| notice)
    }

    /// A policy or sandbox refusal seen again for the same reason, whatever the arguments.
    fn refused(
        &mut self,
        tool: &str,
        observation: &Observation,
    ) -> Option<(&'static str, Option<String>)> {
        if observation.status != ObsStatus::Denied {
            return None;
        }
        let payload = &observation.payload;
        let reason = payload
            .get("reason")
            .or_else(|| payload.get("error"))
            .and_then(serde_json::Value::as_str)?
            .to_owned();
        let count = self.reasons.entry(format!("{tool}\n{reason}")).or_default();
        *count += 1;
        match *count {
            1 => None,
            2 => Some((TWICE, None)),
            _ => Some((
                THRICE,
                (*count == 3).then(|| {
                    format!(
                        "{tool} was refused three times ({reason}); the model was told to stop."
                    )
                }),
            )),
        }
    }
}

#[cfg(test)]
#[path = "refusals_tests.rs"]
mod tests;
