//! Whole-file knowledge assembled only from pages delivered to this chat.

use sandbox::GuardedFileState;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Files one chat can have read, counted per chat so another chat cannot push them out.
const FILES: usize = 1000;
/// Every chat's records together, so many chats cannot grow memory without bound.
const ALL_FILES: usize = 16_000;
/// Page text kept so a changed file needs only its changed pages read again.
const BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Default)]
pub(crate) struct Revisions(Arc<Mutex<VecDeque<Entry>>>);

struct Entry {
    session: Option<ulid::Ulid>,
    path: PathBuf,
    revision: Revision,
}

#[derive(Default)]
struct Revision {
    pages: BTreeMap<u64, (u64, String, bool)>,
    known: Option<([u8; 32], u64)>,
}

impl Revision {
    fn bytes(&self) -> usize {
        self.pages.values().map(|(_, text, _)| text.len()).sum()
    }

    fn digest(&self) -> Option<([u8; 32], u64)> {
        let mut next = 1;
        let mut hash = Sha256::new();
        let mut len = 0;
        for (&start, (end, content, more)) in &self.pages {
            if start != next {
                return None;
            }
            hash.update(content.as_bytes());
            len += content.len() as u64;
            if !more {
                return Some((hash.finalize().into(), len));
            }
            next = end.checked_add(1)?;
        }
        None
    }
}

impl Revisions {
    pub fn delivered(&self, observation: &kernel::Observation) {
        if !matches!(observation.status, kernel::ObsStatus::Ok) {
            return;
        }
        let payload = &observation.payload;
        let (Some(path), Some(text), Some(start), Some(end)) = (
            payload["resolved_path"].as_str(),
            payload["content"].as_str(),
            payload["start_line"].as_u64(),
            payload["end_line"].as_u64(),
        ) else {
            return;
        };
        let Some(view) = crate::model_view::file_page(payload) else {
            return;
        };
        if !observation
            .model_text
            .as_deref()
            .is_some_and(|shown| shown.starts_with(&view))
            || crate::model_view::page_lines(text)
                .any(|line| matches!(line, std::borrow::Cow::Owned(_)))
            || payload["note"].is_string()
        {
            return;
        }
        self.page(
            Path::new(path),
            start,
            end,
            text,
            payload["has_more"] == true,
        );
    }

    pub fn page(&self, path: &Path, start: u64, end: u64, text: &str, more: bool) {
        let mut files = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let owner = kernel::egress::session_id();
        let index = files
            .iter()
            .position(|entry| entry.session == owner && entry.path == path);
        let mut revision = index
            .and_then(|index| files.remove(index))
            .map(|entry| entry.revision)
            .unwrap_or_default();
        revision.pages.insert(start, (end, text.into(), more));
        if revision.bytes() > BYTES {
            return;
        }
        if let Some(known) = revision.digest() {
            revision.known = Some(known);
        }
        files.push_back(Entry {
            session: owner,
            path: path.into(),
            revision,
        });
        evict(&mut files, owner);
    }

    pub fn permits(&self, path: &Path, state: &GuardedFileState) -> bool {
        let GuardedFileState::Present { sha256, len, .. } = state else {
            return true;
        };
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|entry| {
                entry.session == kernel::egress::session_id()
                    && entry.path == path
                    && entry.revision.known == Some((*sha256, *len))
            })
    }

    /// Why this chat's copy of `content` may be out of date: no read of it, or a read before it changed.
    pub fn stale(
        &self,
        path: &Path,
        state: &GuardedFileState,
        content: &str,
    ) -> Option<&'static str> {
        let GuardedFileState::Present { sha256, len, .. } = state else {
            return None;
        };
        let files = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let owner = kernel::egress::session_id();
        let Some(entry) = files
            .iter()
            .find(|entry| entry.session == owner && entry.path == path)
        else {
            return Some("No read in this chat shows this file's current text.");
        };
        if entry.revision.known == Some((*sha256, *len)) {
            return None;
        }
        let lines: Vec<&str> = content.split_inclusive('\n').collect();
        let current = |start: u64, end: u64| -> Option<String> {
            let from = usize::try_from(start.checked_sub(1)?).ok()?;
            let to = usize::try_from(end).ok()?;
            Some(lines.get(from..to.min(lines.len()))?.concat())
        };
        let changed = entry.revision.pages.is_empty()
            || entry
                .revision
                .pages
                .iter()
                .any(|(&start, (end, text, _))| current(start, *end).as_deref() != Some(text));
        changed.then_some("The file has changed since this chat last read it.")
    }

    pub fn written(&self, path: &Path, text: &str) {
        let mut files = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let owner = kernel::egress::session_id();
        files.retain(|entry| entry.session != owner || entry.path != path);
        files.push_back(Entry {
            session: owner,
            path: path.into(),
            revision: Revision {
                known: Some((Sha256::digest(text.as_bytes()).into(), text.len() as u64)),
                ..Revision::default()
            },
        });
        evict(&mut files, owner);
    }
}

/// Drops the oldest records: this chat's past its own count, any past the shared bounds.
fn evict(files: &mut VecDeque<Entry>, owner: Option<ulid::Ulid>) {
    while files.iter().filter(|entry| entry.session == owner).count() > FILES {
        let oldest = files.iter().position(|entry| entry.session == owner);
        files.remove(oldest.unwrap_or(0));
    }
    while files.len() > ALL_FILES
        || files
            .iter()
            .map(|entry| entry.revision.bytes())
            .sum::<usize>()
            > BYTES
    {
        files.pop_front();
    }
}

#[cfg(test)]
#[path = "read_revisions_tests.rs"]
mod tests;
