//! File access permission system with live ask-then-persist flow.
//!
//! 1. RESOLVE target path fully before any check
//! 2. Allow immediately if inside workspace root
//! 3. Check the machine-local trust file for trusted paths if outside workspace
//! 4. Prompt user via HumanGate if not trusted
//! 5. Persist "always allow" decisions to the machine-local trust file
//! 6. Separate read/write permissions
//! 7. Load all entries into memory on startup
//! 8. Audit log every out-of-workspace access attempt

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use fd_lock::RwLock as FileRwLock;
use kernel::HumanGate;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("I/O error: {0}")]
    Io(String),
    #[error("Path resolution failed: {0}")]
    Resolution(String),
    #[error("User denied access to {path}")]
    Denied { path: PathBuf },
    #[error("Human gate unavailable for approval")]
    NoHumanGate,
    #[error("Persistent trust file must be machine-local and outside the workspace: {path}")]
    RepositoryTrustFile { path: PathBuf },
    #[error("Persistent trust is disabled for this sandbox")]
    PersistenceDisabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionType {
    Read,
    Write,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPath {
    pub path: PathBuf,
    pub permission: PermissionType,
    #[serde(with = "serde_ts")]
    pub granted_at: SystemTime,
}

mod serde_ts {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(time.duration_since(UNIX_EPOCH).unwrap().as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(UNIX_EPOCH + std::time::Duration::from_secs(secs))
    }
}

/// Canonicalize a prospective file path even when the leaf (or some parent
/// directories) does not exist yet. This makes the workspace-boundary check in
/// [`PermissionManager::new`] resistant to `..` components and existing
/// symlinked ancestors.
fn resolve_path_allowing_missing_leaf(path: &Path) -> Result<PathBuf, PermissionError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| PermissionError::Resolution(e.to_string()))?
            .join(path)
    };

    let mut missing = Vec::new();
    let mut current = path.as_path();
    loop {
        if current.exists() {
            let mut resolved = current.canonicalize().map_err(|e| {
                PermissionError::Resolution(format!(
                    "Failed to canonicalize trust path ancestor {}: {e}",
                    current.display()
                ))
            })?;
            for component in missing.iter().rev() {
                resolved.push(component);
            }
            return Ok(resolved);
        }
        if let Some(component) = current.file_name() {
            missing.push(component.to_os_string());
        }
        current = current.parent().ok_or_else(|| {
            PermissionError::Resolution(format!(
                "Trust path has no existing ancestor: {}",
                path.display()
            ))
        })?;
    }
}

/// Live, process-wide view of the user-approved out-of-workspace roots.
/// Cloned handles share one underlying set, so a grant recorded here by the
/// permission manager is immediately visible to every consumer that snapshots
/// it — above all the OS exec sandbox, which resolves its filesystem roots
/// per spawned command. "Once" approvals never enter this set.
#[derive(Clone, Default)]
pub struct ApprovedRoots {
    inner: Arc<RwLock<ApprovedRootsInner>>,
}

#[derive(Default)]
struct ApprovedRootsInner {
    read: HashSet<PathBuf>,
    write: HashSet<PathBuf>,
}

impl ApprovedRoots {
    pub fn allow_read(&self, path: PathBuf) {
        self.inner.write().unwrap().read.insert(path);
    }

    pub fn allow_write(&self, path: PathBuf) {
        self.inner.write().unwrap().write.insert(path);
    }

    /// Withdraw a read grant — the scoped counterpart of [`allow_read`](Self::allow_read),
    /// used to honour "Once" approvals: granted for one retry, then removed.
    pub fn remove_read(&self, path: &Path) {
        self.inner.write().unwrap().read.remove(path);
    }

    pub fn read_roots(&self) -> Vec<PathBuf> {
        self.inner.read().unwrap().read.iter().cloned().collect()
    }

    pub fn write_roots(&self) -> Vec<PathBuf> {
        self.inner.read().unwrap().write.iter().cloned().collect()
    }

    /// True when `path` or any ancestor was approved for the permission.
    pub fn is_allowed(&self, path: &Path, perm: PermissionType) -> bool {
        let inner = self.inner.read().unwrap();
        let roots = match perm {
            PermissionType::Read => &inner.read,
            PermissionType::Write => &inner.write,
        };
        let mut current = Some(path);
        while let Some(p) = current {
            if roots.contains(p) {
                return true;
            }
            current = p.parent();
        }
        false
    }

    fn extend(&self, persisted: PersistedPaths) {
        let mut inner = self.inner.write().unwrap();
        inner.read.extend(persisted.read);
        inner.write.extend(persisted.write);
    }
}

/// Manages file access permissions with live ask-then-persist flow
pub struct PermissionManager {
    workspace_root: PathBuf,
    /// Explicit user approvals only. `None` is a hard jail with no persistent
    /// out-of-workspace grants.
    trust_path: Option<PathBuf>,
    audit_path: PathBuf,
    /// In-memory allowlist loaded from the machine-local trust file. A shared
    /// handle: the exec sandbox reads the same set this manager writes.
    trusted: ApprovedRoots,
    /// Human gate for prompting user
    human_gate: Option<Arc<dyn HumanGate>>,
    /// Mutex to serialize prompts (one at a time)
    prompt_mutex: Mutex<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistFailure {
    PartialWrite,
    BeforeRename,
}

#[derive(Default)]
struct PersistedPaths {
    read: HashSet<PathBuf>,
    write: HashSet<PathBuf>,
}

struct RemoveFileOnDrop(Option<PathBuf>);

impl Drop for RemoveFileOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn permission_io(context: impl Into<String>, error: std::io::Error) -> PermissionError {
    PermissionError::Io(format!("{}: {error}", context.into()))
}

fn sibling_name(path: &Path, suffix: &str) -> Result<PathBuf, PermissionError> {
    let parent = path.parent().ok_or_else(|| {
        PermissionError::Io(format!("path has no parent directory: {}", path.display()))
    })?;
    let mut name = path
        .file_name()
        .ok_or_else(|| PermissionError::Io(format!("path has no filename: {}", path.display())))?
        .to_os_string();
    name.push(suffix);
    Ok(parent.join(name))
}

fn open_private(path: &Path, create_new: bool) -> Result<File, PermissionError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|e| permission_io(format!("could not open {}", path.display()), e))
}

