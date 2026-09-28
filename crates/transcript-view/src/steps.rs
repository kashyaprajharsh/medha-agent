use crate::call::{Call, describe};
use crate::outcome::{Outcome, outcome};
use crate::stored::StoredNames;
use crate::tool::canonical;
use serde_json::Value;
use std::collections::HashMap;

/// The tool steps of one session, read in order. A result is shaped by the
/// call it answers, and a later read of stored output is named after the file
/// an earlier call read, so both need the calls that came before.
#[derive(Default)]
pub struct Steps {
    stored: StoredNames,
    names: HashMap<String, String>,
}

impl Steps {
    pub fn call(&mut self, id: &str, tool: &str, args: &Value) -> Call {
        let name = canonical(tool, args);
        let mut call = describe(&name, tool, args);
        if let Some(target) = self.stored.target(args) {
            call.target = Some(target);
        }
        self.stored.call(id, args);
        self.names.insert(id.to_owned(), name);
        call
    }

    /// `tool` is used only when the call was never seen.
    pub fn result(&mut self, id: &str, tool: &str, ok: bool, output: &Value) -> Outcome {
        self.stored.result(id, output);
        let name = self
            .names
            .get(id)
            .cloned()
            .unwrap_or_else(|| canonical(tool, &Value::Null));
        outcome(&name, ok, output)
    }
}

#[cfg(test)]
#[path = "steps_tests.rs"]
mod tests;
