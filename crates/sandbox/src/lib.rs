//! Execution backends behind one interface (§4.8). Phase 0 ships the
//! `workspace` backend: path-jailed file ops with snapshot-before-write so
//! every mutation is reversible (the basis for `medha undo`). Container/microVM
//! backends are added later behind this same surface (P8).
//!
//! The new permission system allows legitimate access to files outside the
//! workspace via a live ask-then-persist flow (see issues.txt).

use std::collections::HashMap;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use kernel::HumanGate;
use permissions::PermissionManager;
use sha2::{Digest, Sha256};

pub mod exec;
pub use exec::{
    BackendKind, ExecBackend, ExecError, ExecOutput, ExecRequest, HostBackend, NetPolicy,
    SandboxConfig, ShellOutcome, native_backend_available, native_sandbox_supported,
    program_in_dir, program_on_path, run_command_bounded, run_shell_bounded,
    run_shell_bounded_with, select_backend,
};
pub use permissions::ApprovedRoots;

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
    #[error("write target changed after approval: {0}")]
    Conflict(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardedFileState {
    Missing,
    Present {
        sha256: [u8; 32],
        identity: String,
        len: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedInspection {
    pub state: GuardedFileState,
    pub bytes: Option<Vec<u8>>,
}

/// Result of a bounded streaming line-range read.
///
/// `total_lines` is known only when the scan reached EOF. Returning `None`
/// rather than scanning the remainder keeps a small range read independent of
/// the size of a multi-gigabyte file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineRangeRead {
    pub content: String,
    pub start_line: u64,
    pub end_line: u64,
    pub total_lines: Option<u64>,
    pub has_more: bool,
    pub beyond_eof: bool,
    pub bytes_scanned: u64,
}

/// Capability-based write anchor: the authorized directory is held as an open
/// handle and every later component is traversed with `openat`+`O_NOFOLLOW`,
/// so a parent directory swapped for a symlink after authorization cannot
/// redirect the write (AUD-024).
///
/// SCOPE: Unix only. The `#[cfg(not(unix))]` branches below operate by
/// pathname after authorization, so on Windows the parent-symlink-swap race
/// remains open — the same accepted platform gap as the Windows exec sandbox.
/// A port needs `NtCreateFile` relative-open against a held directory handle
/// (or `FILE_FLAG_OPEN_REPARSE_POINT` traversal checks) before writes.
#[cfg(unix)]
struct UnixWriteCapability {
    anchor: std::fs::File,
    pending_dirs: Vec<OsString>,
    leaf: OsString,
}

/// A directory listing bounded by [`WorkspaceSandbox::list_bounded`]. `total`
/// counts every entry seen, so truncation is reportable without retention.
pub struct DirListing {
    pub entries: Vec<String>,
    pub total: usize,
}

type WriteLockTable = Mutex<HashMap<String, Weak<PathLock>>>;

/// One canonical write lane. The table owns only a `Weak`; every waiter/guard
/// owns this wrapper. When the final user disappears—including cancellation
/// while awaiting the mutex—`Drop` removes the matching weak entry.
struct PathLock {
    mutex: Arc<tokio::sync::Mutex<()>>,
    key: String,
    table: std::sync::Weak<WriteLockTable>,
    self_weak: Weak<PathLock>,
}

impl Drop for PathLock {
    fn drop(&mut self) {
        let Some(table) = self.table.upgrade() else {
            return;
        };
        let mut table = table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if table
            .get(&self.key)
            .is_some_and(|entry| Weak::ptr_eq(entry, &self.self_weak))
        {
            table.remove(&self.key);
        }
    }
}

/// An authorised write target with its canonical per-target lock held.
///
/// Callers performing a read-modify-write must retain this value from before
/// the read through the final write. The resolved path is intentionally carried
/// with the guard so execution does not fall back to the model's raw spelling
/// after locking a different alias.
pub struct WritePathGuard {
    resolved: PathBuf,
    #[cfg(unix)]
    capability: UnixWriteCapability,
    _guard: tokio::sync::OwnedMutexGuard<()>,
    _lock: Arc<PathLock>,
}

impl WritePathGuard {
    /// Canonical physical target selected and authorised before this lock was
    /// acquired.
    pub fn resolved(&self) -> &Path {
        &self.resolved
    }
}

/// Turn an authorised, canonical path into its lock-table identity.
///
/// Existing targets have already been canonicalized as a whole, so relative,
/// absolute, `.` and symlink spellings converge here. A prospective target is
/// represented by its canonical existing ancestor plus its normalized missing
/// tail. Windows and the usual macOS filesystems are case-insensitive; folding
/// their spelling also makes two concurrent creates with case-only aliases
/// contend. On case-sensitive volumes this is conservatively over-serializing,
/// never under-serializing.
fn path_lock_key(path: &Path) -> String {
    let canonical = path.to_string_lossy();
    #[cfg(any(windows, target_os = "macos"))]
    {
        canonical.to_lowercase()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        canonical.into_owned()
    }
}

#[cfg(unix)]
fn component_cstring(component: &OsStr) -> Result<std::ffi::CString, SandboxError> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(component.as_bytes())
        .map_err(|_| SandboxError::Escape("path component contains NUL".into()))
}

#[cfg(unix)]
fn file_from_fd(fd: i32) -> std::fs::File {
    use std::os::fd::FromRawFd;
    unsafe { std::fs::File::from_raw_fd(fd) }
}

#[cfg(unix)]
fn open_root_dir() -> Result<std::fs::File, SandboxError> {
    let root = std::ffi::CString::new("/").expect("static path");
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(SandboxError::Io(
            std::io::Error::last_os_error().to_string(),
        ))
    } else {
        Ok(file_from_fd(fd))
    }
}

#[cfg(unix)]
fn open_dir_at(parent: &std::fs::File, name: &OsStr) -> Result<std::fs::File, std::io::Error> {
    use std::os::fd::AsRawFd;
    let name = component_cstring(name)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(file_from_fd(fd))
    }
}

#[cfg(unix)]
fn mkdir_at(parent: &std::fs::File, name: &OsStr) -> Result<(), std::io::Error> {
    use std::os::fd::AsRawFd;
    let name = component_cstring(name)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
fn open_absolute_dir(path: &Path, create: bool) -> Result<Option<std::fs::File>, SandboxError> {
    if !path.is_absolute() {
        return Err(SandboxError::Escape(path.display().to_string()));
    }
    let mut current = open_root_dir()?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        match open_dir_at(&current, name) {
            Ok(next) => current = next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                return Ok(None);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                mkdir_at(&current, name).map_err(|error| SandboxError::Io(error.to_string()))?;
                current = open_dir_at(&current, name)
                    .map_err(|error| SandboxError::Io(error.to_string()))?;
            }
            Err(error) => return Err(SandboxError::Io(error.to_string())),
        }
    }
    Ok(Some(current))
}

#[cfg(unix)]
impl UnixWriteCapability {
    fn open(target: &Path) -> Result<Self, SandboxError> {
        if !target.is_absolute() {
            return Err(SandboxError::Escape(target.display().to_string()));
        }
        let leaf = target
            .file_name()
            .ok_or_else(|| SandboxError::Escape(target.display().to_string()))?
            .to_os_string();
        let parent = target
            .parent()
            .ok_or_else(|| SandboxError::Escape(target.display().to_string()))?;
        let mut anchor = open_root_dir()?;
        let mut pending_dirs = Vec::new();
        let mut missing = false;
        for component in parent.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            if missing {
                pending_dirs.push(name.to_os_string());
                continue;
            }
            match open_dir_at(&anchor, name) {
                Ok(next) => anchor = next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing = true;
                    pending_dirs.push(name.to_os_string());
                }
                Err(error) => return Err(SandboxError::Io(error.to_string())),
            }
        }
        Ok(Self {
            anchor,
            pending_dirs,
            leaf,
        })
    }

    fn open_parent(&self, create: bool) -> Result<Option<std::fs::File>, SandboxError> {
        let mut current = self
            .anchor
            .try_clone()
            .map_err(|error| SandboxError::Io(error.to_string()))?;
        for name in &self.pending_dirs {
            match open_dir_at(&current, name) {
                Ok(next) => current = next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                    return Ok(None);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    mkdir_at(&current, name)
                        .map_err(|error| SandboxError::Io(error.to_string()))?;
                    current = open_dir_at(&current, name)
                        .map_err(|error| SandboxError::Io(error.to_string()))?;
                }
                Err(error) => return Err(SandboxError::Io(error.to_string())),
            }
        }
        Ok(Some(current))
    }

    fn try_clone(&self) -> Result<Self, SandboxError> {
        Ok(Self {
            anchor: self
                .anchor
                .try_clone()
                .map_err(|error| SandboxError::Io(error.to_string()))?,
            pending_dirs: self.pending_dirs.clone(),
            leaf: self.leaf.clone(),
        })
    }
}

#[cfg(unix)]
fn open_regular_file_at(
    parent: &std::fs::File,
    leaf: &OsStr,
) -> Result<Option<std::fs::File>, SandboxError> {
    use std::os::fd::AsRawFd;
    let leaf = component_cstring(leaf)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(SandboxError::Io(error.to_string()));
    }
    let file = file_from_fd(fd);
    let metadata = file
        .metadata()
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    if !metadata.is_file() {
        return Err(SandboxError::Io(
            "write target exists but is not a regular file".into(),
        ));
    }
    Ok(Some(file))
}

fn inspect_open_file(mut file: std::fs::File) -> Result<GuardedInspection, SandboxError> {
    use std::io::Read;
    let metadata = file
        .metadata()
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    let sha256: [u8; 32] = Sha256::digest(&bytes).into();
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    };
    #[cfg(windows)]
    let identity = {
        // `Metadata::volume_serial_number`/`file_index` are still unstable, so
        // ask the OS directly: volume + file index is Windows' equivalent of
        // dev+ino, and it must come from the open handle rather than a path so
        // a swapped target cannot answer for the file we actually inspected.
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            return Err(SandboxError::Io(format!(
                "cannot inspect file identity: {}",
                std::io::Error::last_os_error()
            )));
        }
        format!(
            "{}:{}:{}",
            info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
        )
    };
    #[cfg(not(any(unix, windows)))]
    let identity = format!("{}:{:?}", metadata.len(), metadata.modified().ok());
    Ok(GuardedInspection {
        state: GuardedFileState::Present {
            sha256,
            identity,
            len: metadata.len(),
        },
        bytes: Some(bytes),
    })
}

