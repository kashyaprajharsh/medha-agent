//! File access permission system with live ask-then-persist flow.
//!
//! Implements the exact logic from issues.txt:
//! 1. RESOLVE target path fully before any check
//! 2. Allow immediately if inside workspace root
//! 3. Check medha.lock for trusted paths if outside workspace
//! 4. Prompt user via HumanGate if not trusted
//! 5. Persist "always allow" decisions to medha.lock
//! 6. Separate read/write permissions
//! 7. Load all entries into memory on startup
//! 8. Audit log every out-of-workspace access attempt

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Manages file access permissions with live ask-then-persist flow
pub struct PermissionManager {
    workspace_root: PathBuf,
    lock_path: PathBuf,
    audit_path: PathBuf,
    /// In-memory allowlist loaded from medha.lock
    trusted_read_paths: RwLock<HashSet<PathBuf>>,
    trusted_write_paths: RwLock<HashSet<PathBuf>>,
    /// Human gate for prompting user
    human_gate: Option<Arc<dyn HumanGate>>,
    /// Mutex to serialize prompts (one at a time)
    prompt_mutex: Mutex<()>,
}

impl PermissionManager {
    /// Create a new permission manager
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        lock_path: impl Into<PathBuf>,
        audit_path: impl Into<PathBuf>,
    ) -> Result<Self, PermissionError> {
        let workspace_root = workspace_root.into();
        let workspace_root = workspace_root.canonicalize().map_err(|e| {
            PermissionError::Resolution(format!("Failed to canonicalize workspace root: {e}"))
        })?;

        let lock_path = lock_path.into();
        let audit_path = audit_path.into();

        let mut mgr = Self {
            workspace_root,
            lock_path,
            audit_path,
            trusted_read_paths: RwLock::new(HashSet::new()),
            trusted_write_paths: RwLock::new(HashSet::new()),
            human_gate: None,
            prompt_mutex: Mutex::new(()),
        };

        mgr.load_trusted_paths()?;
        Ok(mgr)
    }

    /// Set the human gate for user prompts
    pub fn set_human_gate(&mut self, gate: Arc<dyn HumanGate>) {
        self.human_gate = Some(gate);
    }

    /// Load trusted paths from medha.lock into memory
    fn load_trusted_paths(&mut self) -> Result<(), PermissionError> {
        // Parse medha.lock as a generic TOML value to extract permissions
        if let Ok(content) = std::fs::read_to_string(&self.lock_path)
            && let Ok(value) = toml::from_str::<toml::Value>(&content)
            && let Some(perms) = value.get("permissions")
            && let Some(paths) = perms.get("trusted_paths").and_then(|v| v.as_array())
        {
            let mut read_paths = self.trusted_read_paths.write().unwrap();
            let mut write_paths = self.trusted_write_paths.write().unwrap();

            for trusted in paths {
                if let (Some(path_str), Some(perm_str)) = (
                    trusted.get("path").and_then(|v| v.as_str()),
                    trusted.get("permission").and_then(|v| v.as_str()),
                ) {
                    let path = PathBuf::from(path_str);
                    let path = path.canonicalize().unwrap_or(path);
                    match perm_str {
                        "Read" => {
                            read_paths.insert(path);
                        }
                        "Write" => {
                            write_paths.insert(path);
                        }
                        _ => {}
                    }
                }
            }
        }
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
            path.strip_prefix("~").map(|p| home.join(p)).unwrap_or(path.to_path_buf())
        } else {
            path.to_path_buf()
        };

        // Canonicalize to resolve symlinks and collapse .. - target must exist for read
        path.canonicalize().map_err(|e| {
            PermissionError::Resolution(format!("Failed to canonicalize path {}: {e}", path.display()))
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
            path.strip_prefix("~").map(|p| home.join(p)).unwrap_or(path.to_path_buf())
        } else {
            path.to_path_buf()
        };

        // Split into parent directory and filename
        let parent = path.parent().ok_or_else(|| {
            PermissionError::Resolution(format!("Path has no parent directory: {}", path.display()))
        })?;
        let filename = path.file_name().ok_or_else(|| {
            PermissionError::Resolution(format!("Path has no filename component: {}", path.display()))
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
                    PermissionError::Resolution(format!("Failed to canonicalize parent directory {}: {e}", current.display()))
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
            PermissionError::Resolution(format!("Failed to canonicalize parent directory {}: {e}", path.display()))
        })?;
        Ok((canonical, Vec::new()))
    }

    /// Check if a resolved path is inside the workspace root
    fn is_inside_workspace(&self, resolved_path: &Path) -> bool {
        resolved_path.starts_with(&self.workspace_root)
    }

    /// Check if a path (or its parent) is trusted for the given permission
    fn is_trusted(&self, resolved_path: &Path, perm: PermissionType) -> bool {
        let trusted_paths = match perm {
            PermissionType::Read => self.trusted_read_paths.read().unwrap(),
            PermissionType::Write => self.trusted_write_paths.read().unwrap(),
        };

        // Check exact match or any parent directory
        let mut current = Some(resolved_path);
        while let Some(p) = current {
            if trusted_paths.contains(p) {
                return true;
            }
            current = p.parent();
        }
        false
    }

    /// Add a path to the trusted set (in memory and persisted)
    fn trust_path(&self, resolved_path: PathBuf, perm: PermissionType) -> Result<(), PermissionError> {
        // Add to in-memory set
        match perm {
            PermissionType::Read => {
                self.trusted_read_paths.write().unwrap().insert(resolved_path.clone());
            }
            PermissionType::Write => {
                self.trusted_write_paths.write().unwrap().insert(resolved_path.clone());
            }
        }

        // Persist to medha.lock
        self.persist_trusted_path(&resolved_path, perm)
    }

    /// Persist a trusted path to medha.lock
    fn persist_trusted_path(&self, path: &Path, perm: PermissionType) -> Result<(), PermissionError> {
        // Read existing lock file
        let content = std::fs::read_to_string(&self.lock_path).unwrap_or_default();
        let mut value: toml::Value = if content.trim().is_empty() {
            toml::Value::Table(toml::Table::new())
        } else {
            toml::from_str(&content).map_err(|e| PermissionError::Io(e.to_string()))?
        };

        // Ensure permissions table exists. A hand-edited/corrupt medha.lock
        // (e.g. `permissions = "oops"`) must return an error, not panic and take
        // down the whole agent on the next "Always" approval.
        let permissions = value
            .as_table_mut()
            .ok_or_else(|| PermissionError::Io("medha.lock: top-level is not a TOML table".into()))?
            .entry("permissions")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));

        let permissions_table = permissions.as_table_mut().ok_or_else(|| {
            PermissionError::Io("medha.lock: [permissions] is not a table".into())
        })?;

        // Ensure trusted_paths array exists
        let trusted_paths = permissions_table
            .entry("trusted_paths")
            .or_insert_with(|| toml::Value::Array(vec![]));

        let trusted_paths_array = trusted_paths.as_array_mut().ok_or_else(|| {
            PermissionError::Io("medha.lock: permissions.trusted_paths is not an array".into())
        })?;

        // Add new trusted path
        let perm_str = match perm {
            PermissionType::Read => "Read",
            PermissionType::Write => "Write",
        };

        trusted_paths_array.push(toml::Value::Table({
            let mut table = toml::Table::new();
            table.insert("path".into(), toml::Value::String(path.to_string_lossy().to_string()));
            table.insert("permission".into(), toml::Value::String(perm_str.into()));
            table.insert(
                "granted_at".into(),
                toml::Value::Integer(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                ),
            );
            table
        }));

        // Write back to file
        let new_content = toml::to_string_pretty(&value).map_err(|e| PermissionError::Io(e.to_string()))?;
        std::fs::write(&self.lock_path, new_content).map_err(|e| PermissionError::Io(e.to_string()))
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

        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_path)
            .map_err(|e| PermissionError::Io(e.to_string()))?
            .write_all(log_entry.as_bytes())
            .map_err(|e| PermissionError::Io(e.to_string()))
    }

    /// Request permission for a path (the main entry point)
    pub async fn request_permission(
        &self,
        path: &Path,
        permission: PermissionType,
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

        let human_gate = self.human_gate.as_ref().ok_or(PermissionError::NoHumanGate)?;

        // The surface (TUI/terminal) renders the selectable options; keep the
        // detail to just the explanation so it isn't duplicated.
        let prompt = format!("This path is outside the workspace: {}", resolved.display());

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
                // Allow this operation only; do not persist to medha.lock.
                self.audit_log(path, &resolved, permission, "allowed (user approved, once)")?;
                Ok(resolved)
            }
            kernel::Approval::Always => {
                // Persist the resolved path to medha.lock for this permission type.
                self.trust_path(resolved.clone(), permission)?;
                self.audit_log(path, &resolved, permission, "allowed (user approved, persisted)")?;
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
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Mock gate that always returns a fixed decision.
    struct FixedGate(Approval);
    #[async_trait::async_trait]
    impl HumanGate for FixedGate {
        async fn confirm(&self, _action: &str, _detail: Option<&str>, _escalated: bool) -> Approval {
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

    /// PART 1: "Allow once" must NOT persist to medha.lock, so the next run re-asks.
    #[tokio::test]
    async fn allow_once_does_not_persist() {
        let ws = unique_dir("ws_once");
        let outside = unique_dir("out_once");
        std::fs::write(outside.join("f.txt"), "x").unwrap();
        let lock = ws.join("medha.lock");
        let audit = ws.join("audit.log");
        let target = outside.join("f.txt");

        let mut mgr = PermissionManager::new(&ws, &lock, &audit).unwrap();
        mgr.set_human_gate(Arc::new(FixedGate(Approval::Once)));
        assert!(mgr.request_read(&target).await.is_ok(), "once should allow");
        assert!(
            !lock.exists() || std::fs::read_to_string(&lock).unwrap().trim().is_empty(),
            "allow-once must not write to medha.lock"
        );

        // A fresh manager has no trust for it → a Deny gate now blocks it.
        let mut mgr2 = PermissionManager::new(&ws, &lock, &audit).unwrap();
        mgr2.set_human_gate(Arc::new(FixedGate(Approval::Deny)));
        assert!(mgr2.request_read(&target).await.is_err(), "should re-ask, not silently allow");
    }

    /// PART 1: "Always allow" persists to medha.lock and is trusted on reload.
    #[tokio::test]
    async fn always_allow_persists_and_reloads() {
        let ws = unique_dir("ws_always");
        let outside = unique_dir("out_always");
        std::fs::write(outside.join("f.txt"), "x").unwrap();
        let lock = ws.join("medha.lock");
        let audit = ws.join("audit.log");
        let target = outside.join("f.txt");

        let mut mgr = PermissionManager::new(&ws, &lock, &audit).unwrap();
        mgr.set_human_gate(Arc::new(FixedGate(Approval::Always)));
        assert!(mgr.request_read(&target).await.is_ok());
        assert!(std::fs::read_to_string(&lock).unwrap().contains("trusted_paths"));

        // Fresh manager with NO gate: must already trust the path from the lock file.
        let mgr2 = PermissionManager::new(&ws, &lock, &audit).unwrap();
        assert!(mgr2.request_read(&target).await.is_ok(), "persisted path should be trusted on reload");
    }

    /// PART 1: read trust never grants write trust (permissions tracked separately).
    #[tokio::test]
    async fn read_trust_does_not_grant_write() {
        let ws = unique_dir("ws_sep");
        let outside = unique_dir("out_sep");
        std::fs::write(outside.join("f.txt"), "x").unwrap();
        let lock = ws.join("medha.lock");
        let audit = ws.join("audit.log");
        let target = outside.join("f.txt");

        let mut mgr = PermissionManager::new(&ws, &lock, &audit).unwrap();
        mgr.set_human_gate(Arc::new(FixedGate(Approval::Always)));
        assert!(mgr.request_read(&target).await.is_ok());

        // Reload; write should still be untrusted → a Deny gate blocks it.
        let mut mgr2 = PermissionManager::new(&ws, &lock, &audit).unwrap();
        mgr2.set_human_gate(Arc::new(FixedGate(Approval::Deny)));
        assert!(mgr2.request_write(&target).await.is_err(), "read trust must not grant write");
    }

    /// A write target under not-yet-existing nested subdirectories must resolve
    /// to the FULL path — the intermediate components must not be dropped (which
    /// would silently retarget the write onto an existing ancestor).
    #[test]
    fn write_resolution_preserves_nonexistent_intermediate_dirs() {
        let ws = unique_dir("ws_nested");
        let lock = ws.join("medha.lock");
        let audit = ws.join("audit.log");
        // <ws>/deep/sub/dir/file.txt — none of deep/sub/dir exist yet.
        let target = ws.join("deep").join("sub").join("dir").join("file.txt");

        let mgr = PermissionManager::new(&ws, &lock, &audit).unwrap();
        let resolved = mgr.resolve_path_for_write(&target).unwrap();

        // The canonical <ws> prefix may differ (e.g. /var → /private/var on
        // macOS), so assert on the preserved tail rather than the whole path.
        assert!(
            resolved.ends_with("deep/sub/dir/file.txt"),
            "intermediate dirs were dropped: {resolved:?}"
        );
    }
}
