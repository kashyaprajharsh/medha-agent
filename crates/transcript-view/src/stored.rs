use crate::tool::{clip, grouped};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// The kernel's `SPILL_THRESHOLD`: a result whose JSON is longer than this is
/// stored under the SHA-256 of that JSON and the model pages it by hash.
const SPILL_THRESHOLD: usize = 16_000;

/// Names stored outputs after what produced them. A large result is kept as
/// an artifact and paged later by hash, so later pages can say which file or
/// command they are part of.
#[derive(Default)]
pub(crate) struct StoredNames {
    call_paths: HashMap<String, String>,
    hash_paths: HashMap<String, String>,
}

impl StoredNames {
    /// Remember what a call asked for, before its result arrives.
    pub fn call(&mut self, id: &str, args: &Value) {
        let name = match args.get("path").and_then(Value::as_str) {
            Some(path) => Some(path.to_owned()),
            None => self.page_name(args).or_else(|| {
                let command = args.get("command")?.as_str()?;
                Some(format!("output of {}", clip(command, 60)))
            }),
        };
        if let Some(name) = name {
            self.call_paths.insert(id.to_owned(), name);
        }
    }

    /// Link the hashes a result is stored under to what its call named: the
    /// one a paging read returns, and the one the kernel spilled it to.
    pub fn result(&mut self, id: &str, output: &Value) {
        let Some(path) = self.call_paths.get(id) else {
            return;
        };
        let file = path.split(" · ").next().unwrap_or(path).to_owned();
        let returned = output
            .get("hash")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let spilled = serde_json::to_string(output)
            .ok()
            .filter(|json| json.len() > SPILL_THRESHOLD)
            .map(|json| format!("{:x}", Sha256::digest(json.as_bytes())));
        for hash in returned.into_iter().chain(spilled) {
            self.hash_paths.entry(hash).or_insert_with(|| file.clone());
        }
    }

    /// The target for a call that reads a stored output by hash.
    pub fn target(&self, args: &Value) -> Option<String> {
        args.get("hash")?;
        self.page_name(args)
    }

    fn page_name(&self, args: &Value) -> Option<String> {
        let hash = args.get("hash")?.as_str()?;
        let name = self.hash_paths.get(hash).cloned().unwrap_or_else(|| {
            format!("stored output {}", hash.chars().take(8).collect::<String>())
        });
        let offset = args.get("offset").and_then(Value::as_u64);
        let length = args.get("length").and_then(Value::as_u64);
        Some(match (offset, length) {
            (Some(offset), Some(length)) => {
                format!("{name} · {}–{}", grouped(offset), grouped(offset + length))
            }
            (Some(offset), None) => format!("{name} · from {}", grouped(offset)),
            _ => name,
        })
    }
}

#[cfg(test)]
#[path = "stored_tests.rs"]
mod tests;
