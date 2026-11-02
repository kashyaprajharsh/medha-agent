use crate::tool::{clip, grouped};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Results saved before the kernel named where it stored them: one whose JSON
/// was longer than this was stored under the SHA-256 of that JSON.
const LEGACY_SPILL_THRESHOLD: usize = 16_000;

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
            Some(path) if stored_file(path).is_none() => Some(path.to_owned()),
            _ => self.page_name(args).or_else(|| {
                let command = args.get("command")?.as_str()?;
                Some(format!("output of {}", clip(command, 60)))
            }),
        };
        if let Some(name) = name {
            self.call_paths.insert(id.to_owned(), name);
        }
    }

    /// Link the hashes a result is stored under to what its call named: the
    /// one a paging read returns, and the one the kernel spilled it to, which
    /// it names (`stored`) or, for older results, implied by their size.
    pub fn result(&mut self, id: &str, output: &Value, stored: Option<&str>) {
        let Some(path) = self.call_paths.get(id) else {
            return;
        };
        let file = path.split(" · ").next().unwrap_or(path).to_owned();
        let returned = output
            .get("hash")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let spilled = match stored {
            Some(hash) => Some(hash.to_owned()),
            None => serde_json::to_string(output)
                .ok()
                .filter(|json| json.len() > LEGACY_SPILL_THRESHOLD)
                .map(|json| format!("{:x}", Sha256::digest(json.as_bytes()))),
        };
        for hash in returned.into_iter().chain(spilled) {
            self.hash_paths.entry(hash).or_insert_with(|| file.clone());
        }
    }

    /// The target for a call that reads a stored output, by its hash or by the file it is kept in.
    pub fn target(&self, args: &Value) -> Option<String> {
        let path = args.get("path").and_then(Value::as_str);
        if path.and_then(stored_file).is_some() {
            return self.page_name(args);
        }
        // Matches `read` in tools: beside a path, only a real 64-hex hash means stored output.
        let real = args.get("hash")?.as_str().is_some_and(is_hash);
        let named = path.is_some_and(|path| !path.trim().is_empty());
        (real || !named).then_some(())?;
        self.page_name(args)
    }

    fn page_name(&self, args: &Value) -> Option<String> {
        let by_path = args
            .get("path")
            .and_then(Value::as_str)
            .and_then(stored_file);
        let hash = match by_path {
            Some(hash) => hash,
            None => args.get("hash")?.as_str()?,
        };
        let name = self.hash_paths.get(hash).cloned().unwrap_or_else(|| {
            format!("stored output {}", hash.chars().take(8).collect::<String>())
        });
        let offset = args.get("offset").and_then(Value::as_u64);
        if by_path.is_some() {
            // A stored file read by path is paged by lines, like any file.
            return Some(match offset {
                Some(line) => format!("{name} · from line {}", grouped(line)),
                None => name,
            });
        }
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

fn is_hash(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The hash a path names when it is a file in a project's store, `…/artifacts/<hash>`.
fn stored_file(path: &str) -> Option<&str> {
    let (folder, name) = path
        .trim_end_matches(['/', '\\'])
        .rsplit_once(['/', '\\'])?;
    let parent = folder.rsplit(['/', '\\']).next()?;
    (parent == "artifacts" && is_hash(name)).then_some(name)
}

#[cfg(test)]
#[path = "stored_tests.rs"]
mod tests;
