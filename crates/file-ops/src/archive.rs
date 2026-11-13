//! Unpacking a zip safely: every entry stays inside the target, and its count and size are bounded.

use std::io::Read;
use std::path::{Path, PathBuf};

/// How much one archive may hold.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub entries: usize,
    pub bytes: u64,
}

/// What to do with the archive's top folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Top {
    /// Always one wrapper folder, as GitHub's archives have.
    Drop,
    /// Dropped only when every entry sits in the same one, as when a person zips a folder.
    DropIfShared,
}

#[derive(Debug)]
pub enum UnpackError {
    Unreadable(String),
    Unsafe(String),
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
}

impl std::fmt::Display for UnpackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(why) => write!(f, "the archive cannot be read: {why}"),
            Self::Unsafe(why) => f.write_str(why),
            Self::Io { path, error } => write!(f, "unpacking {}: {error}", path.display()),
        }
    }
}

/// Unpacks `bytes` into `dir`. Links are not made: each comes back as (where it sits, what it names).
pub fn unpack_zip(
    bytes: &[u8],
    dir: &Path,
    limits: Limits,
    top: Top,
) -> Result<Vec<(PathBuf, String)>, UnpackError> {
    let unreadable = |error: zip::result::ZipError| UnpackError::Unreadable(error.to_string());
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(unreadable)?;
    if archive.len() > limits.entries {
        return Err(UnpackError::Unsafe(format!(
            "the archive has more than {} files",
            limits.entries
        )));
    }
    let mut names = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(unreadable)?;
        let path = entry.enclosed_name().ok_or_else(|| {
            UnpackError::Unsafe(format!(
                "archive entry {} leaves its folder",
                entry.name().as_deref().unwrap_or("?")
            ))
        })?;
        names.push(path);
    }
    let strip = match top {
        Top::Drop => 1,
        Top::DropIfShared => usize::from(shares_one_folder(&names)),
    };
    let (mut unpacked, mut links) = (0u64, Vec::new());
    for (index, path) in names.into_iter().enumerate() {
        let mut entry = archive.by_index(index).map_err(unreadable)?;
        let relative: PathBuf = path.components().skip(strip).collect();
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = dir.join(&relative);
        let io = |error| UnpackError::Io {
            path: target.clone(),
            error,
        };
        if entry.is_symlink() {
            let mut pointee = String::new();
            entry.read_to_string(&mut pointee).map_err(io)?;
            links.push((relative, pointee));
            continue;
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(io)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let mut file = std::fs::File::create(&target).map_err(io)?;
        unpacked += std::io::copy(
            &mut (&mut entry).take(limits.bytes - unpacked + 1),
            &mut file,
        )
        .map_err(io)?;
        if unpacked > limits.bytes {
            return Err(UnpackError::Unsafe(format!(
                "the archive unpacks to more than {} MB",
                limits.bytes / (1024 * 1024)
            )));
        }
    }
    Ok(links)
}

/// Every entry lies in one folder, which holds more than itself.
fn shares_one_folder(names: &[PathBuf]) -> bool {
    let mut first = None;
    let mut inside = false;
    for name in names {
        let mut parts = name.components();
        let Some(top) = parts.next() else {
            return false;
        };
        if *first.get_or_insert(top) != top {
            return false;
        }
        inside |= parts.next().is_some();
    }
    inside
}
