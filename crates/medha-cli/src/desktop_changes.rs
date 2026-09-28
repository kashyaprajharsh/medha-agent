//! What one session changed on disk, file by file: each write and edit in
//! turn, newest first, and the net change from before Medha first touched the
//! file to after its last write. Read from the event log rather than Git, so
//! it works in any folder and for nested repos.

use kernel::{Event, EventKind};
use serde::Serialize;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use std::collections::HashMap;

const WRITE_TOOLS: [&str; 4] = ["edit", "write", "fs.edit", "fs.write"];
const DIFF_LIMIT: usize = 200_000;
const EDIT_DIFF_LIMIT: usize = 50_000;
const EDITS_SHOWN: usize = 20;

#[derive(Serialize, Debug, PartialEq)]
pub(crate) struct FileChange {
    pub path: String,
    pub created: bool,
    pub added: usize,
    pub removed: usize,
    pub edits: usize,
    pub diff: String,
    pub last_ts: f64,
    /// The latest writes first, at most [`EDITS_SHOWN`] of them.
    pub history: Vec<Edit>,
}

#[derive(Serialize, Debug, PartialEq)]
pub(crate) struct Edit {
    /// `created`, `wrote` for a whole-file write, or `edited`.
    pub kind: &'static str,
    pub added: usize,
    pub removed: usize,
    pub diff: String,
    pub ts: f64,
}

struct Touched {
    before: Option<String>,
    after: String,
    edits: Vec<(bool, String, String, f64)>,
}

/// Files in the order the session first wrote them. A file whose last write
/// put back what was there before is left out: it has no net change.
pub(crate) fn session_changes(events: &[Event]) -> Vec<FileChange> {
    let mut whole_writes: HashMap<String, bool> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut touched: HashMap<String, Touched> = HashMap::new();
    for event in events {
        let field = |key: &str| event.payload.get(key).and_then(Value::as_str);
        match event.kind {
            EventKind::ModelIntent => {
                if let (Some(id), Some(tool)) = (field("id"), field("tool"))
                    && WRITE_TOOLS.contains(&tool)
                {
                    let args = &event.payload["args"];
                    let whole = args.get("old_string").is_none() && args.get("content").is_some();
                    whole_writes.insert(id.to_owned(), whole);
                }
            }
            EventKind::ToolObs => {
                let Some(id) = field("intent_id") else {
                    continue;
                };
                let Some(&whole) = whole_writes.get(id) else {
                    continue;
                };
                let result = &event.payload["payload"];
                let (Some("ok"), Some(path), Some(after)) = (
                    field("status"),
                    result.get("path").and_then(Value::as_str),
                    result.get("new").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let old = result.get("old").and_then(Value::as_str);
                let entry = touched.entry(path.to_owned()).or_insert_with(|| {
                    order.push(path.to_owned());
                    Touched {
                        before: old.map(str::to_owned),
                        after: String::new(),
                        edits: Vec::new(),
                    }
                });
                let previous = old.map_or_else(|| entry.after.clone(), str::to_owned);
                entry
                    .edits
                    .push((whole, previous, after.to_owned(), event.ts));
                entry.after = after.to_owned();
            }
            _ => {}
        }
    }
    order
        .into_iter()
        .filter_map(|path| {
            let file = touched.remove(&path)?;
            let before = file.before.as_deref().unwrap_or_default();
            if before == file.after {
                return None;
            }
            let created = file.before.as_deref().is_none_or(str::is_empty);
            let (added, removed, diff) = diff_of(before, &file.after, &path, DIFF_LIMIT);
            let edits = file.edits.len();
            let last_ts = file.edits.last().map_or(0.0, |edit| edit.3);
            let history = file
                .edits
                .iter()
                .enumerate()
                .rev()
                .take(EDITS_SHOWN)
                .map(|(index, (whole, old, new, ts))| {
                    let (added, removed, diff) = diff_of(old, new, &path, EDIT_DIFF_LIMIT);
                    let kind = match (index, created, whole) {
                        (0, true, _) => "created",
                        (_, _, true) => "wrote",
                        _ => "edited",
                    };
                    Edit {
                        kind,
                        added,
                        removed,
                        diff,
                        ts: *ts,
                    }
                })
                .collect();
            Some(FileChange {
                created,
                path,
                added,
                removed,
                edits,
                diff,
                last_ts,
                history,
            })
        })
        .collect()
}

/// Lines added and removed, and a unified diff cut to `limit` bytes.
fn diff_of(before: &str, after: &str, path: &str, limit: usize) -> (usize, usize, String) {
    let diff = TextDiff::from_lines(before, after);
    let (added, removed) = diff
        .iter_all_changes()
        .fold((0, 0), |(added, removed), change| match change.tag() {
            ChangeTag::Insert => (added + 1, removed),
            ChangeTag::Delete => (added, removed + 1),
            ChangeTag::Equal => (added, removed),
        });
    let mut text = diff
        .unified_diff()
        .context_radius(3)
        .header(path, path)
        .to_string();
    if text.len() > limit {
        let cut = (0..=limit)
            .rev()
            .find(|at| text.is_char_boundary(*at))
            .unwrap_or(0);
        text.truncate(cut);
        text.push_str("\n… diff cut short; open the file to see the rest\n");
    }
    (added, removed, text)
}

#[cfg(test)]
#[path = "desktop_changes_tests.rs"]
mod tests;
