//! A throwaway folder: one per chat, kept while the chat is in use, or one per process, removed at exit.

use std::path::{Path, PathBuf};

/// A chat's scratch folder unused for this long is removed; macOS clears its temp files at the same age.
pub const IDLE_DAYS: u64 = 3;
const IDLE: std::time::Duration = std::time::Duration::from_secs(IDLE_DAYS * 24 * 60 * 60);
/// Chat folders are named for the chat, so reopening it finds the same files.
const CHAT: &str = "chat-";

pub struct Scratch {
    path: PathBuf,
    lock: PathBuf,
    held: Option<std::fs::File>,
    /// A chat's folder outlives the session that opened it.
    kept: bool,
}

/// Whether a released lock was last used longer ago than [`IDLE`].
fn idle(lock: &std::fs::File) -> bool {
    lock.metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|used| used.elapsed().ok())
        .is_some_and(|age| age > IDLE)
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
            // A chat's folder waits for the chat to come back, until it has gone unused too long.
            let chat = lock
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(CHAT));
            if chat && !idle(&released) {
                continue;
            }
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
            kept: false,
        })
    }

    /// The chat's own folder, the same whenever the chat is opened, so paths in its history stay valid.
    pub fn for_chat(chat: &str) -> std::io::Result<Self> {
        Self::for_chat_in(&std::env::temp_dir(), chat)
    }

    fn for_chat_in(temp: &Path, chat: &str) -> std::io::Result<Self> {
        if chat.is_empty() || !chat.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(std::io::Error::other("a chat id names its scratch folder"));
        }
        let root = user_root(temp)?;
        sweep_unlocked(&root);
        let name = format!("{CHAT}{chat}");
        let lock = root.join(format!("{name}.lock"));
        let held = try_lock_file(&lock)
            .ok_or_else(|| std::io::Error::other("the chat's scratch folder is open elsewhere"))?;
        // The lock's time is the chat's last use, which the sweep reads.
        let _ = held.set_modified(std::time::SystemTime::now());
        let path = root.join(name);
        if let Err(error) = std::fs::create_dir(&path)
            && error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(error);
        }
        if !std::fs::symlink_metadata(&path)?.is_dir() {
            return Err(std::io::Error::other(format!(
                "{} is not a folder",
                path.display()
            )));
        }
        #[cfg(unix)]
        let path = path.canonicalize()?;
        Ok(Self {
            path,
            lock,
            held: Some(held),
            kept: true,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.kept {
            // Its lock file stays: its time is the chat's last use.
            self.held.take();
            return;
        }
        let _ = std::fs::remove_dir_all(&self.path);
        // Windows refuses to delete a file that is still open.
        self.held.take();
        let _ = std::fs::remove_file(&self.lock);
    }
}

#[cfg(test)]
#[path = "scratch_tests.rs"]
mod tests;
