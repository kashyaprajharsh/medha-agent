//! Keeps a project's stored results and edit backups within their size limits, oldest first.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Stored text this recent stays whatever the size: a chat may still read it by path.
pub(crate) const STORED_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// Removes the oldest plain files in `folder` until it holds `limit` bytes or less. A file newer than
/// `keep_newer`, or one `keep` picks out by its first bytes, stays. Links and hidden files are never touched.
pub(crate) fn sweep(
    folder: &Path,
    limit: u64,
    keep_newer: Option<Duration>,
    keep: impl Fn(&[u8]) -> bool,
) -> usize {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return 0;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let meta = entry.path().symlink_metadata().ok()?;
            let hidden = entry.file_name().to_string_lossy().starts_with('.');
            (meta.is_file() && !hidden).then_some(())?;
            Some((meta.modified().ok()?, meta.len(), entry.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    if total <= limit {
        return 0;
    }
    files.sort();
    let cutoff = keep_newer.and_then(|age| SystemTime::now().checked_sub(age));
    let mut removed = 0;
    for (modified, size, path) in files {
        if total <= limit || cutoff.is_some_and(|cutoff| modified > cutoff) {
            break;
        }
        if keep(&first_bytes(&path)) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
            removed += 1;
        }
    }
    removed
}

fn first_bytes(path: &Path) -> Vec<u8> {
    use std::io::Read;
    let mut bytes = Vec::new();
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(32).read_to_end(&mut bytes);
    }
    bytes
}
