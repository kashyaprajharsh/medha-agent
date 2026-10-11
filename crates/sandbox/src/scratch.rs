//! A throwaway folder owned by one Medha process: empty at start, removed at exit.

use std::path::{Path, PathBuf};

pub struct Scratch {
    path: PathBuf,
    lock: PathBuf,
    held: Option<std::fs::File>,
}

/// Holds an exclusive advisory lock on `path` without blocking.
pub(crate) fn try_lock_file(path: &Path) -> Option<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// Whether the process exists. An id that cannot be one is treated as alive,
/// so nothing is removed on its account.
#[cfg(unix)]
pub(crate) fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return true;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let exists = unsafe { libc::kill(pid, 0) } == 0;
    exists || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Removes every folder whose sibling `.lock` no live process holds, and every
/// `<pid>-…` folder left with no lock at all once that process has exited.
pub(crate) fn sweep_unlocked(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let found: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    for lock in &found {
        if lock.extension().is_none_or(|ext| ext != "lock") {
            continue;
        }
        if let Some(released) = try_lock_file(lock) {
            let _ = std::fs::remove_dir_all(lock.with_extension(""));
            drop(released);
            let _ = std::fs::remove_file(lock);
        }
    }
    // The system's temp cleaner can take an idle lock file and leave its
    // folder, which the pass above then never sees. A live owner may have lost
    // its lock file the same way, so only its exit makes the folder removable.
    #[cfg(unix)]
    for folder in &found {
        let owner = folder
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|_| folder.extension().is_none())
            .and_then(|name| name.split_once('-'))
            .and_then(|(pid, _)| pid.parse::<i32>().ok());
        if let Some(pid) = owner
            && !folder.with_extension("lock").exists()
            && !process_alive(pid)
            && std::fs::symlink_metadata(folder).is_ok_and(|kind| kind.is_dir())
        {
            let _ = std::fs::remove_dir_all(folder);
        }
    }
}

/// One root per user: on Linux the temp directory is shared with every account.
fn user_root(temp: &Path) -> std::io::Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        // SAFETY: reads this process's own user id.
        let uid = unsafe { libc::geteuid() };
        let root = temp.join(format!("medha-scratch-{uid}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&root) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        // Another account could have made this name first, as a link or a folder it can read.
        let found = std::fs::symlink_metadata(&root)?;
        if !found.is_dir() || found.uid() != uid || found.mode() & 0o077 != 0 {
            return Err(std::io::Error::other(format!(
                "{} is not a private folder of this user",
                root.display()
            )));
        }
        Ok(root)
    }
    #[cfg(not(unix))]
    {
        let root = temp.join("medha-scratch");
        std::fs::create_dir_all(&root)?;
        Ok(root)
    }
}

impl Scratch {
    /// Clears folders left by crashed processes, then makes a new empty one.
    pub fn create() -> std::io::Result<Self> {
        Self::create_in(&std::env::temp_dir())
    }

    fn create_in(temp: &Path) -> std::io::Result<Self> {
        let root = user_root(temp)?;
        sweep_unlocked(&root);
        let name = format!("{}-{}", std::process::id(), ulid::Ulid::new());
        let lock = root.join(format!("{name}.lock"));
        // Locked before the folder exists, so another process's sweep never sees it unowned.
        let held = try_lock_file(&lock)
            .ok_or_else(|| std::io::Error::other("the scratch lock could not be taken"))?;
        let path = root.join(name);
        std::fs::create_dir(&path)?;
        // One spelling for the prompt, the jail and the scanner; Windows' resolved form is not typable.
        #[cfg(unix)]
        let path = path.canonicalize()?;
        Ok(Self {
            path,
            lock,
            held: Some(held),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        // Windows refuses to delete a file that is still open.
        self.held.take();
        let _ = std::fs::remove_file(&self.lock);
    }
}

#[cfg(test)]
#[path = "scratch_tests.rs"]
mod tests;
