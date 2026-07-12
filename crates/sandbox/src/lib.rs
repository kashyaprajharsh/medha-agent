//! Execution backends behind one interface (§4.8). Phase 0 ships the
//! `workspace` backend: path-jailed file ops with snapshot-before-write so
//! every mutation is reversible (the basis for `medha undo`). Container/microVM
//! backends are added later behind this same surface (P8).
//!
//! The new permission system allows legitimate access to files outside the
//! workspace via a live ask-then-persist flow (see issues.txt).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use kernel::HumanGate;
use permissions::PermissionManager;

pub mod exec;
pub use exec::{
    BackendKind, ExecBackend, ExecError, ExecOutput, ExecRequest, HostBackend, NetPolicy,
    SandboxConfig, native_backend_available, program_on_path, select_backend,
};

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("path escapes the workspace jail: {0}")]
    Escape(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("permission system error: {0}")]
    Permission(#[from] permissions::PermissionError),
}

/// A workspace sandbox with permission management for out-of-workspace access.
pub struct WorkspaceSandbox {
    root: PathBuf,
    snapshots: PathBuf,
    permission_manager: Arc<PermissionManager>,
    /// Backend that runs shell/build/VCS commands (host or OS-native jail).
    exec: Arc<dyn ExecBackend>,
    /// Per-path write locks (P0-4): serialize concurrent read-modify-write on the
    /// same file so two same-turn edits can't both read the original and clobber
    /// each other (last-write-wins, silent loss, corrupted snapshot chain).
    /// Different paths never contend.
    write_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl WorkspaceSandbox {
    /// Create a new sandbox with permission management.
    /// 
    /// The lock_path should point to medha.lock, audit_path to medha_audit.log
    pub fn new(
        root: impl Into<PathBuf>,
        lock_path: impl Into<PathBuf>,
        audit_path: impl Into<PathBuf>,
        human_gate: Option<Arc<dyn HumanGate>>,
    ) -> Result<Self, SandboxError> {
        let root = root.into();
        let root = root.canonicalize().unwrap_or(root);
        let snapshots = root.join(".medha").join("snapshots");

        let mut permission_manager = PermissionManager::new(&root, lock_path, audit_path)?;
        if let Some(gate) = human_gate {
            permission_manager.set_human_gate(gate);
        }

        Ok(Self {
            root,
            snapshots,
            permission_manager: Arc::new(permission_manager),
            exec: Arc::new(HostBackend),
            write_locks: Mutex::new(HashMap::new()),
        })
    }

    /// Create a new sandbox without permission management (backward compatible).
    /// This maintains the old behavior - hard jail with no out-of-workspace access.
    pub fn new_jailed(root: impl Into<PathBuf>) -> Result<Self, SandboxError> {
        let root = root.into();
        let root = root.canonicalize().unwrap_or(root);
        let snapshots = root.join(".medha").join("snapshots");

        // Create a permission manager with no human gate - will deny all out-of-workspace
        let permission_manager = PermissionManager::new(&root, root.join("medha.lock"), root.join("medha_audit.log"))?;
        // No human gate set = will deny all external access

        Ok(Self {
            root,
            snapshots,
            permission_manager: Arc::new(permission_manager),
            exec: Arc::new(HostBackend),
            write_locks: Mutex::new(HashMap::new()),
        })
    }

    /// Acquire the write lock for `path` (P0-4). Hold the returned guard across a
    /// read-modify-write (as `fs.edit` / `multi_edit` / `fs.write` do) so two
    /// concurrent edits to the *same* file serialize instead of clobbering each
    /// other. Keyed on the raw path string — the model uses a consistent spelling
    /// for a file within a turn, so same-file calls share the lock; distinct
    /// files get distinct locks and run in parallel.
    pub async fn path_guard(&self, path: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.write_locks.lock().expect("write_locks poisoned");
            locks.entry(path.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
        };
        lock.lock_owned().await
    }

    /// Install the execution backend used by shell/build/VCS tools. Defaults to
    /// [`HostBackend`]; the CLI swaps in the OS-native jail per `medha.lock`.
    pub fn with_exec_backend(mut self, backend: Arc<dyn ExecBackend>) -> Self {
        self.exec = backend;
        self
    }

    /// Relocate the undo-snapshot directory out of the workspace. By default
    /// snapshots live at `<root>/.medha/snapshots`; the CLI points this at the
    /// per-workspace state dir (`~/.medha/projects/<enc>/snapshots`) so runtime
    /// state never lands in the working tree. The jail `root` is unchanged.
    pub fn with_snapshots_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.snapshots = dir.into();
        self
    }