#[cfg(unix)]
fn inspect_capability(capability: &UnixWriteCapability) -> Result<GuardedInspection, SandboxError> {
    let Some(parent) = capability.open_parent(false)? else {
        return Ok(GuardedInspection {
            state: GuardedFileState::Missing,
            bytes: None,
        });
    };
    match open_regular_file_at(&parent, &capability.leaf)? {
        Some(file) => inspect_open_file(file),
        None => Ok(GuardedInspection {
            state: GuardedFileState::Missing,
            bytes: None,
        }),
    }
}

#[cfg(unix)]
fn create_file_at(
    parent: &std::fs::File,
    leaf: &OsStr,
    mode: libc::mode_t,
) -> Result<std::fs::File, SandboxError> {
    use std::os::fd::AsRawFd;
    let leaf = component_cstring(leaf)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        Err(SandboxError::Io(
            std::io::Error::last_os_error().to_string(),
        ))
    } else {
        Ok(file_from_fd(fd))
    }
}

#[cfg(unix)]
fn unlink_file_at(parent: &std::fs::File, leaf: &OsStr) -> Result<(), SandboxError> {
    use std::os::fd::AsRawFd;
    let leaf = component_cstring(leaf)?;
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(SandboxError::Io(error.to_string()))
        }
    }
}

#[cfg(unix)]
fn rename_file_at(parent: &std::fs::File, from: &OsStr, to: &OsStr) -> Result<(), SandboxError> {
    use std::os::fd::AsRawFd;
    let from = component_cstring(from)?;
    let to = component_cstring(to)?;
    let result = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(SandboxError::Io(
            std::io::Error::last_os_error().to_string(),
        ))
    }
}

