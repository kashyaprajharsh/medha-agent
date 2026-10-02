//! A small, bounded shelf of the pages servers sent for their screens, so a
//! chat reopened later can show a screen without its server. One copy per
//! screen however many chats used it, and the oldest go when the shelf is full.

use serde_json::{Value, json};
use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
};

const MAX_PAGES: usize = 16;
const MAX_PAGE: usize = 8 * 1024 * 1024;

fn file(dir: &Path, server: &str, uri: &str) -> PathBuf {
    let mut hash = DefaultHasher::new();
    (server, uri).hash(&mut hash);
    dir.join(format!("{:016x}.json", hash.finish()))
}

pub fn keep(dir: &Path, server: &str, uri: &str, page: &Value) -> Result<(), String> {
    let text = json!({ "server": server, "uri": uri, "page": page }).to_string();
    if text.len() > MAX_PAGE {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let target = file(dir, server, uri);
    // Written beside and moved into place, so a page is never read half written.
    let staged = target.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&staged, text).map_err(|error| error.to_string())?;
    fs::rename(&staged, &target).map_err(|error| error.to_string())?;
    trim(dir);
    Ok(())
}

pub fn kept(dir: &Path, server: &str, uri: &str) -> Option<Value> {
    let text = fs::read_to_string(file(dir, server, uri)).ok()?;
    let mut entry: Value = serde_json::from_str(&text).ok()?;
    // Two screens may share a file name; only its own page is ever handed back.
    (entry["server"] == server && entry["uri"] == uri).then(|| entry["page"].take())
}

fn trim(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut pages: Vec<_> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|kind| kind == "json"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    pages.sort();
    let extra = pages.len().saturating_sub(MAX_PAGES);
    for (_, path) in pages.into_iter().take(extra) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
#[path = "kept_tests.rs"]
mod tests;
