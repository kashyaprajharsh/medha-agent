//! A refusal seen again is a dead end: the result says so instead of letting the model try variations.

use crate::types::{ObsStatus, Observation};
use std::collections::HashMap;

const TWICE: &str = "This was refused twice for the same reason. Do not try another \
    variation of it: change the approach, or tell the person what is blocked and why.";
const THRICE: &str = "Refused three times for the same reason. Stop using this tool for \
    it and tell the person what was blocked, why, and what they can do about it.";

/// Each refusal by tool and reason, counted across one run.
#[derive(Default)]
pub(crate) struct Refusals(HashMap<String, usize>);

impl Refusals {
    /// Marks a repeated refusal; on the third returns a notice for the person.
    pub(crate) fn note(&mut self, tool: &str, observation: &mut Observation) -> Option<String> {
        if !matches!(observation.status, ObsStatus::Denied | ObsStatus::Error) {
            return None;
        }
        let payload = &observation.payload;
        let reason = payload
            .get("reason")
            .or_else(|| payload.get("error"))
            .and_then(serde_json::Value::as_str)?
            .to_owned();
        let count = self.0.entry(format!("{tool}\n{reason}")).or_default();
        *count += 1;
        let note = match *count {
            1 => return None,
            2 => TWICE,
            _ => THRICE,
        };
        observation.payload["repeated"] = serde_json::json!(note);
        if let Some(text) = &mut observation.model_text {
            text.push_str("\n\n");
            text.push_str(note);
        }
        (*count >= 3).then(|| {
            format!("Medha stopped the model retrying {tool}: refused three times ({reason})")
        })
    }
}

#[cfg(test)]
#[path = "refusals_tests.rs"]
mod tests;
