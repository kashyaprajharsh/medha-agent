//! A folder's whole chat history: the state Medha keeps for it outside the folder.

use anyhow::{Context, Result};
use std::path::Path;

pub const OPEN: &str =
    "A chat in this folder is open. Close it first, then delete the folder's history.";

/// Bytes the folder's history takes on disk, links not followed.
pub fn size(folder: &Path) -> u64 {
    crate::config::existing_state_dir(folder).map_or(0, |state| bytes_in(&state))
}

fn bytes_in(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    std::fs::read_dir(path).map_or(0, |entries| {
        entries.flatten().map(|entry| bytes_in(&entry.path())).sum()
    })
}

/// Deletes the folder's history, refused while any chat in it holds its lease, in any process.
pub fn delete(folder: &Path) -> Result<()> {
    let Some(state) = crate::config::existing_state_dir(folder) else {
        return Ok(());
    };
    let mut held = Vec::new();
    for entry in std::fs::read_dir(state.join("session-leases"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name();
        let Some(chat) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".lock"))
            .and_then(|id| id.parse::<ulid::Ulid>().ok())
        else {
            continue;
        };
        held.push(
            crate::lease::SessionLease::acquire(&state, chat).map_err(|_| anyhow::anyhow!(OPEN))?,
        );
    }
    // The leases go last, so a chat cannot start on a log that is half deleted.
    for entry in std::fs::read_dir(&state)?.flatten() {
        if entry.file_name() == "session-leases" {
            continue;
        }
        let path = entry.path();
        let removed = if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.with_context(|| format!("could not delete {}", path.display()))?;
    }
    drop(held);
    std::fs::remove_dir_all(&state).with_context(|| format!("could not delete {}", state.display()))
}
