//! Cooperative ownership of a durable conversation across processes.
//!
//! The inode is permanent; deleting a lock file would let two owners lock
//! different inodes. The OS releases ownership after a crash, without a TTL
//! that could expire while the original owner is still writing.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, Weak};

pub struct SessionLease {
    _file: File,
    pub session: ulid::Ulid,
    pub generation: ulid::Ulid,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        // A concurrent fork can briefly inherit the open file description
        // before CLOEXEC takes effect. Closing only our descriptor would keep
        // the conversation spuriously locked until that unrelated exec.
        let _ = self._file.unlock();
    }
}

impl SessionLease {
    pub fn acquire(state: &Path, session: ulid::Ulid) -> Result<Self> {
        let directory = state.join("session-leases");
        std::fs::create_dir_all(&directory)
            .context("could not create the conversation lease directory")?;
        if std::fs::symlink_metadata(&directory)?
            .file_type()
            .is_symlink()
        {
            anyhow::bail!("conversation lease directory must not be a symlink");
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        }
        let path = directory.join(format!("{session}.lock"));
        let mut file = options
            .open(&path)
            .context("could not open the conversation lease")?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            anyhow::bail!("conversation lease must be a regular file");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
                anyhow::bail!(
                    "conversation lease must be this user's own single-link regular file"
                );
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
            {
                anyhow::bail!("conversation lease must not be a reparse point");
            }
        }
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => anyhow::anyhow!(
                "Conversation {session} is already open in another Medha session. Close it there before resuming it here."),
            std::fs::TryLockError::Error(error) => anyhow::anyhow!(error).context("could not lock the conversation"),
        })?;
        // A new owner has a distinct diagnostic identity, even after PID reuse.
        // Metadata is diagnostic; ownership comes from the held OS lock.
        let generation = ulid::Ulid::new();
        file.set_len(0)?;
        writeln!(file, "generation={generation}\npid={}", std::process::id())?;
        Ok(Self {
            _file: file,
            session,
            generation,
        })
    }
}

/// Keeps the idle conversation owned, and separately reserves targets while
/// a resume or rewind is prepared. Old reservations expire when the final
/// turn/replay holding them finishes, rather than accumulating after /clear.
pub struct Ownership {
    directory: PathBuf,
    state: Mutex<Owned>,
}

struct Owned {
    current: kernel::SessionClaim,
    known: HashMap<ulid::Ulid, Weak<SessionLease>>,
}

impl Ownership {
    pub fn new(directory: PathBuf, lease: SessionLease) -> Self {
        let session = lease.session;
        let lease = Arc::new(lease);
        Self {
            directory,
            state: Mutex::new(Owned {
                current: kernel::SessionClaim::new(session, lease.clone()),
                known: HashMap::from([(session, Arc::downgrade(&lease))]),
            }),
        }
    }
}

impl kernel::SessionOwner for Ownership {
    fn reserve(&self, session: ulid::Ulid) -> Result<kernel::SessionClaim, String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.known.retain(|_, lease| lease.strong_count() > 0);
        let lease = match state.known.get(&session).and_then(Weak::upgrade) {
            Some(lease) => lease,
            None => {
                let lease = Arc::new(
                    SessionLease::acquire(&self.directory, session)
                        .map_err(|error| format!("{error:#}"))?,
                );
                state.known.insert(session, Arc::downgrade(&lease));
                lease
            }
        };
        Ok(kernel::SessionClaim::new(session, lease))
    }

    fn adopt(&self, claim: kernel::SessionClaim) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .current = claim;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::SessionOwner;

    #[test]
    fn switching_keeps_in_flight_claims_and_releases_old_idle_ownership() {
        let state = tempfile::tempdir().unwrap();
        let old = ulid::Ulid::new();
        let next = ulid::Ulid::new();
        let owner = Ownership::new(
            state.path().to_path_buf(),
            SessionLease::acquire(state.path(), old).unwrap(),
        );
        let turn = owner.reserve(old).unwrap();
        let replay = owner.reserve(next).unwrap();
        assert!(SessionLease::acquire(state.path(), next).is_err());
        drop(replay);
        // Abandoning a replay leaves the current chat owned, and lets go of
        // the target. Adopting it later keeps an old running turn protected.
        drop(SessionLease::acquire(state.path(), next).unwrap());
        assert!(SessionLease::acquire(state.path(), old).is_err());
        owner.adopt(owner.reserve(next).unwrap());
        assert!(SessionLease::acquire(state.path(), old).is_err());
        drop(turn);
        assert_eq!(owner.state.lock().unwrap().current.session, next);
        assert_eq!(owner.state.lock().unwrap().known[&old].strong_count(), 0);
        drop(SessionLease::acquire(state.path(), old).unwrap());
        assert!(SessionLease::acquire(state.path(), next).is_err());
        drop(owner);
        assert!(SessionLease::acquire(state.path(), next).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn lease_release_does_not_wait_for_an_unrelated_fork_to_exec() {
        let state = tempfile::tempdir().unwrap();
        let id = ulid::Ulid::new();
        let lease = SessionLease::acquire(state.path(), id).unwrap();
        let mut pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        // Only async-signal-safe C calls run in the child; it deliberately
        // keeps the inherited file description until the parent releases it.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                libc::close(pipe[1]);
                let mut byte = 0u8;
                libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
                libc::_exit(0);
            }
        }
        assert!(pid > 0);
        unsafe {
            libc::close(pipe[0]);
        }
        drop(lease);
        let acquired = SessionLease::acquire(state.path(), id);
        unsafe {
            libc::close(pipe[1]);
            while libc::waitpid(pid, std::ptr::null_mut(), 0) < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
            }
        }
        assert!(acquired.is_ok(), "{:?}", acquired.err());
    }

    #[test]
    fn owners_exclude_each_other_but_distinct_conversations_do_not() {
        let state = tempfile::tempdir().unwrap();
        let id = ulid::Ulid::new();
        let first = SessionLease::acquire(state.path(), id).unwrap();
        assert!(SessionLease::acquire(state.path(), id).is_err());
        let other = SessionLease::acquire(state.path(), ulid::Ulid::new()).unwrap();
        let generation = first.generation;
        drop(first);
        let next = SessionLease::acquire(state.path(), id).unwrap();
        assert_ne!(generation, next.generation);
        drop(other);
    }

    #[cfg(unix)]
    #[test]
    fn canonical_path_aliases_coordinate_and_symlink_files_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&state, &alias).unwrap();
        let id = ulid::Ulid::new();
        let _lease = SessionLease::acquire(&state, id).unwrap();
        assert!(SessionLease::acquire(&alias, id).is_err());
        let victim = root.path().join("victim");
        std::fs::write(&victim, "preserve").unwrap();
        let linked = ulid::Ulid::new();
        std::os::unix::fs::symlink(
            &victim,
            state.join("session-leases").join(format!("{linked}.lock")),
        )
        .unwrap();
        assert!(SessionLease::acquire(&state, linked).is_err());
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "preserve");
    }
}