    /// The label of the active execution backend (`"host"` / `"native"`).
    pub fn exec_backend_label(&self) -> &str {
        self.exec.label()
    }

    /// How strongly the active backend confines commands (§4.8) — read by the
    /// kernel's trust-flow escalation.
    pub fn containment(&self) -> kernel::Containment {
        self.exec.containment()
    }

    /// Run a command through the active execution backend, rooted at the
    /// workspace. `clear_env` starts the child from an empty environment (used
    /// by `shell.exec`); fixed-program tools pass `false` to inherit.
    pub async fn exec(
        &self,
        program: &str,
        args: &[String],
        env: Vec<(String, String)>,
        clear_env: bool,
    ) -> Result<ExecOutput, ExecError> {
        self.exec
            .run(ExecRequest {
                program: program.to_string(),
                args: args.to_vec(),
                cwd: self.root.clone(),
                env,
                clear_env,
            })
            .await
    }

    /// Spawn a command as a background task through the active backend (same
    /// jail as [`exec`]). Returns immediately with a [`BgProc`] handle whose
    /// output streams into a rolling buffer; the process keeps running until it
    /// exits or is killed. Backs `shell.exec`'s promote-to-background behavior.
    pub fn exec_background(
        &self,
        program: &str,
        args: &[String],
        env: Vec<(String, String)>,
        clear_env: bool,
    ) -> Result<crate::exec::BgProc, ExecError> {
        let cmd = self.exec.build_command(&ExecRequest {
            program: program.to_string(),
            args: args.to_vec(),
            cwd: self.root.clone(),
            env,
            clear_env,
        })?;
        crate::exec::spawn_background(cmd)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get the permission manager for advanced use cases
    pub fn permission_manager(&self) -> Arc<PermissionManager> {
        self.permission_manager.clone()
    }

    /// Canonicalize a candidate path built under the jail and confirm it still
    /// lives under the (already-canonical) root *after symlink resolution*.
    ///
    /// The textual `starts_with` check alone is not enough: a symlink inside the
    /// workspace (e.g. `escape -> /`) makes a "simple relative" path like
    /// `escape/etc/passwd` textually in-jail while the OS follows it straight
    /// out. To handle not-yet-existing write targets, we canonicalize the
    /// nearest existing ancestor (which resolves any symlinked directory in the
    /// path) and re-append the missing tail components verbatim — a `..`-free
    /// tail appended to an in-jail canonical dir cannot escape.
    fn canonicalize_within_root(&self, candidate: &Path, requested: &str) -> Result<PathBuf, SandboxError> {
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut current: &Path = candidate;
        loop {
            if current.exists() {
                let canonical = current
                    .canonicalize()
                    .map_err(|e| SandboxError::Io(e.to_string()))?;
                if !canonical.starts_with(&self.root) {
                    return Err(SandboxError::Escape(requested.to_string()));
                }
                let mut out = canonical;
                for comp in tail.iter().rev() {
                    out.push(comp);
                }
                return Ok(out);
            }
            match current.file_name() {
                Some(name) => tail.push(name.to_os_string()),
                None => break,
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => break,
            }
        }
        // The jail root always exists, so the loop should have returned above.
        // If we get here nothing along the path existed — fail closed.
        Err(SandboxError::Escape(requested.to_string()))
    }

    /// Resolve a path - now supports absolute paths and paths outside workspace
    /// via the permission system.
    pub async fn resolve(&self, path: &str) -> Result<PathBuf, SandboxError> {
        let path = Path::new(path);

        // If it's a relative path without .. or absolute components, treat as workspace-relative
        let is_simple_relative = path.is_relative() 
            && !path.components().any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)));

        if is_simple_relative {
            // Traditional workspace-relative resolution
            let mut out = self.root.clone();
            for comp in path.components() {
                match comp {
                    Component::Normal(c) => out.push(c),
                    Component::CurDir => {}
                    Component::ParentDir => {
                        if !out.pop() || !out.starts_with(&self.root) {
                            return Err(SandboxError::Escape(path.display().to_string()));
                        }
                    }
                    _ => return Err(SandboxError::Escape(path.display().to_string())),
                }
            }
            // Resolve symlinks and re-check under the canonical root: a textual
            // prefix check alone lets an in-workspace symlink escape the jail.
            self.canonicalize_within_root(&out, &path.display().to_string())
        } else {
            // Absolute path or path with .. - use permission system
            Ok(self.permission_manager
                .request_read(path)
                .await
                .map_err(SandboxError::Permission)?)
        }
    }

    /// Resolve a path for writing (requires write permission)
    pub async fn resolve_for_write(&self, path: &str) -> Result<PathBuf, SandboxError> {
        let path = Path::new(path);

        let is_simple_relative = path.is_relative() 
            && !path.components().any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)));

        if is_simple_relative {
            // Traditional workspace-relative resolution
            let mut out = self.root.clone();
            for comp in path.components() {
                match comp {
                    Component::Normal(c) => out.push(c),
                    Component::CurDir => {}
                    Component::ParentDir => {
                        if !out.pop() || !out.starts_with(&self.root) {
                            return Err(SandboxError::Escape(path.display().to_string()));
                        }
                    }
                    _ => return Err(SandboxError::Escape(path.display().to_string())),
                }
            }
            // Resolve symlinks and re-check under the canonical root (see
            // `canonicalize_within_root`): handles new files under the jail too.
            self.canonicalize_within_root(&out, &path.display().to_string())
        } else {
            // Absolute path or path with .. - use permission system for write access
            Ok(self.permission_manager
                .request_write(path)
                .await
                .map_err(SandboxError::Permission)?)
        }
    }

    /// Read a file - supports paths outside workspace via permission system
    pub async fn read(&self, path: &str) -> Result<String, SandboxError> {
        let resolved = self.resolve(path).await?;
        tokio::task::spawn_blocking(move || {
            std::fs::read_to_string(&resolved).map_err(|e| SandboxError::Io(e.to_string()))
        })
        .await
        .map_err(|e| SandboxError::Io(e.to_string()))?
    }

    /// Write a file - supports paths outside workspace via permission system
    pub async fn write(&self, path: &str, contents: &str) -> Result<Option<String>, SandboxError> {
        let resolved = self.resolve_for_write(path).await?;
        let snapshots = self.snapshots.clone();
        let contents = contents.to_string();
        tokio::task::spawn_blocking(move || {
            let snapshot_id = Self::snapshot_if_exists_at(&snapshots, &resolved)?;
            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent).map_err(|e| SandboxError::Io(e.to_string()))?;
            }
            // Crash-atomic: write a sibling temp file, then rename over the target.
            let tmp = resolved.with_extension(format!("medha-tmp-{}", ulid::Ulid::new()));
            std::fs::write(&tmp, &contents).map_err(|e| SandboxError::Io(e.to_string()))?;
            std::fs::rename(&tmp, &resolved).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                SandboxError::Io(e.to_string())
            })?;
            Ok(snapshot_id)
        })
        .await
        .map_err(|e| SandboxError::Io(e.to_string()))?
    }

    /// List a directory - supports paths outside workspace via permission system
    pub async fn list(&self, path: &str) -> Result<Vec<String>, SandboxError> {
        let resolved = self.resolve(path).await?;
        tokio::task::spawn_blocking(move || {
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&resolved).map_err(|e| SandboxError::Io(e.to_string()))? {
                let entry = entry.map_err(|e| SandboxError::Io(e.to_string()))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let suffix = if entry.path().is_dir() { "/" } else { "" };
                entries.push(format!("{name}{suffix}"));
            }
            entries.sort();
            Ok(entries)
        })
        .await
        .map_err(|e| SandboxError::Io(e.to_string()))?
    }

    fn snapshot_if_exists_at(snapshots: &Path, path: &Path) -> Result<Option<String>, SandboxError> {
        if !path.exists() {
            return Ok(None);
        }
        std::fs::create_dir_all(snapshots).map_err(|e| SandboxError::Io(e.to_string()))?;
        let id = ulid::Ulid::new().to_string();
        let dest = snapshots.join(&id);
        std::fs::copy(path, &dest).map_err(|e| SandboxError::Io(e.to_string()))?;
        Ok(Some(id))
    }

    /// Restore a single file to a snapshot taken before an earlier write —
    /// the primitive behind code rewind (§18.4). `snapshot = Some(id)` copies
    /// that pre-write snapshot back over `path`; `snapshot = None` means the
    /// write being undone had *created* the file, so rewinding removes it. The
    /// target path goes through the same write-jail resolution as a normal write
    /// (so a rewind can never escape the workspace), and the snapshot id is
    /// validated as a bare ULID so it can't reach outside the snapshots dir.
    pub async fn restore(&self, path: &str, snapshot: Option<&str>) -> Result<(), SandboxError> {
        let resolved = self.resolve_for_write(path).await?;
        match snapshot {
            Some(id) => {
                let src = self.snapshot_path(id)?;
                if let Some(parent) = resolved.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| SandboxError::Io(e.to_string()))?;
                }
                std::fs::copy(&src, &resolved).map_err(|e| SandboxError::Io(e.to_string()))?;
            }
            None => {
                // Undo a creation: remove the file if it's still there.
                if resolved.exists() {
                    std::fs::remove_file(&resolved).map_err(|e| SandboxError::Io(e.to_string()))?;
                }
            }
        }
        Ok(())
    }

    /// Snapshot ids are ULIDs (Crockford base32); reject anything else so a
    /// restore can never read a path outside the snapshots directory.
    fn snapshot_path(&self, id: &str) -> Result<PathBuf, SandboxError> {
        if id.len() != 26 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(SandboxError::Escape(format!("invalid snapshot id: {id}")));
        }
        Ok(self.snapshots.join(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::AutoDeny;

    #[tokio::test]
    async fn rejects_escape_jailed() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();
        assert!(sbx.resolve("../etc/passwd").await.is_err());
        assert!(sbx.resolve("/etc/passwd").await.is_err());
        assert!(sbx.resolve("sub/ok.txt").await.is_ok());
    }

    #[tokio::test]
    async fn write_then_read_roundtrips_jailed() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();
        // Write a test file
        sbx.write("test.txt", "hello").await.unwrap();
        // Read it back
        let content = sbx.read("test.txt").await.unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn write_is_atomic_and_leaves_no_temp_files() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();
        sbx.write("a.txt", "v1").await.unwrap();
        sbx.write("a.txt", "v2").await.unwrap();
        assert_eq!(sbx.read("a.txt").await.unwrap(), "v2");
        // No sibling temp files survive a successful write.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("medha-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn restore_rolls_a_file_back_and_deletes_created_files() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();

        // v1 exists, then a second write snapshots v1 and stores v2.
        sbx.write("f.txt", "v1").await.unwrap();
        let snap = sbx.write("f.txt", "v2").await.unwrap().expect("snapshot of v1");
        assert_eq!(sbx.read("f.txt").await.unwrap(), "v2");

        // Restoring the snapshot rolls the file back to v1.
        sbx.restore("f.txt", Some(&snap)).await.unwrap();
        assert_eq!(sbx.read("f.txt").await.unwrap(), "v1");

        // A newly-created file (no prior snapshot) is removed on rewind.
        sbx.write("new.txt", "born").await.unwrap();
        sbx.restore("new.txt", None).await.unwrap();
        assert!(sbx.read("new.txt").await.is_err(), "created file removed");

        // A bogus (non-ULID) snapshot id can't escape the snapshots dir.
        assert!(sbx.restore("f.txt", Some("../../etc/passwd")).await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn allows_workspace_relative_paths() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let gate = Arc::new(AutoDeny);
        let sbx = WorkspaceSandbox::new(&dir, dir.join("medha.lock"), dir.join("medha_audit.log"), Some(gate)).unwrap();
        
        // Write a test file
        sbx.write("test.txt", "hello").await.unwrap();
        // Read it back
        let content = sbx.read("test.txt").await.unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn denies_outside_workspace_without_permission() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let gate = Arc::new(AutoDeny); // AutoDeny always returns false
        let sbx = WorkspaceSandbox::new(&dir, dir.join("medha.lock"), dir.join("medha_audit.log"), Some(gate)).unwrap();
        
        // Try to read /etc/passwd - should be denied
        let result = sbx.read("/etc/passwd").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SandboxError::Permission(_)));
    }

    /// A symlink *inside* the workspace pointing *out* of it must not let a
    /// "simple relative" path (no `..`, not absolute) escape the jail. This is
    /// the path that previously skipped canonicalization AND the permission
    /// manager entirely.
    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_escape_simple_relative() {
        let base = std::env::temp_dir().join(format!("medha-sbx-sym-{}", ulid::Ulid::new()));
        let root = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "top secret").unwrap();

        // workspace/escape -> outside
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&root).unwrap();

        // Reading an existing file through the symlink is refused as an escape.
        let read = sbx.resolve("escape/secret.txt").await;
        assert!(matches!(read, Err(SandboxError::Escape(_))), "symlink read escape not blocked: {read:?}");

        // Writing a *new* file through the symlink is also refused (the symlinked
        // ancestor resolves outside root).
        let write = sbx.resolve_for_write("escape/planted.txt").await;
        assert!(matches!(write, Err(SandboxError::Escape(_))), "symlink write escape not blocked: {write:?}");
    }

    /// Creating a brand-new file under not-yet-existing nested dirs must still
    /// work (the canonicalization guard must not require the target to exist).
    #[tokio::test]
    async fn allows_new_nested_file_within_jail() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-nest-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();

        sbx.write("a/b/c/new.txt", "hi").await.unwrap();
        assert_eq!(sbx.read("a/b/c/new.txt").await.unwrap(), "hi");
    }
}