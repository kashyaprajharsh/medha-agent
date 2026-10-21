//! What each call must not run beside: its lane among writers.

use kernel::write_lanes::{WHOLE, path_key};
use std::path::{Path, PathBuf};

/// Git subcommands that only read the repository.
pub(crate) const GIT_READS: [&str; 5] = ["status", "diff", "log", "blame", "show"];

/// Keeps a reading git from taking the index lock a commit beside it needs.
pub(crate) fn git_read_env() -> (String, String) {
    ("GIT_OPTIONAL_LOCKS".into(), "0".into())
}

/// Where `path` really is, through links, even before its last parts exist.
fn real(path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    while !existing.exists() {
        missing.push(existing.file_name()?);
        existing = existing.parent()?;
    }
    let mut real = existing.canonicalize().ok()?;
    real.extend(missing.into_iter().rev());
    Some(real)
}

/// Lane of a command that may write anywhere in the workspace at `root`.
pub(crate) fn tree(root: &Path) -> String {
    real(root).map_or(WHOLE.into(), |root| path_key(&root))
}

/// Lane of a write to the file `path` names; one that cannot be placed runs alone.
pub(crate) fn file(root: &Path, path: Option<&str>) -> String {
    path.and_then(|path| {
        let mut joined = root.to_path_buf();
        for part in Path::new(path).components() {
            // Windows normalizes `..` when pushing onto a verbatim path.
            // Inspect it before that can hide an unknown or non-directory parent.
            if part == std::path::Component::ParentDir && !joined.is_dir() {
                return None;
            }
            joined.push(part);
        }
        real(&joined)
    })
    .map_or(WHOLE.into(), |path| path_key(&path))
}

/// Whether a shell command only reads: nothing in it writes a file or runs a program that might.
pub(crate) fn shell_only_reads(command: &str) -> bool {
    shell_reader::read(command).reads_only()
}

#[cfg(test)]
#[path = "lanes_tests.rs"]
mod tests;
