//! Where the desktop app keeps its personal chats, so the terminal can list and continue them.

use std::path::{Path, PathBuf};

/// The desktop app's identifier; its data folder is named after it, as `tauri.conf.json` says.
const DESKTOP_APP: &str = "app.medha.desktop";
/// Most personal chats listed, newest first, so a long history stays quick to show.
const MOST: usize = 30;

/// Each personal chat's folder, newest first; none when the desktop app was never used.
pub(crate) fn folders() -> Vec<PathBuf> {
    // The desktop's data folder: the system data folder plus its identifier.
    dirs::data_dir()
        .map(|data| folders_in(&data.join(DESKTOP_APP).join("personal")))
        .unwrap_or_default()
}

fn folders_in(personal: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(personal) else {
        return Vec::new();
    };
    let mut chats: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("chat-"))
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| {
            let used = entry.metadata().and_then(|meta| meta.modified());
            (used.unwrap_or(std::time::UNIX_EPOCH), entry.path())
        })
        .collect();
    chats.sort_by_key(|chat| std::cmp::Reverse(chat.0));
    chats.into_iter().take(MOST).map(|(_, path)| path).collect()
}

#[cfg(test)]
#[path = "personal_chats_tests.rs"]
mod tests;