fn read_trust_value(path: &Path) -> Result<toml::Value, PermissionError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(permission_io(
                format!("could not read trust file {}", path.display()),
                error,
            ));
        }
    };
    if content.trim().is_empty() {
        Ok(toml::Value::Table(toml::Table::new()))
    } else {
        toml::from_str(&content).map_err(|error| {
            PermissionError::Io(format!(
                "could not parse trust file {}: {error}",
                path.display()
            ))
        })
    }
}

fn collect_persisted_paths(value: &toml::Value) -> Result<PersistedPaths, PermissionError> {
    let root = value
        .as_table()
        .ok_or_else(|| PermissionError::Io("trust file: top-level is not a TOML table".into()))?;
    let Some(permissions) = root.get("permissions") else {
        return Ok(PersistedPaths::default());
    };
    let permissions = permissions
        .as_table()
        .ok_or_else(|| PermissionError::Io("trust file: [permissions] is not a table".into()))?;
    let Some(paths) = permissions.get("trusted_paths") else {
        return Ok(PersistedPaths::default());
    };
    let paths = paths.as_array().ok_or_else(|| {
        PermissionError::Io("trust file: permissions.trusted_paths is not an array".into())
    })?;

    let mut persisted = PersistedPaths::default();
    for (index, trusted) in paths.iter().enumerate() {
        let table = trusted.as_table().ok_or_else(|| {
            PermissionError::Io(format!(
                "trust file: permissions.trusted_paths[{index}] is not a table"
            ))
        })?;
        let raw_path = table
            .get("path")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| {
                PermissionError::Io(format!(
                    "trust file: permissions.trusted_paths[{index}].path is missing or not a string"
                ))
            })?;
        let raw_permission = table
            .get("permission")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| {
                PermissionError::Io(format!(
                    "trust file: permissions.trusted_paths[{index}].permission is missing or not a string"
                ))
            })?;
        let path = PathBuf::from(raw_path);
        if !path.is_absolute() {
            return Err(PermissionError::Io(format!(
                "trust file: permissions.trusted_paths[{index}].path must be absolute"
            )));
        }
        let path = resolve_path_allowing_missing_leaf(&path)?;
        match raw_permission {
            "Read" => {
                persisted.read.insert(path);
            }
            "Write" => {
                persisted.write.insert(path);
            }
            other => {
                return Err(PermissionError::Io(format!(
                    "trust file: permissions.trusted_paths[{index}].permission is invalid: {other}"
                )));
            }
        }
    }
    Ok(persisted)
}

