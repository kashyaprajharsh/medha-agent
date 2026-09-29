//! Test fixtures shared across the workspace.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A temporary folder removed when dropped, pass or fail; usable wherever a path is.
pub struct Scratch {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

/// A fresh folder named after `tag`, e.g. `scratch("medha-store")`.
pub fn scratch(tag: &str) -> Scratch {
    let dir = tempfile::Builder::new()
        .prefix(&format!("{tag}-"))
        .tempdir()
        .expect("create a scratch folder");
    let path = dir.path().to_path_buf();
    Scratch { _dir: dir, path }
}

impl Scratch {
    /// The same folder, pointing at `relative` inside it.
    pub fn at(self, relative: impl AsRef<Path>) -> Scratch {
        let path = self.path.join(relative);
        Scratch {
            _dir: self._dir,
            path,
        }
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<OsStr> for Scratch {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}