/// Publish a newly created file without ever replacing an intervening entry.
///
/// The state is checked immediately before this call, but only an exclusive
/// rename closes the final syscall-sized create race. macOS and Linux expose
/// descriptor-relative no-replace renames; other Unix platforms use an atomic
/// hard-link publication followed by unlinking the private temporary name.
#[cfg(unix)]
fn publish_new_file_at(
    parent: &std::fs::File,
    from: &OsStr,
    to: &OsStr,
) -> Result<(), SandboxError> {
    use std::os::fd::AsRawFd;
    let from = component_cstring(from)?;
    let to_c = component_cstring(to)?;

    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to_c.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    let result = unsafe {
        libc::linkat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to_c.as_ptr(),
            0,
        )
    };

    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(SandboxError::Conflict(to.to_string_lossy().into_owned()));
        }
        return Err(SandboxError::Io(error.to_string()));
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    {
        // The destination link is already durable content if this cleanup
        // fails, so report the failure rather than trying to overwrite it.
        let unlink_result = unsafe { libc::unlinkat(parent.as_raw_fd(), from.as_ptr(), 0) };
        if unlink_result != 0 {
            return Err(SandboxError::Io(
                std::io::Error::last_os_error().to_string(),
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn publish_windows_file(
    temporary: &Path,
    target: &Path,
    target_exists: bool,
) -> Result<(), SandboxError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_WRITE_THROUGH, MoveFileExW, ReplaceFileW,
    };

    // Keep the `Path` arguments intact: the error paths below report the
    // human-readable target, which a shadowing UTF-16 buffer cannot supply.
    let temporary_wide: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // ReplaceFileW atomically merges the replaced file's ACL, attributes,
    // encryption/compression state, object id, and named streams into the
    // replacement. MoveFileExW without REPLACE_EXISTING is the no-clobber
    // publication primitive for a target approved as missing.
    let result = unsafe {
        if target_exists {
            ReplaceFileW(
                target_wide.as_ptr(),
                temporary_wide.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } else {
            MoveFileExW(
                temporary_wide.as_ptr(),
                target_wide.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        }
    };
    if result != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if !target_exists && error.kind() == std::io::ErrorKind::AlreadyExists {
        Err(SandboxError::Conflict(target.display().to_string()))
    } else {
        Err(SandboxError::Io(format!(
            "atomic metadata-preserving replacement failed: {error}"
        )))
    }
}

#[cfg(unix)]
fn set_file_mode(file: &std::fs::File, mode: u32) -> Result<(), SandboxError> {
    use std::os::fd::AsRawFd;
    let result = unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) };
    if result == 0 {
        Ok(())
    } else {
        Err(SandboxError::Io(
            std::io::Error::last_os_error().to_string(),
        ))
    }
}

#[cfg(unix)]
fn copy_file_owner_and_mode(
    source: &std::fs::File,
    destination: &std::fs::File,
) -> Result<(), SandboxError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let source_metadata = source
        .metadata()
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    let destination_metadata = destination
        .metadata()
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    if source_metadata.uid() != destination_metadata.uid()
        || source_metadata.gid() != destination_metadata.gid()
    {
        let result = unsafe {
            libc::fchown(
                destination.as_raw_fd(),
                source_metadata.uid(),
                source_metadata.gid(),
            )
        };
        if result != 0 {
            return Err(SandboxError::Io(format!(
                "cannot preserve file ownership: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    // chown may clear setuid/setgid, so mode is deliberately restored after it.
    set_file_mode(destination, source_metadata.mode() & 0o7777)
}

#[cfg(target_os = "macos")]
fn copy_acl_and_xattrs(
    source: &std::fs::File,
    destination: &std::fs::File,
) -> Result<(), SandboxError> {
    use std::os::fd::AsRawFd;
    let flags = libc::COPYFILE_ACL | libc::COPYFILE_XATTR;
    let result = unsafe {
        libc::fcopyfile(
            source.as_raw_fd(),
            destination.as_raw_fd(),
            std::ptr::null_mut(),
            flags,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(SandboxError::Io(format!(
            "cannot preserve ACLs and extended attributes: {}",
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_xattr_unsupported(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOTSUP) || error.raw_os_error() == Some(libc::EOPNOTSUPP)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_linux_xattrs(
    file: &std::fs::File,
) -> Result<std::collections::BTreeMap<Vec<u8>, Vec<u8>>, SandboxError> {
    use std::os::fd::AsRawFd;

    const MAX_NAME_LIST: usize = 1024 * 1024;
    const MAX_VALUE: usize = 16 * 1024 * 1024;
    const MAX_TOTAL: usize = 64 * 1024 * 1024;
    let fd = file.as_raw_fd();
    let list_len = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
    if list_len < 0 {
        let error = std::io::Error::last_os_error();
        if linux_xattr_unsupported(&error) {
            return Ok(std::collections::BTreeMap::new());
        }
        return Err(SandboxError::Io(format!(
            "cannot enumerate extended attributes: {error}"
        )));
    }
    let list_len = usize::try_from(list_len)
        .map_err(|_| SandboxError::Io("extended-attribute list is too large".into()))?;
    if list_len > MAX_NAME_LIST {
        return Err(SandboxError::Io(format!(
            "extended-attribute name list exceeds {MAX_NAME_LIST} bytes"
        )));
    }
    let mut names = vec![0u8; list_len];
    if list_len != 0 {
        let got = unsafe { libc::flistxattr(fd, names.as_mut_ptr().cast(), names.len()) };
        if got < 0 {
            return Err(SandboxError::Io(format!(
                "cannot read extended-attribute names: {}",
                std::io::Error::last_os_error()
            )));
        }
        names.truncate(usize::try_from(got).unwrap_or(0).min(names.len()));
    }

    let mut attributes = std::collections::BTreeMap::new();
    let mut total = 0usize;
    for raw_name in names.split_inclusive(|byte| *byte == 0) {
        if raw_name.is_empty() {
            continue;
        }
        if raw_name.last() != Some(&0) || raw_name.len() == 1 {
            return Err(SandboxError::Io(
                "filesystem returned a malformed extended-attribute name list".into(),
            ));
        }
        let name = std::ffi::CString::from_vec_with_nul(raw_name.to_vec())
            .map_err(|error| SandboxError::Io(error.to_string()))?;
        let value_len = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0) };
        if value_len < 0 {
            return Err(SandboxError::Io(format!(
                "cannot size extended attribute {:?}: {}",
                name,
                std::io::Error::last_os_error()
            )));
        }
        let value_len = usize::try_from(value_len)
            .map_err(|_| SandboxError::Io("extended-attribute value is too large".into()))?;
        if value_len > MAX_VALUE {
            return Err(SandboxError::Io(format!(
                "extended attribute {:?} exceeds {MAX_VALUE} bytes",
                name
            )));
        }
        total = total
            .checked_add(value_len)
            .ok_or_else(|| SandboxError::Io("extended-attribute aggregate size overflow".into()))?;
        if total > MAX_TOTAL {
            return Err(SandboxError::Io(format!(
                "extended attributes exceed the {MAX_TOTAL}-byte aggregate ceiling"
            )));
        }
        let mut value = vec![0u8; value_len];
        if value_len != 0 {
            let got = unsafe {
                libc::fgetxattr(fd, name.as_ptr(), value.as_mut_ptr().cast(), value.len())
            };
            if got < 0 {
                return Err(SandboxError::Io(format!(
                    "cannot read extended attribute {:?}: {}",
                    name,
                    std::io::Error::last_os_error()
                )));
            }
            value.truncate(usize::try_from(got).unwrap_or(0).min(value.len()));
        }
        attributes.insert(name.to_bytes().to_vec(), value);
    }
    Ok(attributes)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn copy_acl_and_xattrs(
    source: &std::fs::File,
    destination: &std::fs::File,
) -> Result<(), SandboxError> {
    use std::os::fd::AsRawFd;

    let source_attributes = read_linux_xattrs(source)?;
    let destination_attributes = read_linux_xattrs(destination)?;
    for name in destination_attributes
        .keys()
        .filter(|name| !source_attributes.contains_key(*name))
    {
        let name = std::ffi::CString::new(name.as_slice())
            .map_err(|error| SandboxError::Io(error.to_string()))?;
        let result = unsafe { libc::fremovexattr(destination.as_raw_fd(), name.as_ptr()) };
        if result != 0 {
            return Err(SandboxError::Io(format!(
                "cannot remove destination-only extended attribute {:?}: {}",
                name,
                std::io::Error::last_os_error()
            )));
        }
    }
    for (name, value) in source_attributes {
        if destination_attributes.get(&name) == Some(&value) {
            continue;
        }
        let name =
            std::ffi::CString::new(name).map_err(|error| SandboxError::Io(error.to_string()))?;
        let value_ptr = if value.is_empty() {
            std::ptr::null()
        } else {
            value.as_ptr().cast()
        };
        let result = unsafe {
            libc::fsetxattr(
                destination.as_raw_fd(),
                name.as_ptr(),
                value_ptr,
                value.len(),
                0,
            )
        };
        if result != 0 {
            return Err(SandboxError::Io(format!(
                "cannot preserve extended attribute {:?}: {}",
                name,
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "linux", target_os = "android"))
))]
fn copy_acl_and_xattrs(
    _source: &std::fs::File,
    _destination: &std::fs::File,
) -> Result<(), SandboxError> {
    Err(SandboxError::Io(
        "metadata-preserving replacement is unavailable on this Unix platform; refusing to drop ACLs or extended attributes"
            .into(),
    ))
}

#[cfg(target_os = "macos")]
fn copy_file_flags(
    source: &std::fs::File,
    destination: &std::fs::File,
) -> Result<(), SandboxError> {
    use std::os::fd::AsRawFd;
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(source.as_raw_fd(), &mut status) } != 0 {
        return Err(SandboxError::Io(format!(
            "cannot inspect file flags: {}",
            std::io::Error::last_os_error()
        )));
    }
    let flags = status.st_flags;
    let write_blocking =
        libc::UF_IMMUTABLE | libc::UF_APPEND | libc::SF_IMMUTABLE | libc::SF_APPEND;
    if flags & write_blocking != 0 {
        return Err(SandboxError::Io(
            "file has immutable/append-only flags; refusing an atomic replacement that cannot preserve them safely"
                .into(),
        ));
    }
    if unsafe { libc::fchflags(destination.as_raw_fd(), flags) } == 0 {
        Ok(())
    } else {
        Err(SandboxError::Io(format!(
            "cannot preserve file flags: {}",
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn copy_file_flags(
    source: &std::fs::File,
    destination: &std::fs::File,
) -> Result<(), SandboxError> {
    use std::os::fd::AsRawFd;

    const FS_IMMUTABLE_FL: libc::c_long = 0x0000_0010;
    const FS_APPEND_FL: libc::c_long = 0x0000_0020;
    const FS_FL_USER_MODIFIABLE: libc::c_long = 0x0003_80ff;
    let mut flags: libc::c_long = 0;
    if unsafe { libc::ioctl(source.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error().is_some_and(|code| {
            code == libc::ENOTTY || code == libc::EOPNOTSUPP || code == libc::ENOSYS
        }) {
            return Ok(());
        }
        return Err(SandboxError::Io(format!(
            "cannot inspect file flags: {error}"
        )));
    }
    let flags = flags & FS_FL_USER_MODIFIABLE;
    if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
        return Err(SandboxError::Io(
            "file has immutable/append-only flags; refusing an atomic replacement that cannot preserve them safely"
                .into(),
        ));
    }
    let mut flags_to_set = flags;
    if unsafe {
        libc::ioctl(
            destination.as_raw_fd(),
            libc::FS_IOC_SETFLAGS,
            &mut flags_to_set,
        )
    } == 0
    {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // A filesystem can answer GETFLAGS yet refuse SETFLAGS (tmpfs does). With
    // zero flags to carry over the refusal loses nothing; only a real flag set
    // that cannot be copied makes the atomic replacement unsafe.
    let unsupported = error.raw_os_error().is_some_and(|code| {
        code == libc::ENOTTY || code == libc::EOPNOTSUPP || code == libc::ENOSYS
    });
    if unsupported && flags == 0 {
        Ok(())
    } else {
        Err(SandboxError::Io(format!(
            "cannot preserve file flags: {error}"
        )))
    }
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "linux", target_os = "android"))
))]
fn copy_file_flags(
    _source: &std::fs::File,
    _destination: &std::fs::File,
) -> Result<(), SandboxError> {
    Ok(())
}

#[cfg(unix)]
fn copy_required_metadata(
    source: &std::fs::File,
    destination: &std::fs::File,
) -> Result<(), SandboxError> {
    copy_acl_and_xattrs(source, destination)?;
    copy_file_owner_and_mode(source, destination)?;
    copy_file_flags(source, destination)
}

#[cfg(unix)]
fn write_snapshot_capability(
    snapshots: &Path,
    source: &std::fs::File,
    bytes: &[u8],
) -> Result<String, SandboxError> {
    use std::io::Write;
    let directory = open_absolute_dir(snapshots, true)?
        .ok_or_else(|| SandboxError::Io("snapshot directory is unavailable".into()))?;
    let id = ulid::Ulid::new().to_string();
    let leaf = OsString::from(&id);
    let mut file = create_file_at(&directory, &leaf, 0o600)?;
    if let Err(error) = (|| {
        file.write_all(bytes)
            .map_err(|error| SandboxError::Io(error.to_string()))?;
        copy_required_metadata(source, &file)?;
        file.sync_all()
            .map_err(|error| SandboxError::Io(error.to_string()))?;
        directory
            .sync_all()
            .map_err(|error| SandboxError::Io(error.to_string()))
    })() {
        let _ = unlink_file_at(&directory, &leaf);
        return Err(error);
    }
    Ok(id)
}

#[cfg(unix)]
fn remove_snapshot_capability(snapshots: &Path, id: &str) {
    if let Ok(Some(directory)) = open_absolute_dir(snapshots, false) {
        let _ = unlink_file_at(&directory, OsStr::new(id));
    }
}

#[cfg(unix)]
fn write_through_capability(
    capability: &UnixWriteCapability,
    snapshots: &Path,
    contents: &[u8],
    expected: Option<&GuardedFileState>,
) -> Result<Option<String>, SandboxError> {
    use std::io::Write;

    let parent = capability
        .open_parent(true)?
        .ok_or_else(|| SandboxError::Io("write parent is unavailable".into()))?;
    let existing = open_regular_file_at(&parent, &capability.leaf)?;
    let (initial, original_file) = match existing {
        Some(file) => {
            let metadata_source = file
                .try_clone()
                .map_err(|error| SandboxError::Io(error.to_string()))?;
            (inspect_open_file(file)?, Some(metadata_source))
        }
        None => (
            GuardedInspection {
                state: GuardedFileState::Missing,
                bytes: None,
            },
            None,
        ),
    };
    if expected.is_some_and(|expected| expected != &initial.state) {
        return Err(SandboxError::Conflict(
            capability.leaf.to_string_lossy().into_owned(),
        ));
    }

    let snapshot_id = match (&initial.bytes, &original_file) {
        (Some(bytes), Some(source)) => Some(write_snapshot_capability(snapshots, source, bytes)?),
        _ => None,
    };
    let mut temporary = capability.leaf.clone();
    temporary.push(format!(".medha-tmp-{}", ulid::Ulid::new()));
    let mut temp_file = match create_file_at(&parent, &temporary, 0o666) {
        Ok(file) => file,
        Err(error) => {
            if let Some(id) = &snapshot_id {
                remove_snapshot_capability(snapshots, id);
            }
            return Err(error);
        }
    };
    let prepare = (|| {
        temp_file
            .write_all(contents)
            .map_err(|error| SandboxError::Io(error.to_string()))?;
        if let Some(source) = &original_file {
            copy_required_metadata(source, &temp_file)?;
        }
        temp_file
            .sync_all()
            .map_err(|error| SandboxError::Io(error.to_string()))
    })();
    if let Err(error) = prepare {
        let _ = unlink_file_at(&parent, &temporary);
        if let Some(id) = &snapshot_id {
            remove_snapshot_capability(snapshots, id);
        }
        return Err(error);
    }

    // Re-open through the held parent handle immediately before replacement.
    // This detects create/delete/replace/content changes by other processes,
    // including same-content inode replacement.
    let current = match open_regular_file_at(&parent, &capability.leaf)? {
        Some(file) => inspect_open_file(file)?,
        None => GuardedInspection {
            state: GuardedFileState::Missing,
            bytes: None,
        },
    };
    if current.state != initial.state {
        let _ = unlink_file_at(&parent, &temporary);
        if let Some(id) = &snapshot_id {
            remove_snapshot_capability(snapshots, id);
        }
        return Err(SandboxError::Conflict(
            capability.leaf.to_string_lossy().into_owned(),
        ));
    }

    let publish = if initial.state == GuardedFileState::Missing {
        publish_new_file_at(&parent, &temporary, &capability.leaf)
    } else {
        rename_file_at(&parent, &temporary, &capability.leaf)
    };
    if let Err(error) = publish {
        let _ = unlink_file_at(&parent, &temporary);
        if let Some(id) = &snapshot_id {
            remove_snapshot_capability(snapshots, id);
        }
        return Err(error);
    }
    parent
        .sync_all()
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    Ok(snapshot_id)
}

#[cfg(unix)]
fn restore_through_capability(
    capability: &UnixWriteCapability,
    snapshots: &Path,
    snapshot: Option<&str>,
) -> Result<(), SandboxError> {
    use std::io::Write;

    let Some(id) = snapshot else {
        if let Some(parent) = capability.open_parent(false)? {
            unlink_file_at(&parent, &capability.leaf)?;
            parent
                .sync_all()
                .map_err(|error| SandboxError::Io(error.to_string()))?;
        }
        return Ok(());
    };

    let snapshot_dir = open_absolute_dir(snapshots, false)?
        .ok_or_else(|| SandboxError::Io("snapshot directory is unavailable".into()))?;
    let snapshot_file = open_regular_file_at(&snapshot_dir, OsStr::new(id))?
        .ok_or_else(|| SandboxError::Io(format!("snapshot {id} does not exist")))?;
    let metadata_source = snapshot_file
        .try_clone()
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    let inspection = inspect_open_file(snapshot_file)?;
    let bytes = inspection
        .bytes
        .ok_or_else(|| SandboxError::Io(format!("snapshot {id} is empty or unavailable")))?;

    let parent = capability
        .open_parent(true)?
        .ok_or_else(|| SandboxError::Io("restore parent is unavailable".into()))?;
    let mut temporary = capability.leaf.clone();
    temporary.push(format!(".medha-restore-{}", ulid::Ulid::new()));
    let mut temp_file = create_file_at(&parent, &temporary, 0o600)?;
    let prepared = (|| {
        temp_file
            .write_all(&bytes)
            .map_err(|error| SandboxError::Io(error.to_string()))?;
        copy_required_metadata(&metadata_source, &temp_file)?;
        temp_file
            .sync_all()
            .map_err(|error| SandboxError::Io(error.to_string()))
    })();
    if let Err(error) = prepared {
        let _ = unlink_file_at(&parent, &temporary);
        return Err(error);
    }
    if let Err(error) = rename_file_at(&parent, &temporary, &capability.leaf) {
        let _ = unlink_file_at(&parent, &temporary);
        return Err(error);
    }
    parent
        .sync_all()
        .map_err(|error| SandboxError::Io(error.to_string()))
}

fn read_line_range_bounded<R: std::io::Read>(
    reader: R,
    file_len: u64,
    start_line: u64,
    limit: Option<u64>,
    max_input_bytes: u64,
    max_output_bytes: usize,
) -> Result<LineRangeRead, SandboxError> {
    use std::io::BufRead;

    if max_input_bytes == 0 {
        return Err(SandboxError::Io(
            "line-range input byte ceiling must be greater than zero".into(),
        ));
    }
    let limited = reader.take(max_input_bytes);
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, limited);
    let mut content = Vec::with_capacity(max_output_bytes.min(64 * 1024));
    let mut bytes_scanned = 0u64;
    let mut line = 1u64;
    let mut newline_count = 0u64;
    let mut last_was_newline = false;
    let mut saw_input = false;
    let mut returned_lines = 0u64;
    let mut last_selected_line = None;
    let mut stopped_at_limit = limit == Some(0);

    while !stopped_at_limit {
        let (consumed, stop) = {
            let available = reader
                .fill_buf()
                .map_err(|error| SandboxError::Io(error.to_string()))?;
            if available.is_empty() {
                break;
            }
            let mut consumed = 0usize;
            let mut stop = false;
            while consumed < available.len() {
                let rest = &available[consumed..];
                let segment_len = rest
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(rest.len(), |newline| newline + 1);
                let segment = &rest[..segment_len];
                let selected = line >= start_line;
                if selected {
                    let new_len = content.len().checked_add(segment.len()).ok_or_else(|| {
                        SandboxError::Io("line-range output size overflow".into())
                    })?;
                    if new_len > max_output_bytes {
                        return Err(SandboxError::Io(format!(
                            "requested line range exceeds the {max_output_bytes}-byte output ceiling; use a smaller offset/limit or grep"
                        )));
                    }
                    content.extend_from_slice(segment);
                    last_selected_line = Some(line);
                }
                consumed += segment_len;
                bytes_scanned = bytes_scanned.saturating_add(segment_len as u64);
                saw_input = true;
                last_was_newline = segment.last() == Some(&b'\n');
                if last_was_newline {
                    newline_count = newline_count.saturating_add(1);
                    if selected {
                        returned_lines = returned_lines.saturating_add(1);
                    }
                    line = line.saturating_add(1);
                    if limit.is_some_and(|limit| returned_lines >= limit) {
                        stop = true;
                        break;
                    }
                }
            }
            (consumed, stop)
        };
        reader.consume(consumed);
        stopped_at_limit = stop;
    }

    let reached_eof = bytes_scanned >= file_len;
    if !stopped_at_limit && !reached_eof {
        return Err(SandboxError::Io(format!(
            "requested line range requires scanning more than the {max_input_bytes}-byte input ceiling; use grep or a nearer offset"
        )));
    }

    let total_lines = reached_eof.then(|| {
        if !saw_input {
            0
        } else {
            newline_count + u64::from(!last_was_newline)
        }
    });
    let effective_start = total_lines
        .map(|total| start_line.saturating_sub(1).min(total).saturating_add(1))
        .unwrap_or(start_line);
    let end_line = last_selected_line.unwrap_or_else(|| effective_start.saturating_sub(1));
    let beyond_eof =
        total_lines.is_some_and(|total| total > 0 && start_line.saturating_sub(1) >= total);

    let content = String::from_utf8(content)
        .map_err(|error| SandboxError::Io(format!("file is not valid UTF-8: {error}")))?;
    Ok(LineRangeRead {
        content,
        start_line: effective_start,
        end_line,
        total_lines,
        has_more: !reached_eof,
        beyond_eof,
        bytes_scanned,
    })
}

/// A workspace sandbox with permission management for out-of-workspace access.
pub struct WorkspaceSandbox {
    root: PathBuf,
    snapshots: PathBuf,
    permission_manager: Arc<PermissionManager>,
    /// Backend that runs shell/build/VCS commands (host or OS-native jail).
    exec: Arc<dyn ExecBackend>,
    /// Per-target write locks (P0-4): serialize concurrent read-modify-write on
    /// the same physical file so two same-turn edits can't both read the
    /// original and clobber each other (last-write-wins, silent loss, corrupted
    /// snapshot chain). Keys are produced only after resolution/authorization.
    write_locks: Arc<WriteLockTable>,
}

impl WorkspaceSandbox {
    /// Create a new sandbox with permission management.
    ///
    /// `trust_path` must point to a machine-local file outside the workspace;
    /// repository files such as `medha.lock` are deliberately rejected as
    /// trust sources. `audit_path` receives the access audit log.
    pub fn new(
        root: impl Into<PathBuf>,
        trust_path: impl Into<PathBuf>,
        audit_path: impl Into<PathBuf>,
        human_gate: Option<Arc<dyn HumanGate>>,
    ) -> Result<Self, SandboxError> {
        Self::new_with_roots(
            root,
            trust_path,
            audit_path,
            human_gate,
            ApprovedRoots::default(),
        )
    }

    /// Like [`new`](Self::new), but the permission manager publishes grants
    /// into `approved` — the same live handle the exec backend snapshots per
    /// spawned command, so a user approval opens both enforcement paths at
    /// once. Trust-file grants land in it at construction.
    pub fn new_with_roots(
        root: impl Into<PathBuf>,
        trust_path: impl Into<PathBuf>,
        audit_path: impl Into<PathBuf>,
        human_gate: Option<Arc<dyn HumanGate>>,
        approved: ApprovedRoots,
    ) -> Result<Self, SandboxError> {
        let root = root.into();
        let root = root.canonicalize().unwrap_or(root);
        let snapshots = root.join(".medha").join("snapshots");

        let mut permission_manager =
            PermissionManager::new_with_roots(&root, trust_path, audit_path, approved)?;
        if let Some(gate) = human_gate {
            permission_manager.set_human_gate(gate);
        }

        Ok(Self {
            root,
            snapshots,
            permission_manager: Arc::new(permission_manager),
            exec: Arc::new(HostBackend),
            write_locks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Create a new sandbox without permission management (backward compatible).
    /// This maintains the old behavior - hard jail with no out-of-workspace access.
    pub fn new_jailed(root: impl Into<PathBuf>) -> Result<Self, SandboxError> {
        let root = root.into();
        let root = root.canonicalize().unwrap_or(root);
        let snapshots = root.join(".medha").join("snapshots");

        // No trust file and no human gate: repository content can never open
        // an out-of-workspace path in this hard-jail mode.
        let permission_manager =
            PermissionManager::new_jailed(&root, root.join("medha_audit.log"))?;

        Ok(Self {
            root,
            snapshots,
            permission_manager: Arc::new(permission_manager),
            exec: Arc::new(HostBackend),
            write_locks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Resolve and authorise `path`, then acquire the lock for that canonical
    /// physical target. Hold the returned guard across a read-modify-write (as
    /// `fs.edit` / `multi_edit` / `fs.write` do).
    ///
    /// Resolution must precede lock selection: `x`, `./x`, an absolute path and
    /// a symlink can all name one file. For a target that does not exist yet,
    /// [`resolve_for_write`](Self::resolve_for_write) supplies the secured
    /// canonical ancestor plus normalized missing components, which stays the
    /// same key after the first contender creates it.
    pub async fn path_guard(&self, path: &str) -> Result<WritePathGuard, SandboxError> {
        let resolved = self.resolve_for_write(path).await?;
        let lock = self.path_lock(&resolved);
        let guard = lock.mutex.clone().lock_owned().await;
        #[cfg(unix)]
        let capability = {
            let target = resolved.clone();
            match tokio::task::spawn_blocking(move || UnixWriteCapability::open(&target)).await {
                Ok(Ok(capability)) => capability,
                Ok(Err(error)) => return Err(error),
                Err(error) => return Err(SandboxError::Io(error.to_string())),
            }
        };
        Ok(WritePathGuard {
            resolved,
            #[cfg(unix)]
            capability,
            _guard: guard,
            _lock: lock,
        })
    }

    fn path_lock(&self, resolved: &Path) -> Arc<PathLock> {
        let key = path_lock_key(resolved);
        let mut locks = self
            .write_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match locks.get(&key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let table = Arc::downgrade(&self.write_locks);
                let lock = Arc::new_cyclic(|self_weak| PathLock {
                    mutex: Arc::new(tokio::sync::Mutex::new(())),
                    key: key.clone(),
                    table,
                    self_weak: self_weak.clone(),
                });
                locks.insert(key.clone(), Arc::downgrade(&lock));
                lock
            }
        }
    }

    /// Install the execution backend used by shell/build/VCS tools. Defaults to
    /// [`HostBackend`]; the CLI swaps in the OS-native jail per `medha.lock`.
    pub fn with_exec_backend(mut self, backend: Arc<dyn ExecBackend>) -> Self {
        self.exec = backend;
        self
    }

    /// Grant prompt-free READ access to harness-owned directories outside the
    /// workspace — e.g. the user skills root, whose bundled reference files
    /// the model reads on demand (a dialog per file would break skills).
    /// In-memory only; writes stay gated.
    pub fn with_readable_roots(self, roots: &[PathBuf]) -> Self {
        for root in roots {
            self.permission_manager.allow_read_dir(root);
        }
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
    ///
    /// A denial inside the OS jail on a path the user never ruled on escalates
    /// to the same approval card the file tools show, then retries once — so a
    /// "yes" actually unblocks the command instead of surfacing as a tool
    /// error. "Always" stays live for the session and beyond; "Once" is
    /// granted for the single retry and withdrawn.
    pub async fn exec(
        &self,
        program: &str,
        args: &[String],
        env: Vec<(String, String)>,
        clear_env: bool,
    ) -> Result<ExecOutput, ExecError> {
        let req = ExecRequest {
            program: program.to_string(),
            args: args.to_vec(),
            cwd: self.root.clone(),
            env,
            clear_env,
        };
        let mut output = self.exec.run(req.clone()).await?;
        if self.exec.label() != "native" {
            // Host/container/ssh denials are real permission errors, not jail
            // policy — an approval card could not change them.
            return Ok(output);
        }
        let approved = self.permission_manager.approved_roots();
        let mut prompted: Vec<PathBuf> = Vec::new();
        while output.status != Some(0) && prompted.len() < 3 {
            let candidate = exec::escalation_candidates(&output, &req.args, &self.root, &approved)
                .into_iter()
                .find(|candidate| !prompted.contains(candidate));
            let Some(candidate) = candidate else { break };
            prompted.push(candidate.clone());
            let shown = match req.args.as_slice() {
                [flag, command] if flag == "-c" => command.clone(),
                _ => format!("{} {}", req.program, req.args.join(" ")),
            };
            let detail = format!(
                "The OS sandbox blocked this command on a path outside the workspace:\n  {shown}"
            );
            let Ok(resolved) = self
                .permission_manager
                .request_permission_with_detail(
                    &candidate,
                    permissions::PermissionType::Read,
                    Some(&detail),
                )
                .await
            else {
                break;
            };
            let once = !approved.is_allowed(&resolved, permissions::PermissionType::Read);
            if once {
                approved.allow_read(resolved.clone());
            }
            let retried = self.exec.run(req.clone()).await;
            if once {
                approved.remove_read(&resolved);
            }
            output = retried?;
        }
        Ok(output)
    }

    /// Spawn an owned command task through the active backend (same jail as
    /// [`exec`]). Returns immediately with a [`BgProc`] handle whose output
    /// streams into a rolling buffer. Callers must install cancellation cleanup
    /// before awaiting.
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

    /// Label of the active execution backend, so callers can pick the shell that
    /// backend actually provides — see [`crate::exec::shell_argv`].
    pub fn backend_label(&self) -> &str {
        self.exec.label()
    }

    /// Spawn a shell *command line* as an owned task, choosing the interpreter
    /// the active backend provides. Prefer this over passing `sh` to
    /// [`exec_background`]: Windows has no `sh`, and hardcoding one made every
    /// shell command fail before it ran.
    pub fn shell_background(
        &self,
        command: &str,
        env: Vec<(String, String)>,
        clear_env: bool,
    ) -> Result<crate::exec::BgProc, ExecError> {
        let (program, args) = crate::exec::shell_argv(self.backend_label(), command);
        self.exec_background(&program, &args, env, clear_env)
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
    fn canonicalize_within_root(
        &self,
        candidate: &Path,
        requested: &str,
    ) -> Result<PathBuf, SandboxError> {
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
            && !path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            });

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
            Ok(self
                .permission_manager
                .request_read(path)
                .await
                .map_err(SandboxError::Permission)?)
        }
    }

    /// Resolve a path for writing (requires write permission)
    pub async fn resolve_for_write(&self, path: &str) -> Result<PathBuf, SandboxError> {
        let path = Path::new(path);

        let is_simple_relative = path.is_relative()
            && !path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            });

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
            Ok(self
                .permission_manager
                .request_write(path)
                .await
                .map_err(SandboxError::Permission)?)
        }
    }

    /// Read a file - supports paths outside workspace via permission system
    pub async fn read(&self, path: &str) -> Result<String, SandboxError> {
        let resolved = self.resolve(path).await?;
        self.read_resolved(&resolved).await
    }

    /// Resolve for reading only if the path is *already* permitted — never
    /// prompts, returning `None` where [`resolve`](Self::resolve) would ask.
    ///
    /// For previews, which run *before* the approval card and so must not put a
    /// permission dialog in front of a user who has not yet been told what the
    /// operation is — and would then be asked again when it runs.
    pub async fn resolve_if_permitted(&self, path: &str) -> Option<PathBuf> {
        let p = Path::new(path);
        let simple_relative = p.is_relative()
            && !p.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            });
        if simple_relative {
            // Stays inside the jail, so this never reaches the human gate.
            return self.resolve(path).await.ok();
        }
        self.permission_manager
            .resolve_if_permitted(p, permissions::PermissionType::Read)
    }

    /// Read a file only if it is already permitted, never prompting.
    pub async fn read_if_permitted(&self, path: &str) -> Option<String> {
        let resolved = self.resolve_if_permitted(path).await?;
        self.read_resolved(&resolved).await.ok()
    }

    /// Read a path [`resolve`](Self::resolve) has already authorised.
    ///
    /// Resolving is what asks the user, so a caller that needs the path for
    /// more than one step — a directory check, a size guard, then the read —
    /// must resolve once and reuse it. Calling `resolve` per step prompts per
    /// step, which is one approval dialog per step for the same file.
    pub async fn read_resolved(&self, resolved: &Path) -> Result<String, SandboxError> {
        let resolved = resolved.to_path_buf();
        tokio::task::spawn_blocking(move || {
            std::fs::read_to_string(&resolved).map_err(|e| SandboxError::Io(e.to_string()))
        })
        .await
        .map_err(|e| SandboxError::Io(e.to_string()))?
    }

    /// Read a 1-based line range without materializing or splitting the whole
    /// file. Both I/O scanned before the requested range and retained output
    /// have hard byte ceilings.
    pub async fn read_line_range_resolved(
        &self,
        resolved: &Path,
        start_line: u64,
        limit: Option<u64>,
        max_input_bytes: u64,
        max_output_bytes: usize,
    ) -> Result<LineRangeRead, SandboxError> {
        let resolved = resolved.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&resolved)
                .map_err(|error| SandboxError::Io(error.to_string()))?;
            let file_len = file
                .metadata()
                .map_err(|error| SandboxError::Io(error.to_string()))?
                .len();
            read_line_range_bounded(
                file,
                file_len,
                start_line.max(1),
                limit,
                max_input_bytes,
                max_output_bytes,
            )
        })
        .await
        .map_err(|error| SandboxError::Io(error.to_string()))?
    }

    /// Inspect the exact file selected by a held write capability. The returned
    /// state combines a SHA-256 content digest and file identity, so an approval
    /// pin detects create/delete/content changes and same-content replacement.
    pub async fn inspect_guarded(
        &self,
        guard: &WritePathGuard,
    ) -> Result<GuardedInspection, SandboxError> {
        #[cfg(unix)]
        {
            let capability = guard.capability.try_clone()?;
            tokio::task::spawn_blocking(move || inspect_capability(&capability))
                .await
                .map_err(|error| SandboxError::Io(error.to_string()))?
        }
        #[cfg(not(unix))]
        {
            let path = guard.resolved.clone();
            tokio::task::spawn_blocking(move || match std::fs::File::open(&path) {
                Ok(file) => inspect_open_file(file),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(GuardedInspection {
                        state: GuardedFileState::Missing,
                        bytes: None,
                    })
                }
                Err(error) => Err(SandboxError::Io(error.to_string())),
            })
            .await
            .map_err(|error| SandboxError::Io(error.to_string()))?
        }
    }

    /// Inspect a preview target only when it is already readable, without
    /// opening a permission prompt ahead of the approval card.
    pub async fn inspect_if_permitted(
        &self,
        path: &str,
    ) -> Result<Option<GuardedInspection>, SandboxError> {
        let Some(resolved) = self.resolve_if_permitted(path).await else {
            return Ok(None);
        };
        #[cfg(unix)]
        {
            let capability = UnixWriteCapability::open(&resolved)?;
            let inspection = tokio::task::spawn_blocking(move || inspect_capability(&capability))
                .await
                .map_err(|error| SandboxError::Io(error.to_string()))??;
            Ok(Some(inspection))
        }
        #[cfg(not(unix))]
        {
            tokio::task::spawn_blocking(move || match std::fs::File::open(&resolved) {
                Ok(file) => inspect_open_file(file).map(Some),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(Some(GuardedInspection {
                        state: GuardedFileState::Missing,
                        bytes: None,
                    }))
                }
                Err(error) => Err(SandboxError::Io(error.to_string())),
            })
            .await
            .map_err(|error| SandboxError::Io(error.to_string()))?
        }
    }

    /// Write a file - supports paths outside workspace via permission system
    pub async fn write(&self, path: &str, contents: &str) -> Result<Option<String>, SandboxError> {
        let guard = self.path_guard(path).await?;
        self.write_guarded(&guard, contents).await
    }

    /// Write through a previously authorised canonical target while its
    /// per-target lock is held. Read-modify-write tools use this instead of
    /// resolving the raw path again after acquiring [`WritePathGuard`].
    pub async fn write_guarded(
        &self,
        guard: &WritePathGuard,
        contents: &str,
    ) -> Result<Option<String>, SandboxError> {
        self.write_guarded_checked(guard, contents, None).await
    }

    /// Write through a held capability, optionally requiring the target to
    /// match an approval/read revision. The comparison and replacement happen
    /// under the same parent directory handle.
    pub async fn write_guarded_checked(
        &self,
        guard: &WritePathGuard,
        contents: &str,
        expected: Option<&GuardedFileState>,
    ) -> Result<Option<String>, SandboxError> {
        #[cfg(unix)]
        {
            let capability = guard.capability.try_clone()?;
            let snapshots = self.snapshots.clone();
            let contents = contents.as_bytes().to_vec();
            let expected = expected.cloned();
            tokio::task::spawn_blocking(move || {
                write_through_capability(&capability, &snapshots, &contents, expected.as_ref())
            })
            .await
            .map_err(|error| SandboxError::Io(error.to_string()))?
        }
        #[cfg(not(unix))]
        {
            use std::io::Write;

            let resolved = guard.resolved.clone();
            let snapshots = self.snapshots.clone();
            let contents = contents.as_bytes().to_vec();
            let expected = expected.cloned();
            tokio::task::spawn_blocking(move || {
                let current = match std::fs::File::open(&resolved) {
                    Ok(file) => inspect_open_file(file)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        GuardedInspection {
                            state: GuardedFileState::Missing,
                            bytes: None,
                        }
                    }
                    Err(error) => return Err(SandboxError::Io(error.to_string())),
                };
                if expected.is_some_and(|expected| expected != current.state) {
                    return Err(SandboxError::Conflict(resolved.display().to_string()));
                }
                let target_existed = current.state != GuardedFileState::Missing;
                #[cfg(not(windows))]
                if target_existed {
                    return Err(SandboxError::Io(
                        "metadata-preserving atomic replacement is unavailable on this platform; refusing to replace an existing file"
                            .into(),
                    ));
                }
                let snapshot_id = Self::snapshot_if_exists_at(&snapshots, &resolved)?;
                if let Some(parent) = resolved.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| SandboxError::Io(e.to_string()))?;
                }
                let tmp = resolved.with_extension(format!("medha-tmp-{}", ulid::Ulid::new()));
                let mut temp_file = std::fs::File::create(&tmp)
                    .map_err(|error| SandboxError::Io(error.to_string()))?;
                let prepared = temp_file
                    .write_all(&contents)
                    .and_then(|_| temp_file.sync_all())
                    .map_err(|error| SandboxError::Io(error.to_string()));
                if let Err(error) = prepared {
                    drop(temp_file);
                    let _ = std::fs::remove_file(&tmp);
                    if let Some(id) = &snapshot_id {
                        let _ = std::fs::remove_file(snapshots.join(id));
                    }
                    return Err(error);
                }
                // Windows cannot move/replace an open source file. Unix
                // permits renaming an open inode, which hid this lifetime bug
                // from the other hosted runners.
                drop(temp_file);

                // Revalidate immediately before the atomic OS publication.
                let latest = match std::fs::File::open(&resolved) {
                    Ok(file) => inspect_open_file(file)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        GuardedInspection {
                            state: GuardedFileState::Missing,
                            bytes: None,
                        }
                    }
                    Err(error) => return Err(SandboxError::Io(error.to_string())),
                };
                if latest.state != current.state {
                    let _ = std::fs::remove_file(&tmp);
                    if let Some(id) = &snapshot_id {
                        let _ = std::fs::remove_file(snapshots.join(id));
                    }
                    return Err(SandboxError::Conflict(resolved.display().to_string()));
                }

                #[cfg(windows)]
                let publish = publish_windows_file(&tmp, &resolved, target_existed);
                #[cfg(not(windows))]
                let publish = std::fs::rename(&tmp, &resolved)
                    .map_err(|error| SandboxError::Io(error.to_string()));
                if let Err(error) = publish {
                    let _ = std::fs::remove_file(&tmp);
                    if let Some(id) = &snapshot_id {
                        let _ = std::fs::remove_file(snapshots.join(id));
                    }
                    return Err(error);
                }
                Ok(snapshot_id)
            })
            .await
            .map_err(|e| SandboxError::Io(e.to_string()))?
        }
    }

    /// List a directory - supports paths outside workspace via permission system
    pub async fn list(&self, path: &str) -> Result<Vec<String>, SandboxError> {
        Ok(self.list_bounded(path, usize::MAX).await?.entries)
    }

    /// Like [`list`](Self::list), retaining at most `max` entries while still
    /// counting the rest — a million-entry directory costs a bounded
    /// allocation, and the caller can report exactly how much was left out.
    pub async fn list_bounded(&self, path: &str, max: usize) -> Result<DirListing, SandboxError> {
        let resolved = self.resolve(path).await?;
        tokio::task::spawn_blocking(move || {
            let mut entries = Vec::new();
            let mut total: usize = 0;
            for entry in
                std::fs::read_dir(&resolved).map_err(|e| SandboxError::Io(e.to_string()))?
            {
                let entry = entry.map_err(|e| SandboxError::Io(e.to_string()))?;
                total += 1;
                if entries.len() < max {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let suffix = if entry.path().is_dir() { "/" } else { "" };
                    entries.push(format!("{name}{suffix}"));
                }
            }
            entries.sort();
            Ok(DirListing { entries, total })
        })
        .await
        .map_err(|e| SandboxError::Io(e.to_string()))?
    }

    #[cfg(not(unix))]
    fn snapshot_if_exists_at(
        snapshots: &Path,
        path: &Path,
    ) -> Result<Option<String>, SandboxError> {
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
        let guard = self.path_guard(path).await?;
        self.restore_guarded(&guard, snapshot).await
    }

    /// Restore through a previously authorised canonical target while its
    /// per-target lock is held.
    async fn restore_guarded(
        &self,
        guard: &WritePathGuard,
        snapshot: Option<&str>,
    ) -> Result<(), SandboxError> {
        if let Some(id) = snapshot {
            self.snapshot_path(id)?;
        }
        #[cfg(unix)]
        {
            let capability = guard.capability.try_clone()?;
            let snapshots = self.snapshots.clone();
            let snapshot = snapshot.map(str::to_owned);
            tokio::task::spawn_blocking(move || {
                restore_through_capability(&capability, &snapshots, snapshot.as_deref())
            })
            .await
            .map_err(|error| SandboxError::Io(error.to_string()))?
        }
        #[cfg(not(unix))]
        {
            let resolved = guard.resolved();
            match snapshot {
                Some(id) => {
                    let src = self.snapshot_path(id)?;
                    if let Some(parent) = resolved.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| SandboxError::Io(e.to_string()))?;
                    }
                    #[cfg(not(windows))]
                    if resolved.exists() {
                        return Err(SandboxError::Io(
                            "metadata-preserving atomic restore is unavailable on this platform; refusing to replace an existing file"
                                .into(),
                        ));
                    }
                    let temporary =
                        resolved.with_extension(format!("medha-restore-{}", ulid::Ulid::new()));
                    std::fs::copy(&src, &temporary)
                        .map_err(|error| SandboxError::Io(error.to_string()))?;
                    let temp_file = std::fs::File::open(&temporary)
                        .map_err(|error| SandboxError::Io(error.to_string()))?;
                    if let Err(error) = temp_file.sync_all() {
                        drop(temp_file);
                        let _ = std::fs::remove_file(&temporary);
                        return Err(SandboxError::Io(error.to_string()));
                    }
                    // `ReplaceFileW`/`MoveFileExW` reject an open source
                    // handle with ERROR_SHARING_VIOLATION. Close the staged
                    // snapshot before publishing it over the target.
                    drop(temp_file);
                    #[cfg(windows)]
                    let publish = publish_windows_file(&temporary, resolved, resolved.exists());
                    #[cfg(not(windows))]
                    let publish = std::fs::rename(&temporary, resolved)
                        .map_err(|error| SandboxError::Io(error.to_string()));
                    if let Err(error) = publish {
                        let _ = std::fs::remove_file(&temporary);
                        return Err(error);
                    }
                }
                None => {
                    // Undo a creation: remove the file if it's still there.
                    if resolved.exists() {
                        std::fs::remove_file(resolved)
                            .map_err(|e| SandboxError::Io(e.to_string()))?;
                    }
                }
            }
            Ok(())
        }
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

#[async_trait::async_trait]
impl kernel::ProgressiveContextPathAuthorizer for WorkspaceSandbox {
    async fn authorize_context_path(&self, path: &Path) -> Option<kernel::AuthorizedContextPath> {
        // Discovery is secondary to an already-completed tool operation. It
        // must never open a new permission prompt: only workspace paths and
        // machine-local roots the user previously approved are eligible.
        let requested = path.to_string_lossy();
        let resolved = self.resolve_if_permitted(&requested).await?;
        let trust = if resolved.starts_with(&self.root) {
            kernel::TrustLabel::Workspace
        } else {
            // An approved external path is local tool input, not repository
            // context. Never upgrade it to Workspace merely because discovery
            // found an AGENTS/MEDHA/CLAUDE file beside the touched target.
            kernel::TrustLabel::Tool
        };
        Some(kernel::AuthorizedContextPath {
            path: resolved,
            trust,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::AutoDeny;

    /// Gate that answers every prompt with one fixed decision and counts asks.
    /// Only the macOS escalation tests construct it; a Linux CI build with
    /// `-D warnings` sees it as dead code without the matching gate.
    #[cfg(target_os = "macos")]
    struct CountingGate(kernel::Approval, std::sync::atomic::AtomicUsize);

    #[cfg(target_os = "macos")]
    impl CountingGate {
        fn new(approval: kernel::Approval) -> Arc<Self> {
            Arc::new(Self(approval, std::sync::atomic::AtomicUsize::new(0)))
        }
        fn asked(&self) -> usize {
            self.1.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[cfg(target_os = "macos")]
    #[async_trait::async_trait]
    impl HumanGate for CountingGate {
        async fn confirm(
            &self,
            _action: &str,
            _detail: Option<&str>,
            _escalated: bool,
        ) -> kernel::Approval {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.0
        }
    }

    #[cfg(target_os = "macos")]
    fn escalation_fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf, ApprovedRoots) {
        let base =
            std::env::temp_dir().join(format!("medha-exec-escal-{tag}-{}", ulid::Ulid::new()));
        let ws = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("notes.md"), "outside-content").unwrap();
        (base, ws, outside, ApprovedRoots::default())
    }

    #[cfg(target_os = "macos")]
    fn escalation_sandbox(
        base: &Path,
        ws: &Path,
        approved: &ApprovedRoots,
        gate: Arc<dyn HumanGate>,
    ) -> WorkspaceSandbox {
        let backend = exec::select_backend(
            &SandboxConfig {
                backend: BackendKind::Native,
                net: NetPolicy::Allow,
                ..Default::default()
            },
            vec![],
            approved.clone(),
        );
        WorkspaceSandbox::new_with_roots(
            ws,
            base.join("state").join("trust.lock"),
            base.join("state").join("audit.log"),
            Some(gate),
            approved.clone(),
        )
        .unwrap()
        .with_exec_backend(backend)
    }

    /// The full escalation loop: a jailed command hits an unapproved root, the
    /// approval card is raised, "Always" opens the root, and the automatic
    /// retry succeeds — one prompt, no restart.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn exec_escalates_sandbox_denials_and_retries_after_always() {
        if !exec::native_sandbox_supported() {
            return;
        }
        let (base, ws, outside, approved) = escalation_fixture("always");
        let gate = CountingGate::new(kernel::Approval::Always);
        let sbx = escalation_sandbox(&base, &ws, &approved, gate.clone());
        assert_eq!(sbx.backend_label(), "native");

        let cmd = format!("cat {}/notes.md", outside.display());
        let out = sbx
            .exec("/bin/sh", &["-c".into(), cmd], vec![], true)
            .await
            .unwrap();
        assert_eq!(
            out.status,
            Some(0),
            "approval must unblock the retried command; stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("outside-content"));
        assert_eq!(gate.asked(), 1, "exactly one card for one root");
        assert!(
            approved.is_allowed(
                &outside.canonicalize().unwrap(),
                permissions::PermissionType::Read
            ),
            "Always must stay live for the session"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// "Once" unblocks the single retry, then the grant is withdrawn: the
    /// next identical command must ask again.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn exec_escalation_once_is_withdrawn_after_the_retry() {
        if !exec::native_sandbox_supported() {
            return;
        }
        let (base, ws, outside, approved) = escalation_fixture("once");
        let gate = CountingGate::new(kernel::Approval::Once);
        let sbx = escalation_sandbox(&base, &ws, &approved, gate.clone());

        let cmd = format!("cat {}/notes.md", outside.display());
        let out = sbx
            .exec("/bin/sh", &["-c".into(), cmd.clone()], vec![], true)
            .await
            .unwrap();
        assert_eq!(
            out.status,
            Some(0),
            "Once must unblock the retry; asked={} stderr={}",
            gate.asked(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !approved.is_allowed(
                &outside.canonicalize().unwrap(),
                permissions::PermissionType::Read
            ),
            "Once must not outlive the retry"
        );

        let again = sbx
            .exec("/bin/sh", &["-c".into(), cmd], vec![], true)
            .await
            .unwrap();
        assert_eq!(again.status, Some(0));
        assert_eq!(
            gate.asked(),
            2,
            "a Once approval must be re-asked next time"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// A denial answered with "No" stays a denial: one card, no retry storm,
    /// and the command's failure is reported faithfully.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn exec_escalation_denial_fails_closed_with_one_prompt() {
        if !exec::native_sandbox_supported() {
            return;
        }
        let (base, ws, outside, approved) = escalation_fixture("deny");
        let gate = CountingGate::new(kernel::Approval::Deny);
        let sbx = escalation_sandbox(&base, &ws, &approved, gate.clone());

        let cmd = format!("cat {}/notes.md", outside.display());
        let out = sbx
            .exec("/bin/sh", &["-c".into(), cmd], vec![], true)
            .await
            .unwrap();
        assert_ne!(out.status, Some(0), "denied means denied");
        assert_eq!(gate.asked(), 1, "no repeat prompting after a No");
        assert!(
            approved.read_roots().is_empty(),
            "a denial must not leave a grant behind"
        );
        std::fs::remove_dir_all(&base).ok();
    }

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
    async fn hard_jail_ignores_repository_permission_grants() {
        let base = std::env::temp_dir().join(format!("medha-sbx-repo-{}", ulid::Ulid::new()));
        let ws = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "secret").unwrap();
        std::fs::write(
            ws.join("medha.lock"),
            "[[permissions.trusted_paths]]\n\
             path = \"/\"\n\
             permission = \"Read\"\n\
             granted_at = 123\n",
        )
        .unwrap();

        let sbx = WorkspaceSandbox::new_jailed(&ws).unwrap();
        assert!(
            sbx.read(secret.to_str().unwrap()).await.is_err(),
            "portable permissions must not weaken a hard jail"
        );
        std::fs::remove_dir_all(&base).ok();
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
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn restore_rolls_a_file_back_and_deletes_created_files() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();

        // v1 exists, then a second write snapshots v1 and stores v2.
        sbx.write("f.txt", "v1").await.unwrap();
        let snap = sbx
            .write("f.txt", "v2")
            .await
            .unwrap()
            .expect("snapshot of v1");
        assert_eq!(sbx.read("f.txt").await.unwrap(), "v2");

        // Restoring the snapshot rolls the file back to v1.
        sbx.restore("f.txt", Some(&snap)).await.unwrap();
        assert_eq!(sbx.read("f.txt").await.unwrap(), "v1");

        // A newly-created file (no prior snapshot) is removed on rewind.
        sbx.write("new.txt", "born").await.unwrap();
        sbx.restore("new.txt", None).await.unwrap();
        assert!(sbx.read("new.txt").await.is_err(), "created file removed");

        // A bogus (non-ULID) snapshot id can't escape the snapshots dir.
        assert!(
            sbx.restore("f.txt", Some("../../etc/passwd"))
                .await
                .is_err()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn allows_workspace_relative_paths() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let gate = Arc::new(AutoDeny);
        let trust =
            std::env::temp_dir().join(format!("medha-sbx-trust-{}.lock", ulid::Ulid::new()));
        let sbx =
            WorkspaceSandbox::new(&dir, trust, dir.join("medha_audit.log"), Some(gate)).unwrap();

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
        let trust =
            std::env::temp_dir().join(format!("medha-sbx-trust-{}.lock", ulid::Ulid::new()));
        let sbx =
            WorkspaceSandbox::new(&dir, trust, dir.join("medha_audit.log"), Some(gate)).unwrap();

        // Try to read /etc/passwd - should be denied
        let result = sbx.read("/etc/passwd").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SandboxError::Permission(_)));
    }

    /// A readable root (e.g. the user skills dir) reads without any gate —
    /// bundled skill files must not raise a permission card per read. Writes
    /// under the same root remain gated.
    #[tokio::test]
    async fn readable_roots_read_without_prompt_but_writes_stay_gated() {
        let base = std::env::temp_dir().join(format!("medha-sbx-skills-{}", ulid::Ulid::new()));
        let ws = base.join("ws");
        let skills = base.join("home-skills");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(skills.join("pdf")).unwrap();
        std::fs::write(skills.join("pdf").join("reference.md"), "details").unwrap();

        let gate = Arc::new(AutoDeny); // would deny any prompt — proves no prompt happens
        let sbx = WorkspaceSandbox::new(
            &ws,
            base.join("trust.lock"),
            ws.join("medha_audit.log"),
            Some(gate),
        )
        .unwrap()
        .with_readable_roots(std::slice::from_ref(&skills));

        let text = sbx
            .read(skills.join("pdf").join("reference.md").to_str().unwrap())
            .await
            .expect("bundled skill file reads prompt-free");
        assert_eq!(text, "details");
        let authorized = kernel::ProgressiveContextPathAuthorizer::authorize_context_path(
            &sbx,
            &skills.join("pdf").join("reference.md"),
        )
        .await
        .expect("the progressive loader sees the same approved read root");
        assert_eq!(authorized.trust, kernel::TrustLabel::Tool);
        assert_eq!(
            authorized.path,
            skills
                .join("pdf")
                .join("reference.md")
                .canonicalize()
                .unwrap()
        );

        // Writing into the readable root still requires permission (denied here).
        let write = sbx
            .write(skills.join("pdf").join("evil.md").to_str().unwrap(), "x")
            .await;
        assert!(write.is_err(), "readable roots must not grant writes");
        std::fs::remove_dir_all(&base).ok();
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
        assert!(
            matches!(read, Err(SandboxError::Escape(_))),
            "symlink read escape not blocked: {read:?}"
        );

        // Writing a *new* file through the symlink is also refused (the symlinked
        // ancestor resolves outside root).
        let write = sbx.resolve_for_write("escape/planted.txt").await;
        assert!(
            matches!(write, Err(SandboxError::Escape(_))),
            "symlink write escape not blocked: {write:?}"
        );
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

    #[tokio::test]
    async fn relative_dot_and_absolute_aliases_share_one_write_lock() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-lock-alias-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.txt"), "x").unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());

        let held = sbx.path_guard("x.txt").await.unwrap();
        let absolute = dir.join("x.txt").to_string_lossy().into_owned();
        for alias in ["./x.txt", absolute.as_str()] {
            let result =
                tokio::time::timeout(std::time::Duration::from_millis(50), sbx.path_guard(alias))
                    .await;
            assert!(
                result.is_err(),
                "{alias:?} acquired a different lock for the same target"
            );
        }
        drop(held);

        // Dropping the canonical guard releases every spelling's waiter lane.
        tokio::time::timeout(std::time::Duration::from_secs(1), sbx.path_guard(&absolute))
            .await
            .expect("absolute alias remained blocked")
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_leaf_aliases_share_the_physical_targets_write_lock() {
        let dir =
            std::env::temp_dir().join(format!("medha-sbx-lock-symlink-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("target.txt"), "x").unwrap();
        std::os::unix::fs::symlink("target.txt", dir.join("alias.txt")).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());

        let held = sbx.path_guard("target.txt").await.unwrap();
        let absolute_alias = dir.join("alias.txt").to_string_lossy().into_owned();
        for alias in ["alias.txt", absolute_alias.as_str()] {
            let result =
                tokio::time::timeout(std::time::Duration::from_millis(50), sbx.path_guard(alias))
                    .await;
            assert!(
                result.is_err(),
                "{alias:?} acquired a different lock from its symlink target"
            );
        }
        drop(held);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Windows and the usual macOS filesystems treat case-only spellings as one
    /// prospective directory entry. The target does not exist yet, so this
    /// specifically exercises key normalization rather than whole-file
    /// canonicalization.
    #[cfg(any(windows, target_os = "macos"))]
    #[tokio::test]
    async fn prospective_case_aliases_share_one_write_lock() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-lock-case-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());

        let held = sbx.path_guard("BrandNew.txt").await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            sbx.path_guard("brandnew.txt"),
        )
        .await;
        assert!(
            result.is_err(),
            "case-only prospective aliases acquired different locks"
        );
        drop(held);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A target that does not exist (including its intermediate directory) must
    /// keep one identity before and after creation. The second task uses an
    /// absolute spelling and must wait, then read the first task's committed
    /// bytes before appending its own.
    #[tokio::test]
    async fn concurrent_new_file_read_modify_write_serializes_across_aliases() {
        let dir = std::env::temp_dir().join(format!("medha-sbx-lock-new-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let first = sbx.path_guard("new/./state.txt").await.unwrap();

        let absolute = dir.join("new/state.txt").to_string_lossy().into_owned();
        let contender_sbx = sbx.clone();
        let mut contender = tokio::spawn(async move {
            let guard = contender_sbx.path_guard(&absolute).await.unwrap();
            let mut current = std::fs::read_to_string(guard.resolved()).unwrap_or_default();
            current.push('B');
            contender_sbx.write_guarded(&guard, &current).await.unwrap();
        });

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut contender)
                .await
                .is_err(),
            "absolute alias entered the new-file critical section concurrently"
        );
        sbx.write_guarded(&first, "A").await.unwrap();
        drop(first);
        contender.await.unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("new/state.txt")).unwrap(),
            "AB",
            "the second RMW must observe the first committed create"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn parent_component_swaps_cannot_redirect_write_snapshot_or_restore() {
        let base = std::env::temp_dir().join(format!("medha-cap-swap-{}", ulid::Ulid::new()));
        let root = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("parent")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("parent/file.txt"), "v1").unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&root).unwrap();

        // Produce a real snapshot, then pin the target's parent directory by
        // descriptor before the attacker replaces its workspace entry.
        let snapshot = sbx
            .write("parent/file.txt", "v2")
            .await
            .unwrap()
            .expect("snapshot");
        let guard = sbx.path_guard("parent/file.txt").await.unwrap();
        let held_parent = root.join("parent-held");
        std::fs::rename(root.join("parent"), &held_parent).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("parent")).unwrap();

        sbx.write_guarded(&guard, "v3").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(held_parent.join("file.txt")).unwrap(),
            "v3"
        );
        assert!(
            !outside.join("file.txt").exists(),
            "write followed a swapped parent symlink"
        );

        sbx.restore_guarded(&guard, Some(&snapshot)).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(held_parent.join("file.txt")).unwrap(),
            "v1"
        );
        assert!(
            !outside.join("file.txt").exists(),
            "restore followed a swapped parent symlink"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_parent_symlink_swap_fails_closed() {
        let base = std::env::temp_dir().join(format!("medha-cap-missing-{}", ulid::Ulid::new()));
        let root = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&root).unwrap();

        let guard = sbx.path_guard("new-parent/file.txt").await.unwrap();
        std::os::unix::fs::symlink(&outside, root.join("new-parent")).unwrap();
        assert!(
            sbx.write_guarded(&guard, "blocked").await.is_err(),
            "a pending parent replaced by a symlink must fail closed"
        );
        assert!(!outside.join("file.txt").exists());
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn write_lock_table_returns_to_steady_state() {
        let dir = std::env::temp_dir().join(format!("medha-lock-gc-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();

        // Deterministically model cancellation after a unique canonical lane
        // was inserted but before an OwnedMutexGuard was returned. This was the
        // remaining Weak-map leak: no WritePathGuard existed to perform cleanup.
        for n in 0..20_000 {
            drop(sbx.path_lock(&dir.join(format!("cancelled-{n}.txt"))));
        }
        for n in 0..2_000 {
            drop(sbx.path_guard(&format!("unique-{n}.txt")).await.unwrap());
        }
        assert!(
            sbx.write_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "dead per-path mutexes accumulated for the process lifetime"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_replacement_preserves_executable_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("medha-mode-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("script.sh");
        std::fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();
        sbx.write("script.sh", "#!/bin/sh\nexit 1\n").await.unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o7777, 0o755);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_replacement_preserves_read_only_mode_and_new_files_are_not_executable() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = std::env::temp_dir().join(format!("medha-mode-ro-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("read-only.txt");
        std::fs::write(&target, "v1").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444)).unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();

        sbx.write("read-only.txt", "v2").await.unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o7777, 0o444);

        sbx.write("new.txt", "new").await.unwrap();
        let new_mode = std::fs::metadata(dir.join("new.txt")).unwrap().mode() & 0o7777;
        assert_eq!(
            new_mode & 0o111,
            0,
            "new files follow 0666-and-umask semantics and are never executable"
        );
        assert_ne!(new_mode & 0o600, 0, "the owner must retain file access");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn atomic_replacement_and_restore_preserve_extended_attributes() {
        use std::os::fd::AsRawFd;

        fn set_xattr(file: &std::fs::File, name: &std::ffi::CStr, value: &[u8]) -> bool {
            unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                    0,
                ) == 0
            }
        }
        fn get_xattr(file: &std::fs::File, name: &std::ffi::CStr) -> Vec<u8> {
            let length = unsafe {
                libc::fgetxattr(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                )
            };
            assert!(
                length >= 0,
                "fgetxattr: {}",
                std::io::Error::last_os_error()
            );
            let mut value = vec![0u8; length as usize];
            let read = unsafe {
                libc::fgetxattr(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                    0,
                    0,
                )
            };
            assert!(read >= 0, "fgetxattr: {}", std::io::Error::last_os_error());
            value.truncate(read as usize);
            value
        }

        let dir = std::env::temp_dir().join(format!("medha-xattr-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("file.txt");
        std::fs::write(&target, "v1").unwrap();
        let name = std::ffi::CString::new("com.medha.audit-test").unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&target)
            .unwrap();
        if !set_xattr(&file, &name, b"original") {
            let error = std::io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::ENOTSUP || code == libc::EOPNOTSUPP)
            {
                std::fs::remove_dir_all(&dir).ok();
                return;
            }
            panic!("fsetxattr: {error}");
        }
        drop(file);

        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();
        let snapshot = sbx.write("file.txt", "v2").await.unwrap().unwrap();
        let file = std::fs::File::open(&target).unwrap();
        assert_eq!(get_xattr(&file, &name), b"original");
        drop(file);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&target)
            .unwrap();
        assert!(set_xattr(&file, &name, b"changed-after-write"));
        drop(file);
        sbx.restore("file.txt", Some(&snapshot)).await.unwrap();
        let restored = std::fs::File::open(&target).unwrap();
        assert_eq!(get_xattr(&restored, &name), b"original");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn atomic_replacement_preserves_extended_acl_entries() {
        use std::os::fd::AsRawFd;

        fn acl_text(file: &std::fs::File) -> Option<String> {
            type Acl = *mut libc::c_void;
            unsafe extern "C" {
                fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> Acl;
                fn acl_to_text(acl: Acl, len: *mut libc::ssize_t) -> *mut libc::c_char;
                fn acl_free(value: *mut libc::c_void) -> libc::c_int;
            }
            const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
            let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
            if acl.is_null() {
                return None;
            }
            let mut length = 0;
            let text = unsafe { acl_to_text(acl, &mut length) };
            let value = (!text.is_null()).then(|| {
                String::from_utf8_lossy(unsafe {
                    std::slice::from_raw_parts(text.cast::<u8>(), length.max(0) as usize)
                })
                .into_owned()
            });
            unsafe {
                if !text.is_null() {
                    acl_free(text.cast());
                }
                acl_free(acl);
            }
            value
        }

        let dir = std::env::temp_dir().join(format!("medha-acl-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("file.txt");
        std::fs::write(&target, "v1").unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .arg("+a")
            .arg("everyone allow read")
            .arg(&target)
            .status()
            .unwrap();
        if !status.success() {
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        let before = acl_text(&std::fs::File::open(&target).unwrap())
            .expect("filesystem accepted an ACL but could not return it");
        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();
        let snapshot = sbx.write("file.txt", "v2").await.unwrap().unwrap();
        let after =
            acl_text(&std::fs::File::open(&target).unwrap()).expect("replacement dropped the ACL");
        assert_eq!(after, before);

        assert!(
            std::process::Command::new("/bin/chmod")
                .arg("-N")
                .arg(&target)
                .status()
                .unwrap()
                .success()
        );
        sbx.restore("file.txt", Some(&snapshot)).await.unwrap();
        let restored = acl_text(&std::fs::File::open(&target).unwrap())
            .expect("restore dropped the snapshotted ACL");
        assert_eq!(restored, before);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn atomic_replacement_and_restore_preserve_user_file_flags() {
        use std::os::fd::AsRawFd;

        fn flags(file: &std::fs::File) -> libc::c_uint {
            let mut status: libc::stat = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::fstat(file.as_raw_fd(), &mut status) }, 0);
            status.st_flags
        }

        let dir = std::env::temp_dir().join(format!("medha-flags-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("file.txt");
        std::fs::write(&target, "v1").unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&target)
            .unwrap();
        let expected = libc::UF_HIDDEN | libc::UF_NODUMP;
        if unsafe { libc::fchflags(file.as_raw_fd(), expected) } != 0 {
            let error = std::io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::ENOTSUP || code == libc::EOPNOTSUPP)
            {
                std::fs::remove_dir_all(&dir).ok();
                return;
            }
            panic!("fchflags: {error}");
        }
        drop(file);

        let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();
        let snapshot = sbx.write("file.txt", "v2").await.unwrap().unwrap();
        assert_eq!(
            flags(&std::fs::File::open(&target).unwrap()) & expected,
            expected
        );

        let file = std::fs::File::open(&target).unwrap();
        assert_eq!(unsafe { libc::fchflags(file.as_raw_fd(), 0) }, 0);
        drop(file);
        sbx.restore("file.txt", Some(&snapshot)).await.unwrap();
        assert_eq!(
            flags(&std::fs::File::open(&target).unwrap()) & expected,
            expected
        );
        let file = std::fs::File::open(&target).unwrap();
        unsafe {
            libc::fchflags(file.as_raw_fd(), 0);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn new_file_publication_never_clobbers_an_intervening_create() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("medha-create-cas-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let canonical_dir = dir.canonicalize().unwrap();
        let parent = open_absolute_dir(&canonical_dir, false).unwrap().unwrap();
        let temporary = OsStr::new(".private-temp");
        let target = OsStr::new("target.txt");
        let mut temp_file = create_file_at(&parent, temporary, 0o600).unwrap();
        temp_file.write_all(b"medha").unwrap();
        temp_file.sync_all().unwrap();

        // Simulate another process creating the approved-missing destination
        // in the last instant before publication.
        std::fs::write(dir.join(target), "other process").unwrap();
        assert!(matches!(
            publish_new_file_at(&parent, temporary, target),
            Err(SandboxError::Conflict(_))
        ));
        assert_eq!(
            std::fs::read_to_string(dir.join(target)).unwrap(),
            "other process"
        );

        unlink_file_at(&parent, temporary).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn line_range_reader_stops_after_requested_lines_of_a_huge_input() {
        use std::io::Read;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct SyntheticHuge {
            position: u64,
            supplied: Arc<AtomicUsize>,
        }
        impl Read for SyntheticHuge {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if buffer.is_empty() {
                    return Ok(0);
                }
                for byte in buffer.iter_mut() {
                    let position = self.position as usize;
                    *byte = if position < 50 {
                        b"line\n"[position % 5]
                    } else {
                        b'x'
                    };
                    self.position += 1;
                }
                self.supplied.fetch_add(buffer.len(), Ordering::Relaxed);
                Ok(buffer.len())
            }
        }

        let supplied = Arc::new(AtomicUsize::new(0));
        let result = read_line_range_bounded(
            SyntheticHuge {
                position: 0,
                supplied: supplied.clone(),
            },
            5 * 1024 * 1024 * 1024,
            1,
            Some(10),
            64 * 1024 * 1024,
            2_000_000,
        )
        .unwrap();

        assert_eq!(result.content, "line\n".repeat(10));
        assert_eq!(result.bytes_scanned, 50);
        assert!(result.has_more);
        assert_eq!(result.total_lines, None);
        assert!(
            supplied.load(Ordering::Relaxed) <= 64 * 1024,
            "the streaming reader consumed more than one fixed-size buffer"
        );
    }

    #[test]
    fn line_range_reader_preserves_terminators_and_reports_known_eof() {
        let bytes = b"one\r\ntwo\nlast";
        let result = read_line_range_bounded(
            std::io::Cursor::new(bytes),
            bytes.len() as u64,
            2,
            Some(u64::MAX),
            1024,
            1024,
        )
        .unwrap();
        assert_eq!(result.content, "two\nlast");
        assert_eq!(result.start_line, 2);
        assert_eq!(result.end_line, 3);
        assert_eq!(result.total_lines, Some(3));
        assert!(!result.has_more);
    }

    #[test]
    fn line_range_reader_enforces_input_and_output_byte_ceilings() {
        let input = vec![b'x'; 100];
        let scan_error =
            read_line_range_bounded(std::io::Cursor::new(&input), 100, 2, Some(1), 32, 100)
                .unwrap_err();
        assert!(scan_error.to_string().contains("input ceiling"));

        let output_error =
            read_line_range_bounded(std::io::Cursor::new(b"abcdefgh\n"), 9, 1, Some(1), 100, 4)
                .unwrap_err();
        assert!(output_error.to_string().contains("output ceiling"));
    }
}