fn atomic_write_trust(
    target: &Path,
    bytes: &[u8],
    failure: Option<PersistFailure>,
) -> Result<(), PermissionError> {
    let temp_path = sibling_name(target, &format!(".tmp-{}", ulid::Ulid::new()))?;
    let mut cleanup = RemoveFileOnDrop(Some(temp_path.clone()));
    let mut temp = open_private(&temp_path, true)?;

    if failure == Some(PersistFailure::PartialWrite) {
        let partial = bytes.len().clamp(1, 16);
        temp.write_all(&bytes[..partial])
            .map_err(|e| permission_io("injected partial trust write", e))?;
        temp.sync_all()
            .map_err(|e| permission_io("could not sync partial trust write", e))?;
        return Err(PermissionError::Io(
            "injected trust-file write failure".into(),
        ));
    }

    temp.write_all(bytes)
        .map_err(|e| permission_io(format!("could not write {}", temp_path.display()), e))?;
    temp.sync_all()
        .map_err(|e| permission_io(format!("could not sync {}", temp_path.display()), e))?;
    drop(temp);

    if failure == Some(PersistFailure::BeforeRename) {
        return Err(PermissionError::Io(
            "injected trust-file rename failure".into(),
        ));
    }

    atomic_replace(&temp_path, target).map_err(|e| {
        permission_io(
            format!(
                "could not atomically replace trust file {}",
                target.display()
            ),
            e,
        )
    })?;
    cleanup.0 = None;
    sync_parent(target)?;
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Durably record the parent directory entry after a rename. Only Unix needs
/// it: Windows publishes through `MoveFileEx` with `MOVEFILE_WRITE_THROUGH`,
/// which already flushes the directory metadata.
fn sync_parent(
    #[cfg_attr(not(unix), allow(unused_variables))] path: &Path,
) -> Result<(), PermissionError> {
    #[cfg(unix)]
    {
        let parent = path.parent().ok_or_else(|| {
            PermissionError::Io(format!("path has no parent directory: {}", path.display()))
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| {
                permission_io(
                    format!("could not sync trust directory {}", parent.display()),
                    e,
                )
            })?;
    }
    Ok(())
}

impl PermissionManager {
    /// Create a permission manager backed by a machine-local trust file.
    ///
    /// `trust_path` must be outside the workspace. This is a security boundary,
    /// not merely a storage convention: a repository-controlled file must never
    /// become a source of prompt-free grants.
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        trust_path: impl Into<PathBuf>,
        audit_path: impl Into<PathBuf>,
    ) -> Result<Self, PermissionError> {
        Self::new_with_roots(
            workspace_root,
            trust_path,
            audit_path,
            ApprovedRoots::default(),
        )
    }

    /// Like [`new`](Self::new), but publishing grants into a caller-supplied
    /// shared handle so other enforcement layers (the OS exec sandbox) honour
    /// the same approvals. Trust-file grants are loaded into it immediately.
    pub fn new_with_roots(
        workspace_root: impl Into<PathBuf>,
        trust_path: impl Into<PathBuf>,
        audit_path: impl Into<PathBuf>,
        trusted: ApprovedRoots,
    ) -> Result<Self, PermissionError> {
        let workspace_root = workspace_root.into();
        let workspace_root = workspace_root.canonicalize().map_err(|e| {
            PermissionError::Resolution(format!("Failed to canonicalize workspace root: {e}"))
        })?;

        let trust_path = resolve_path_allowing_missing_leaf(&trust_path.into())?;
        if trust_path.starts_with(&workspace_root) {
            return Err(PermissionError::RepositoryTrustFile { path: trust_path });
        }
        let audit_path = audit_path.into();

        let mgr = Self {
            workspace_root,
            trust_path: Some(trust_path),
            audit_path,
            trusted,
            human_gate: None,
            prompt_mutex: Mutex::new(()),
        };

        mgr.load_trusted_paths()?;
        Ok(mgr)
    }

    /// Create a hard-jailed manager. It has no persistent trust source, so a
    /// `medha.lock` checked into the workspace cannot grant external access.
    pub fn new_jailed(
        workspace_root: impl Into<PathBuf>,
        audit_path: impl Into<PathBuf>,
    ) -> Result<Self, PermissionError> {
        let workspace_root = workspace_root.into();
        let workspace_root = workspace_root.canonicalize().map_err(|e| {
            PermissionError::Resolution(format!("Failed to canonicalize workspace root: {e}"))
        })?;
        Ok(Self {
            workspace_root,
            trust_path: None,
            audit_path: audit_path.into(),
            trusted: ApprovedRoots::default(),
            human_gate: None,
            prompt_mutex: Mutex::new(()),
        })
    }

    /// Set the human gate for user prompts
    pub fn set_human_gate(&mut self, gate: Arc<dyn HumanGate>) {
        self.human_gate = Some(gate);
    }

    /// The live approval set this manager publishes into.
    pub fn approved_roots(&self) -> ApprovedRoots {
        self.trusted.clone()
    }

    /// Load trusted paths from the machine-local trust file into memory.
    /// Extends rather than replaces: the shared handle may already carry
    /// grants published by another manager on the same trust file.
    fn load_trusted_paths(&self) -> Result<(), PermissionError> {
        let Some(trust_path) = self.trust_path.as_ref() else {
            return Ok(());
        };
        let value = read_trust_value(trust_path)?;
        let persisted = collect_persisted_paths(&value)?;
        self.trusted.extend(persisted);
        Ok(())
    }

    /// Resolve a path for READ/LIST/EDIT operations where the target is expected to exist.
    /// Canonicalizes the full path directly.
    fn resolve_path_for_read(&self, path: &Path) -> Result<PathBuf, PermissionError> {
        // Expand ~ to home directory
        let path = if path.starts_with("~") {
            let home = dirs::home_dir().ok_or_else(|| {
                PermissionError::Resolution("Could not determine home directory".into())
            })?;
            path.strip_prefix("~")
                .map(|p| home.join(p))
                .unwrap_or(path.to_path_buf())
        } else {
            path.to_path_buf()
        };

        // Canonicalize to resolve symlinks and collapse .. - target must exist for read
        path.canonicalize().map_err(|e| {
            PermissionError::Resolution(format!(
                "Failed to canonicalize path {}: {e}",
                path.display()
            ))
        })
    }

    /// Resolve a path for WRITE/CREATE operations where the file may not exist yet.
    /// Splits into (parent_dir, filename), canonicalizes parent (walking up if needed), then re-joins.
    fn resolve_path_for_write(&self, path: &Path) -> Result<PathBuf, PermissionError> {
        // Expand ~ to home directory
        let path = if path.starts_with("~") {
            let home = dirs::home_dir().ok_or_else(|| {
                PermissionError::Resolution("Could not determine home directory".into())
            })?;
            path.strip_prefix("~")
                .map(|p| home.join(p))
                .unwrap_or(path.to_path_buf())
        } else {
            path.to_path_buf()
        };

        // An existing target must be resolved as a whole, not as
        // `canonical-parent + raw-leaf`. The latter leaves a final-component
        // symlink unresolved, so an absolute symlink alias can be authorised
        // and locked under a different identity from its physical target.
        //
        // `exists` deliberately returns false for a dangling symlink. Writes
        // replace such a directory entry rather than following it, so the
        // prospective-path branch below is the correct identity in that case.
        if path.exists() {
            return path.canonicalize().map_err(|e| {
                PermissionError::Resolution(format!(
                    "Failed to canonicalize write target {}: {e}",
                    path.display()
                ))
            });
        }

        // Split into parent directory and filename
        let parent = path.parent().ok_or_else(|| {
            PermissionError::Resolution(format!("Path has no parent directory: {}", path.display()))
        })?;
        let filename = path.file_name().ok_or_else(|| {
            PermissionError::Resolution(format!(
                "Path has no filename component: {}",
                path.display()
            ))
        })?;

        // Canonicalize the nearest existing ancestor, keeping the intermediate
        // components that don't exist yet, then rebuild the full path.
        let (canonical_ancestor, missing) = self.canonicalize_existing_ancestor(parent)?;

        // Re-join: canonical existing ancestor + missing intermediate dirs + filename.
        let mut resolved = canonical_ancestor;
        for component in &missing {
            resolved.push(component);
        }
        resolved.push(filename);
        Ok(resolved)
    }

    /// Find the nearest existing ancestor of a path and canonicalize it, also
    /// returning the trailing components that do *not* exist yet (in
    /// top-to-bottom order) so the caller can re-append them.
    ///
    /// Dropping those components silently retargets the write to a different
    /// file (e.g. `dir/newsub/f.txt` collapsing onto `dir/f.txt`), so they must
    /// be preserved — that was the bug this return value fixes.
    fn canonicalize_existing_ancestor(
        &self,
        path: &Path,
    ) -> Result<(PathBuf, Vec<std::ffi::OsString>), PermissionError> {
        // Walk up until we find an existing directory or hit the filesystem root
        let mut missing: Vec<std::ffi::OsString> = Vec::new();
        let mut current = path;
        loop {
            if current.exists() && current.is_dir() {
                // Found existing directory - canonicalize it
                let canonical = current.canonicalize().map_err(|e| {
                    PermissionError::Resolution(format!(
                        "Failed to canonicalize parent directory {}: {e}",
                        current.display()
                    ))
                })?;
                missing.reverse(); // collected bottom-up → restore top-to-bottom
                return Ok((canonical, missing));
            }
            if let Some(name) = current.file_name() {
                missing.push(name.to_os_string());
            }
            match current.parent() {
                Some(p) if p != current => current = p,
                _ => break, // Hit filesystem root
            }
        }

        // No existing ancestor found - this shouldn't happen for valid paths,
        // but as fallback, try to canonicalize the original path
        // (may fail if it doesn't exist, which is correct fail-closed behavior).
        let canonical = path.canonicalize().map_err(|e| {
            PermissionError::Resolution(format!(
                "Failed to canonicalize parent directory {}: {e}",
                path.display()
            ))
        })?;
        Ok((canonical, Vec::new()))
    }

    /// Check if a resolved path is inside the workspace root
    fn is_inside_workspace(&self, resolved_path: &Path) -> bool {
        resolved_path.starts_with(&self.workspace_root)
    }

    /// Grant prompt-free READ access to a harness-owned directory for this
    /// process only (in-memory; never persisted to the trust file). Exists for
    /// roots like the user skills dir: skills bundle reference files the model
    /// must read on demand, and a permission dialog per file would break them.
    /// Writes stay fully gated.
    pub fn allow_read_dir(&self, dir: &Path) {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        self.trusted.allow_read(dir);
    }

    /// Check if a path (or its parent) is trusted for the given permission
    fn is_trusted(&self, resolved_path: &Path, perm: PermissionType) -> bool {
        self.trusted.is_allowed(resolved_path, perm)
    }

    /// Add a path to the trusted set. The durable transaction must complete
    /// before the live allowlist is expanded: a failed "Always" decision must
    /// never become a process-local grant that vanishes on restart.
    fn trust_path(
        &self,
        resolved_path: PathBuf,
        perm: PermissionType,
    ) -> Result<(), PermissionError> {
        self.trust_path_with_failure(resolved_path, perm, None)
    }

    fn trust_path_with_failure(
        &self,
        resolved_path: PathBuf,
        perm: PermissionType,
        failure: Option<PersistFailure>,
    ) -> Result<(), PermissionError> {
        let persisted = self.persist_trusted_path(&resolved_path, perm, failure)?;
        self.trusted.extend(persisted);
        Ok(())
    }

    /// Persist a trusted path to the machine-local trust file. The stable
    /// sibling lock spans read-modify-write, file fsync, atomic replacement,
    /// and parent-directory fsync, so independent Medha processes cannot lose
    /// one another's grants or observe a truncated TOML document.
    fn persist_trusted_path(
        &self,
        path: &Path,
        perm: PermissionType,
        failure: Option<PersistFailure>,
    ) -> Result<PersistedPaths, PermissionError> {
        let trust_path = self
            .trust_path
            .as_ref()
            .ok_or(PermissionError::PersistenceDisabled)?;
        let parent = trust_path.parent().ok_or_else(|| {
            PermissionError::Io(format!(
                "trust file has no parent directory: {}",
                trust_path.display()
            ))
        })?;
        std::fs::create_dir_all(parent).map_err(|e| {
            PermissionError::Io(format!(
                "could not create trust directory {}: {e}",
                parent.display()
            ))
        })?;

        // Re-resolve immediately before opening. A missing leaf remains stable;
        // a symlink planted after startup resolves elsewhere and is rejected.
        let current = resolve_path_allowing_missing_leaf(trust_path)?;
        if &current != trust_path {
            return Err(PermissionError::Resolution(format!(
                "trust path identity changed after startup: {} became {}",
                trust_path.display(),
                current.display()
            )));
        }

        let lock_path = sibling_name(trust_path, ".medha-write-lock")?;
        let lock_file = open_private(&lock_path, false)?;
        let mut lock = FileRwLock::new(lock_file);
        let _guard = lock.write().map_err(|e| {
            PermissionError::Io(format!(
                "could not lock trust file {}: {e}",
                trust_path.display()
            ))
        })?;

        let mut value = read_trust_value(trust_path)?;

        // Ensure permissions table exists. A hand-edited/corrupt local trust
        // file must return an error, not panic and take down the whole agent on
        // the next "Always" approval.
        let permissions = value
            .as_table_mut()
            .ok_or_else(|| PermissionError::Io("trust file: top-level is not a TOML table".into()))?
            .entry("permissions")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));

        let permissions_table = permissions.as_table_mut().ok_or_else(|| {
            PermissionError::Io("trust file: [permissions] is not a table".into())
        })?;

        // Ensure trusted_paths array exists
        let trusted_paths = permissions_table
            .entry("trusted_paths")
            .or_insert_with(|| toml::Value::Array(vec![]));

        let trusted_paths_array = trusted_paths.as_array_mut().ok_or_else(|| {
            PermissionError::Io("trust file: permissions.trusted_paths is not an array".into())
        })?;

        // Add a new trusted path only if this exact grant is not already in the
        // transaction snapshot.
        let perm_str = match perm {
            PermissionType::Read => "Read",
            PermissionType::Write => "Write",
        };
        let path_str = path.to_str().ok_or_else(|| {
            PermissionError::Io(format!(
                "trusted path is not valid Unicode and cannot be persisted exactly: {}",
                path.display()
            ))
        })?;
        let already_present = trusted_paths_array.iter().any(|entry| {
            entry.get("path").and_then(toml::Value::as_str) == Some(path_str)
                && entry.get("permission").and_then(toml::Value::as_str) == Some(perm_str)
        });
        if !already_present {
            trusted_paths_array.push(toml::Value::Table({
                let mut table = toml::Table::new();
                table.insert("path".into(), toml::Value::String(path_str.to_owned()));
                table.insert("permission".into(), toml::Value::String(perm_str.into()));
                table.insert(
                    "granted_at".into(),
                    toml::Value::Integer(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|e| PermissionError::Io(e.to_string()))?
                            .as_secs() as i64,
                    ),
                );
                table
            }));
        }

        let new_content =
            toml::to_string_pretty(&value).map_err(|e| PermissionError::Io(e.to_string()))?;
        atomic_write_trust(trust_path, new_content.as_bytes(), failure)?;
        collect_persisted_paths(&value)
    }

    /// Log an access attempt to audit log
    fn audit_log(
        &self,
        requested_path: &Path,
        resolved_path: &Path,
        permission: PermissionType,
        decision: &str,
    ) -> Result<(), PermissionError> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let log_entry = format!(
            "{timestamp} | {permission:?} | requested={} | resolved={} | decision={}\n",
            requested_path.display(),
            resolved_path.display(),
            decision
        );

        // The log's directory may not exist yet (fresh state dir); a missing
        // parent must not turn an approved access into a failure.
        if let Some(parent) = self.audit_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| PermissionError::Io(e.to_string()))?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_path)
            .map_err(|e| PermissionError::Io(e.to_string()))?
            .write_all(log_entry.as_bytes())
            .map_err(|e| PermissionError::Io(e.to_string()))
    }

    /// Resolve a path only if it is *already* permitted — inside the workspace,
    /// or previously trusted. Returns `None` where [`request_permission`] would
    /// have asked the user.
    ///
    /// For callers that must not interrupt, above all previews: a preview runs
    /// before the approval card is shown, so prompting there asks the user to
    /// authorise a path before telling them what it is for, and then asks again
    /// when the operation actually runs.
    pub fn resolve_if_permitted(&self, path: &Path, permission: PermissionType) -> Option<PathBuf> {
        let resolved = match permission {
            PermissionType::Read => self.resolve_path_for_read(path).ok()?,
            PermissionType::Write => self.resolve_path_for_write(path).ok()?,
        };
        (self.is_inside_workspace(&resolved) || self.is_trusted(&resolved, permission))
            .then_some(resolved)
    }

    /// Request permission for a path (the main entry point)
    pub async fn request_permission(
        &self,
        path: &Path,
        permission: PermissionType,
    ) -> Result<PathBuf, PermissionError> {
        self.request_permission_with_detail(path, permission, None)
            .await
    }

    /// Like [`request_permission`](Self::request_permission), with a custom
    /// prompt detail for callers that have better context than the default
    /// wording — e.g. the exec sandbox naming the blocked command.
    pub async fn request_permission_with_detail(
        &self,
        path: &Path,
        permission: PermissionType,
        detail: Option<&str>,
    ) -> Result<PathBuf, PermissionError> {
        // Step 1: RESOLVE the target path fully
        // Use different resolution strategy based on permission type:
        // - READ: canonicalize full path (target must exist)
        // - WRITE: canonicalize parent dir, then re-join (file may not exist)
        let resolved = match permission {
            PermissionType::Read => self.resolve_path_for_read(path)?,
            PermissionType::Write => self.resolve_path_for_write(path)?,
        };

        // Step 2: IF resolved path is inside workspace root → allow immediately
        if self.is_inside_workspace(&resolved) {
            self.audit_log(path, &resolved, permission, "allowed (workspace)")?;
            return Ok(resolved);
        }

        // Step 3: IF outside workspace → check trusted paths
        if self.is_trusted(&resolved, permission) {
            self.audit_log(path, &resolved, permission, "allowed (trusted)")?;
            return Ok(resolved);
        }

        // Step 4: Not trusted → prompt user via HumanGate
        let _guard = self.prompt_mutex.lock().await; // Serialize prompts

        // Another concurrent request may have received "Always" while this
        // request waited for the prompt lane. Recheck under the lane before
        // showing a duplicate approval dialog.
        if self.is_trusted(&resolved, permission) {
            self.audit_log(
                path,
                &resolved,
                permission,
                "allowed (trusted while prompt queued)",
            )?;
            return Ok(resolved);
        }

        let human_gate = self
            .human_gate
            .as_ref()
            .ok_or(PermissionError::NoHumanGate)?;

        // The surface (TUI/terminal) renders the selectable options; keep the
        // detail to just the explanation so it isn't duplicated.
        let prompt = match detail {
            Some(detail) => detail.to_string(),
            None => format!("This path is outside the workspace: {}", resolved.display()),
        };

        // Use the human gate to get the user's decision (allow once / always / deny).
        let decision = human_gate
            .confirm(
                &format!("{permission:?} access to {}", resolved.display()),
                Some(&prompt),
                false, // an out-of-workspace path prompt is not a trust-flow escalation
            )
            .await;

        match decision {
            kernel::Approval::Deny => {
                self.audit_log(path, &resolved, permission, "denied")?;
                Err(PermissionError::Denied { path: resolved })
            }
            kernel::Approval::Once => {
                // Allow this operation only; do not persist to machine-local trust.
                self.audit_log(path, &resolved, permission, "allowed (user approved, once)")?;
                Ok(resolved)
            }
            kernel::Approval::Always => {
                // Persist exactly this resolved path and permission type.
                self.trust_path(resolved.clone(), permission)?;
                self.audit_log(
                    path,
                    &resolved,
                    permission,
                    "allowed (user approved, persisted)",
                )?;
                Ok(resolved)
            }
        }
    }

    /// Convenience method for read permission
    pub async fn request_read(&self, path: &Path) -> Result<PathBuf, PermissionError> {
        self.request_permission(path, PermissionType::Read).await
    }

    /// Convenience method for write permission
    pub async fn request_write(&self, path: &Path) -> Result<PathBuf, PermissionError> {
        self.request_permission(path, PermissionType::Write).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use kernel::Approval;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    /// Mock gate that always returns a fixed decision.
    struct FixedGate(Approval);
    #[async_trait::async_trait]
    impl HumanGate for FixedGate {
        async fn confirm(
            &self,
            _action: &str,
            _detail: Option<&str>,
            _escalated: bool,
        ) -> Approval {
            self.0
        }
    }

    fn unique_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("medha_perm_{tag}_{}_{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn machine_trust_file(tag: &str) -> PathBuf {
        unique_dir(&format!("state_{tag}")).join("trust.lock")
    }

    #[test]
    fn persistence_child() {
        if std::env::var_os("MEDHA_PERMISSION_CHILD").is_none() {
            return;
        }
        let workspace = PathBuf::from(std::env::var_os("MEDHA_TEST_WORKSPACE").unwrap());
        let trust = PathBuf::from(std::env::var_os("MEDHA_TEST_TRUST").unwrap());
        let grant = PathBuf::from(std::env::var_os("MEDHA_TEST_GRANT").unwrap());
        let ready = PathBuf::from(std::env::var_os("MEDHA_TEST_READY").unwrap());
        let start = PathBuf::from(std::env::var_os("MEDHA_TEST_START").unwrap());
        let permission = match std::env::var("MEDHA_TEST_PERMISSION").unwrap().as_str() {
            "Read" => PermissionType::Read,
            "Write" => PermissionType::Write,
            other => panic!("invalid child permission {other}"),
        };

        let manager =
            PermissionManager::new(&workspace, &trust, workspace.join("child-audit.log")).unwrap();
        std::fs::write(&ready, b"ready").unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !start.exists() {
            assert!(
                Instant::now() < deadline,
                "parent never released persistence barrier"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        manager.trust_path(grant, permission).unwrap();
    }

    /// "Allow once" must NOT persist to machine-local trust, so the next run re-asks.
    #[tokio::test]
    async fn allow_once_does_not_persist() {
        let ws = unique_dir("ws_once");
        let outside = unique_dir("out_once");
        std::fs::write(outside.join("f.txt"), "x").unwrap();
        let trust = machine_trust_file("once");
        let audit = ws.join("audit.log");
        let target = outside.join("f.txt");

        let mut mgr = PermissionManager::new(&ws, &trust, &audit).unwrap();
        mgr.set_human_gate(Arc::new(FixedGate(Approval::Once)));
        assert!(mgr.request_read(&target).await.is_ok(), "once should allow");
        assert!(
            !trust.exists() || std::fs::read_to_string(&trust).unwrap().trim().is_empty(),
            "allow-once must not write to machine-local trust"
        );

        // A fresh manager has no trust for it → a Deny gate now blocks it.
        let mut mgr2 = PermissionManager::new(&ws, &trust, &audit).unwrap();
        mgr2.set_human_gate(Arc::new(FixedGate(Approval::Deny)));
        assert!(
            mgr2.request_read(&target).await.is_err(),
            "should re-ask, not silently allow"
        );
    }

    /// An "Always" grant must be visible through a cloned [`ApprovedRoots`]
    /// handle immediately — that handle is what the OS exec sandbox snapshots
    /// per spawn. "Once" must never appear there; trust-file grants must land
    /// in it at construction.
    #[tokio::test]
    async fn grants_publish_into_the_shared_approved_roots_handle() {
        let ws = unique_dir("ws_shared");
        let outside = unique_dir("out_shared");
        std::fs::write(outside.join("f.txt"), "x").unwrap();
        let trust = machine_trust_file("shared");
        let audit = ws.join("audit.log");
        let target = outside.join("f.txt");
        let canonical_outside = outside.canonicalize().unwrap();

        let shared = ApprovedRoots::default();
        let mut mgr =
            PermissionManager::new_with_roots(&ws, &trust, &audit, shared.clone()).unwrap();
        mgr.set_human_gate(Arc::new(FixedGate(Approval::Once)));
        assert!(mgr.request_read(&target).await.is_ok());
        assert!(
            shared.read_roots().is_empty(),
            "a Once approval must not reach the exec sandbox's roots"
        );

        mgr.set_human_gate(Arc::new(FixedGate(Approval::Always)));
        assert!(mgr.request_read(&target).await.is_ok());
        assert!(
            shared.is_allowed(&canonical_outside.join("f.txt"), PermissionType::Read),
            "an Always approval must be live in the shared handle"
        );

        // A fresh manager on the same trust file seeds a fresh handle from it.
        let reloaded = ApprovedRoots::default();
        let _mgr2 =
            PermissionManager::new_with_roots(&ws, &trust, &audit, reloaded.clone()).unwrap();
        assert!(
            reloaded.is_allowed(&canonical_outside.join("f.txt"), PermissionType::Read),
            "trust-file grants must load into the shared handle at construction"
        );
    }

    /// "Always allow" persists to machine-local trust and is trusted on reload.
    #[tokio::test]
    async fn always_allow_persists_and_reloads() {
        let ws = unique_dir("ws_always");
        let outside = unique_dir("out_always");
        std::fs::write(outside.join("f.txt"), "x").unwrap();
        let trust = machine_trust_file("always");
        let audit = ws.join("audit.log");
        let target = outside.join("f.txt");

        let mut mgr = PermissionManager::new(&ws, &trust, &audit).unwrap();
        mgr.set_human_gate(Arc::new(FixedGate(Approval::Always)));
        assert!(mgr.request_read(&target).await.is_ok());
        assert!(
            std::fs::read_to_string(&trust)
                .unwrap()
                .contains("trusted_paths")
        );

        // Fresh manager with NO gate: must trust the explicit local grant.
        let mgr2 = PermissionManager::new(&ws, &trust, &audit).unwrap();
        assert!(
            mgr2.request_read(&target).await.is_ok(),
            "persisted path should be trusted on reload"
        );
    }

    #[test]
    fn failed_persistence_never_publishes_an_in_memory_grant() {
        let ws = unique_dir("ws_persist_failure");
        let outside = unique_dir("out_persist_failure");
        let trust = machine_trust_file("persist_failure");
        let manager = PermissionManager::new(&ws, &trust, ws.join("audit.log")).unwrap();

        let existing = outside.join("existing.txt");
        let partial = outside.join("partial.txt");
        let rename = outside.join("rename.txt");
        for path in [&existing, &partial, &rename] {
            std::fs::write(path, b"x").unwrap();
        }
        let existing = existing.canonicalize().unwrap();
        let partial = partial.canonicalize().unwrap();
        let rename = rename.canonicalize().unwrap();
        manager
            .trust_path(existing.clone(), PermissionType::Read)
            .unwrap();
        let before = std::fs::read(&trust).unwrap();

        assert!(
            manager
                .trust_path_with_failure(
                    partial.clone(),
                    PermissionType::Read,
                    Some(PersistFailure::PartialWrite),
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&trust).unwrap(), before);
        assert!(!manager.is_trusted(&partial, PermissionType::Read));

        assert!(
            manager
                .trust_path_with_failure(
                    rename.clone(),
                    PermissionType::Read,
                    Some(PersistFailure::BeforeRename),
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&trust).unwrap(), before);
        assert!(!manager.is_trusted(&rename, PermissionType::Read));
        assert!(manager.is_trusted(&existing, PermissionType::Read));

        let leftovers: Vec<_> = std::fs::read_dir(trust.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary trust files leaked");

        let reloaded = PermissionManager::new(&ws, &trust, ws.join("audit-reloaded.log")).unwrap();
        assert!(reloaded.is_trusted(&existing, PermissionType::Read));
        assert!(!reloaded.is_trusted(&partial, PermissionType::Read));
        assert!(!reloaded.is_trusted(&rename, PermissionType::Read));
    }

    #[test]
    fn concurrent_processes_preserve_every_trust_grant() {
        let root = unique_dir("concurrent_processes").canonicalize().unwrap();
        let workspace = root.join("workspace");
        let state = root.join("state");
        let grants = root.join("grants");
        let ready = root.join("ready");
        for directory in [&workspace, &state, &grants, &ready] {
            std::fs::create_dir(directory).unwrap();
        }
        let trust = state.join("trust.toml");
        let start = root.join("start");
        let executable = std::env::current_exe().unwrap();
        let count = 8;
        let mut children = Vec::new();

        for index in 0..count {
            let grant = grants.join(format!("grant-{index}"));
            let ready_file = ready.join(index.to_string());
            let mut command = Command::new(&executable);
            command
                .arg("--exact")
                .arg("tests::persistence_child")
                .arg("--nocapture")
                .env("MEDHA_PERMISSION_CHILD", "1")
                .env("MEDHA_TEST_WORKSPACE", &workspace)
                .env("MEDHA_TEST_TRUST", &trust)
                .env("MEDHA_TEST_GRANT", &grant)
                .env("MEDHA_TEST_READY", ready_file)
                .env("MEDHA_TEST_START", &start)
                .env(
                    "MEDHA_TEST_PERMISSION",
                    if index % 2 == 0 { "Read" } else { "Write" },
                );
            children.push(command.spawn().unwrap());
        }

        let ready_deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let ready_count = std::fs::read_dir(&ready).unwrap().count();
            if ready_count == count {
                break;
            }
            if Instant::now() >= ready_deadline {
                for child in &mut children {
                    let _ = child.kill();
                }
                panic!("only {ready_count}/{count} trust writers reached the barrier");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(&start, b"go").unwrap();

        let finish_deadline = Instant::now() + Duration::from_secs(30);
        let mut statuses = vec![None; count];
        while statuses.iter().any(Option::is_none) && Instant::now() < finish_deadline {
            for (status, child) in statuses.iter_mut().zip(&mut children) {
                if status.is_none() {
                    *status = child.try_wait().unwrap();
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        for (index, (status, child)) in statuses.iter().zip(&mut children).enumerate() {
            if status.is_none() {
                let _ = child.kill();
                panic!("trust writer {index} did not settle");
            }
            assert!(status.unwrap().success(), "trust writer {index} failed");
        }

        let value = read_trust_value(&trust).unwrap();
        let persisted = collect_persisted_paths(&value).unwrap();
        assert_eq!(persisted.read.len(), count / 2);
        assert_eq!(persisted.write.len(), count / 2);
        for index in 0..count {
            let grant = grants.join(format!("grant-{index}"));
            let set = if index % 2 == 0 {
                &persisted.read
            } else {
                &persisted.write
            };
            assert!(set.contains(&grant), "grant {index} was lost");
        }
        toml::from_str::<toml::Value>(&std::fs::read_to_string(&trust).unwrap())
            .expect("concurrent readers must never observe truncated TOML");
    }

    /// Counts prompts, so "asked once" is measured rather than assumed.
    struct CountingGate(Arc<AtomicU32>, Approval);
    #[async_trait::async_trait]
    impl HumanGate for CountingGate {
        async fn confirm(&self, _: &str, _: Option<&str>, _: bool) -> Approval {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1
        }
    }

    struct BlockingFirstAlwaysGate {
        asked: Arc<AtomicU32>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl HumanGate for BlockingFirstAlwaysGate {
        async fn confirm(&self, _: &str, _: Option<&str>, _: bool) -> Approval {
            if self.asked.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Approval::Always
        }
    }

    struct RecordingGate {
        actions: Arc<std::sync::Mutex<Vec<String>>>,
        decision: Approval,
    }

    #[async_trait::async_trait]
    impl HumanGate for RecordingGate {
        async fn confirm(&self, action: &str, _: Option<&str>, _: bool) -> Approval {
            self.actions.lock().unwrap().push(action.to_string());
            self.decision
        }
    }

    /// First run of a clone: even a portable grant for `/` remains inert. The
    /// actual target is presented to the gate and denial creates no local trust.
    #[tokio::test]
    async fn first_run_never_imports_repository_permission_grants() {
        let ws = unique_dir("ws_repo_grant");
        let outside = unique_dir("out_repo_grant");
        let target = outside.join("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        let portable = ws.join("medha.lock");
        let portable_contents = "[[permissions.trusted_paths]]\n\
                                 path = \"/\"\n\
                                 permission = \"Read\"\n\
                                 granted_at = 123\n";
        std::fs::write(&portable, portable_contents).unwrap();

        let trust = machine_trust_file("first_run");
        let actions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mgr =
            PermissionManager::new(&ws, &trust, ws.join("audit.log")).expect("local trust path");
        mgr.set_human_gate(Arc::new(RecordingGate {
            actions: actions.clone(),
            decision: Approval::Deny,
        }));

        assert!(matches!(
            mgr.request_read(&target).await,
            Err(PermissionError::Denied { .. })
        ));
        assert!(
            !trust.exists(),
            "denial must not create machine-local trust"
        );
        assert_eq!(
            std::fs::read_to_string(&portable).unwrap(),
            portable_contents,
            "startup must not rewrite or migrate repository input"
        );
        assert_eq!(
            actions.lock().unwrap().as_slice(),
            [format!(
                "Read access to {}",
                target.canonicalize().unwrap().display()
            )],
            "approval must identify the exact resolved path"
        );
    }

    /// Existing machine-local trust remains valid, but stays path-specific even
    /// when the repository claims a much broader `/` grant.
    #[tokio::test]
    async fn existing_machine_local_trust_is_preserved_and_path_specific() {
        let ws = unique_dir("ws_existing_trust");
        let outside = unique_dir("out_existing_trust");
        let approved = outside.join("approved.txt");
        let sibling = outside.join("not-approved.txt");
        std::fs::write(&approved, "approved").unwrap();
        std::fs::write(&sibling, "private").unwrap();
        let trust = machine_trust_file("existing");
        let audit = ws.join("audit.log");

        // This models the only valid source of durable authority: a prior,
        // explicit `Always` decision for one resolved path.
        let mut first = PermissionManager::new(&ws, &trust, &audit).unwrap();
        first.set_human_gate(Arc::new(FixedGate(Approval::Always)));
        first.request_read(&approved).await.unwrap();
        let trust_contents = std::fs::read_to_string(&trust).unwrap();

        // A later checkout can claim `/`, but that portable claim is irrelevant.
        std::fs::write(
            ws.join("medha.lock"),
            "[[permissions.trusted_paths]]\n\
             path = \"/\"\n\
             permission = \"Read\"\n\
             granted_at = 123\n",
        )
        .unwrap();

        let asked = Arc::new(AtomicU32::new(0));
        let mut reloaded = PermissionManager::new(&ws, &trust, &audit).unwrap();
        reloaded.set_human_gate(Arc::new(CountingGate(asked.clone(), Approval::Deny)));

        reloaded
            .request_read(&approved)
            .await
            .expect("existing local grant should reload");
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        assert!(matches!(
            reloaded.request_read(&sibling).await,
            Err(PermissionError::Denied { .. })
        ));
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read_to_string(&trust).unwrap(),
            trust_contents,
            "repository input and denied requests must not alter existing trust"
        );
    }

    #[test]
    fn repository_path_is_rejected_as_a_persistent_trust_source() {
        let ws = unique_dir("ws_trust_boundary");
        let result = PermissionManager::new(&ws, ws.join("medha.lock"), ws.join("audit.log"));
        match result {
            Err(PermissionError::RepositoryTrustFile { path }) => {
                assert!(path.starts_with(ws.canonicalize().unwrap()));
            }
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("repository-controlled trust source was accepted"),
        }
    }

    /// "Always allow" must stop asking immediately, not only after a restart.
    #[tokio::test]
    async fn always_allow_stops_asking_within_the_same_session() {
        let ws = unique_dir("ws_session");
        let outside = unique_dir("out_session");
        std::fs::write(outside.join("f.txt"), "x").unwrap();
        let target = outside.join("f.txt");

        let asked = Arc::new(AtomicU32::new(0));
        let mut mgr =
            PermissionManager::new(&ws, machine_trust_file("session"), ws.join("audit.log"))
                .unwrap();
        mgr.set_human_gate(Arc::new(CountingGate(asked.clone(), Approval::Always)));

        for _ in 0..3 {
            assert!(mgr.request_read(&target).await.is_ok());
        }
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "'always' must persist into the in-memory trust set, not just the trust file"
        );
    }

    #[tokio::test]
    async fn concurrent_identical_requests_share_the_first_persistent_approval() {
        let ws = unique_dir("ws_concurrent_prompt");
        let outside = unique_dir("out_concurrent_prompt");
        let target = outside.join("f.txt");
        std::fs::write(&target, "x").unwrap();

        let asked = Arc::new(AtomicU32::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut manager = PermissionManager::new(
            &ws,
            machine_trust_file("concurrent_prompt"),
            ws.join("audit.log"),
        )
        .unwrap();
        manager.set_human_gate(Arc::new(BlockingFirstAlwaysGate {
            asked: asked.clone(),
            entered: entered.clone(),
            release: release.clone(),
        }));
        let manager = Arc::new(manager);

        let first_manager = manager.clone();
        let first_target = target.clone();
        let first = tokio::spawn(async move { first_manager.request_read(&first_target).await });
        entered.notified().await;

        let second_manager = manager.clone();
        let second_target = target.clone();
        let second = tokio::spawn(async move { second_manager.request_read(&second_target).await });
        // Let the second request resolve and queue behind the prompt mutex while
        // the first decision is deliberately held open.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        release.notify_one();

        assert!(first.await.unwrap().is_ok());
        assert!(second.await.unwrap().is_ok());
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "the queued request must recheck trust instead of prompting again"
        );
    }

    /// The stored path and the queried path both come from `canonicalize`, so
    /// they must compare equal. On Windows that means both carry the `\\?\`
    /// verbatim prefix; if one side kept it and the other did not, every lookup
    /// would miss and "always allow" would silently behave like "allow once".
    #[tokio::test]
    async fn a_trusted_directory_covers_files_beneath_it() {
        let ws = unique_dir("ws_tree");
        let outside = unique_dir("out_tree");
        let nested = outside.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("deep.txt"), "x").unwrap();

        let asked = Arc::new(AtomicU32::new(0));
        let mut mgr =
            PermissionManager::new(&ws, machine_trust_file("tree"), ws.join("audit.log")).unwrap();
        mgr.set_human_gate(Arc::new(CountingGate(asked.clone(), Approval::Always)));

        // Trust the root, then read something several levels below it.
        assert!(mgr.request_read(&outside).await.is_ok());
        assert!(mgr.request_read(&nested.join("deep.txt")).await.is_ok());
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "trusting a directory must cover its descendants"
        );
    }

    /// Read trust never grants write trust (permissions tracked separately).
    #[tokio::test]
    async fn read_trust_does_not_grant_write() {
        let ws = unique_dir("ws_sep");
        let outside = unique_dir("out_sep");
        std::fs::write(outside.join("f.txt"), "x").unwrap();
        let trust = machine_trust_file("separate");
        let audit = ws.join("audit.log");
        let target = outside.join("f.txt");

        let mut mgr = PermissionManager::new(&ws, &trust, &audit).unwrap();
        mgr.set_human_gate(Arc::new(FixedGate(Approval::Always)));
        assert!(mgr.request_read(&target).await.is_ok());

        // Reload; write should still be untrusted → a Deny gate blocks it.
        let mut mgr2 = PermissionManager::new(&ws, &trust, &audit).unwrap();
        mgr2.set_human_gate(Arc::new(FixedGate(Approval::Deny)));
        assert!(
            mgr2.request_write(&target).await.is_err(),
            "read trust must not grant write"
        );
    }

    /// A write target under not-yet-existing nested subdirectories must resolve
    /// to the FULL path — the intermediate components must not be dropped (which
    /// would silently retarget the write onto an existing ancestor).
    #[test]
    fn write_resolution_preserves_nonexistent_intermediate_dirs() {
        let ws = unique_dir("ws_nested");
        let trust = machine_trust_file("nested");
        let audit = ws.join("audit.log");
        // <ws>/deep/sub/dir/file.txt — none of deep/sub/dir exist yet.
        let target = ws.join("deep").join("sub").join("dir").join("file.txt");

        let mgr = PermissionManager::new(&ws, &trust, &audit).unwrap();
        let resolved = mgr.resolve_path_for_write(&target).unwrap();

        // The canonical <ws> prefix may differ (e.g. /var → /private/var on
        // macOS), so assert on the preserved tail rather than the whole path.
        assert!(
            resolved.ends_with("deep/sub/dir/file.txt"),
            "intermediate dirs were dropped: {resolved:?}"
        );
    }
}
