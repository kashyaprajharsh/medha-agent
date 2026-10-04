//! One folder's machine-local state: where it lives, its lock file, and its durable stores.

use crate::Notices;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Workspace {
    /// As the caller named it; lock trust is keyed on this exact spelling.
    pub given: PathBuf,
    pub root: PathBuf,
    pub state: PathBuf,
    pub home: PathBuf,
}

pub struct Store {
    pub log: Arc<store::SqliteLog>,
    pub artifacts: Arc<store::FileArtifactStore>,
}

/// Loads `medha.lock` beside `given`, without what a repository may not grant itself.
pub fn load_lock(given: &Path, notices: &dyn Notices) -> Result<lockfile::MedhaLock> {
    let lock = lockfile::MedhaLock::load(given.join("medha.lock"))?.unwrap_or_default();
    let state = crate::config::state_dir(given)?;
    Ok(apply_lock_trust(
        lock,
        &given.join("medha.lock"),
        given,
        &state,
        notices,
    ))
}

pub fn apply_lock_trust(
    lock: lockfile::MedhaLock,
    lock_path: &Path,
    workspace: &Path,
    state: &Path,
    notices: &dyn Notices,
) -> lockfile::MedhaLock {
    let risky = lock.risky_settings();
    if risky.is_empty() {
        return lock;
    }
    let key = workspace.display().to_string();
    let accepted = lockfile::AcceptedLocks::load(&state.join("lock_trust.toml"));
    if accepted.allows(&key, &risky) {
        return lock;
    }
    notices.say(&format!(
        "warning: ignoring {} privilege-relaxing setting(s) in {} — this file ships \
         inside the repository and cannot grant itself authority:",
        risky.len(),
        lock_path.display()
    ));
    for setting in &risky {
        notices.say(&format!("  · {setting}"));
    }
    notices.say("  run `medha trust` in this directory to accept them.");
    lock.without_risky_settings()
}

pub fn warn_legacy_state(cwd: &Path, state: &Path, notices: &dyn Notices) {
    let legacy = cwd.join(".medha");
    let runtime_entries = [
        "events.db",
        "events.db-wal",
        "events.db-shm",
        "artifacts",
        "snapshots",
        "logs",
        "trust.lock",
    ];
    let found = runtime_entries
        .iter()
        // Detect dangling links without following repository-controlled paths.
        .filter(|name| std::fs::symlink_metadata(legacy.join(name)).is_ok())
        .copied()
        .collect::<Vec<_>>();
    if !found.is_empty() {
        notices.say(&format!(
            "warning: ignored repository-local legacy runtime state in {} ({}) — \
             automatic import is disabled because a checkout cannot authenticate \
             event, artifact, log, or trust data; fresh machine-local state is in {}",
            legacy.display(),
            found.join(", "),
            state.display()
        ));
    }
}

impl Workspace {
    /// Runtime state stays outside the repository.
    pub fn open(given: PathBuf, notices: &dyn Notices) -> Result<Self> {
        let root = given.canonicalize().unwrap_or_else(|_| given.clone());
        let state = crate::config::state_dir(&root)?;
        let home = crate::config::medha_home()?;
        warn_legacy_state(&root, &state, notices);
        Ok(Self {
            given,
            root,
            state,
            home,
        })
    }

    /// A damaged log is kept for recovery and never used as trusted history.
    pub fn open_store(&self) -> Result<Store> {
        let log = Arc::new(store::SqliteLog::open_with_mutation_lock(
            self.state.join("events.db"),
            self.home.join("mutations.db"),
        )?);
        log.verify()
            .context("event log integrity check failed; refusing to start")?;
        let artifacts = Arc::new(store::FileArtifactStore::open(
            self.state.join("artifacts"),
        )?);
        Ok(Store { log, artifacts })
    }
}
