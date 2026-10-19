#[cfg(any(windows, test))]
use crate::exec::ExecError;
use std::{collections::BTreeMap, path::Path};
#[cfg(any(windows, test))]
use std::{
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, Weak},
};

#[cfg(any(windows, test))]
const BYTES: usize = 16 * 1024 * 1024;
#[cfg(any(windows, test))]
const ENTRIES: usize = 4096;

#[cfg(windows)]
pub(crate) struct Backend {
    pub extra: Vec<PathBuf>,
    pub approved: permissions::ApprovedRoots,
}

#[cfg(windows)]
#[async_trait::async_trait]
impl crate::exec::ExecBackend for Backend {
    fn build_command(
        &self,
        request: &crate::exec::ExecRequest,
    ) -> Result<tokio::process::Command, ExecError> {
        crate::exec::ExecBackend::build_command(&crate::exec::HostBackend, request)
    }
    fn command_resources(
        &self,
        request: &crate::exec::ExecRequest,
    ) -> Result<crate::command_resources::CommandResources, ExecError> {
        let mut roots = vec![request.cwd.clone()];
        roots.extend(self.extra.iter().cloned());
        roots.extend(self.approved.write_roots());
        roots.extend(request.write_roots.iter().cloned());
        let owned = self.approved.owned_roots();
        roots.retain(|root| !owned.contains(root));
        crate::command_resources::CommandResources::snapshot(&roots)
    }
    fn label(&self) -> &str {
        "host"
    }
}
#[cfg(any(windows, test))]
type Slots = BTreeMap<PathBuf, Weak<(Mutex<bool>, Condvar)>>;
#[cfg(any(windows, test))]
static SLOTS: Mutex<Slots> = Mutex::new(BTreeMap::new());

#[cfg(any(windows, test))]
struct Lease(Arc<(Mutex<bool>, Condvar)>);
#[cfg(any(windows, test))]
impl Drop for Lease {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = false;
        self.0.1.notify_one();
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Mode {
    readonly: bool,
    #[cfg(unix)]
    bits: u32,
}

impl Mode {
    pub(crate) fn from(metadata: &std::fs::Metadata) -> Self {
        Self {
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            bits: {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode()
            },
        }
    }

    #[cfg(unix)]
    pub(crate) fn apply_file(self, file: &std::fs::File) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(self.bits))
    }

    #[cfg(not(unix))]
    fn apply(self, path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(self.bits))
        }
        #[cfg(not(unix))]
        {
            let mut permissions = std::fs::metadata(path)?.permissions();
            permissions.set_readonly(self.readonly);
            std::fs::set_permissions(path, permissions)
        }
    }
}

#[derive(Clone, PartialEq)]
pub(crate) enum Node {
    Missing,
    File(Vec<u8>, Mode),
    Directory(BTreeMap<std::ffi::OsString, Node>, Mode),
}

#[cfg(any(windows, test))]
pub(crate) struct Snapshot {
    root: PathBuf,
    entry: Node,
    controls: BTreeMap<&'static str, Node>,
    identity: Option<(u64, u64)>,
    _lease: Lease,
    restored: bool,
}

#[cfg(any(windows, test))]
impl Snapshot {
    pub fn new(root: &Path) -> Result<Self, ExecError> {
        let root = root.canonicalize().map_err(failed)?;
        let slot = {
            let mut slots = SLOTS.lock().unwrap();
            slots.retain(|_, slot| slot.strong_count() > 0);
            if slots.len() >= 128 && !slots.contains_key(&root) {
                return Err(ExecError::Unavailable(
                    "Git snapshot exceeds 128 active folders".into(),
                ));
            }
            slots.get(&root).and_then(Weak::upgrade).unwrap_or_else(|| {
                let slot = Arc::new((Mutex::new(false), Condvar::new()));
                slots.insert(root.clone(), Arc::downgrade(&slot));
                slot
            })
        };
        let mut busy = slot.0.lock().unwrap();
        while *busy {
            busy = slot.1.wait(busy).unwrap();
        }
        *busy = true;
        drop(busy);
        let lease = Lease(slot);
        let git = root.join(".git");
        let protected = if git.is_dir() {
            ["config", "commondir", "hooks"]
                .map(|name| git.join(name))
                .into_iter()
                .filter(|path| path.symlink_metadata().is_ok())
                .collect::<Vec<_>>()
        } else if git.symlink_metadata().is_ok() {
            vec![git.clone()]
        } else {
            Vec::new()
        };
        crate::git_controls::validate_aliases(&protected)?;
        let mut budget = (BYTES, ENTRIES);
        let mut entry = read(&git, &mut budget, false).map_err(failed)?;
        let mut controls = BTreeMap::new();
        let identity = if matches!(entry, Node::Directory(_, _)) {
            for name in ["config", "commondir", "hooks"] {
                controls.insert(
                    name,
                    read(&git.join(name), &mut budget, true).map_err(failed)?,
                );
            }
            if let Node::Directory(children, _) = &mut entry {
                children.clear();
            }
            Some(directory_id(&git).map_err(failed)?)
        } else {
            None
        };
        Ok(Self {
            root,
            entry,
            controls,
            identity,
            _lease: lease,
            restored: false,
        })
    }

    pub fn restore(mut self) -> Option<String> {
        let result = self.restore_inner();
        self.restored = true;
        match result {
            Ok(false) => None,
            Ok(true) => Some("Git control files changed during the command and were restored. This check does not undo what the command did while it ran.".into()),
            Err(error) => Some(format!("Git control-file restoration failed: {error}. Repair .git before using this checkout.")),
        }
    }

    fn restore_inner(&self) -> std::io::Result<bool> {
        let git = self.root.join(".git");
        let mut changed = false;
        if let Some(identity) = self.identity {
            if directory_id(&git).ok() != Some(identity) {
                changed = true;
                let mut original = None;
                for candidate in std::fs::read_dir(&self.root)?.take(ENTRIES) {
                    let candidate = candidate?.path();
                    if directory_id(&candidate).ok() == Some(identity) {
                        original = Some(candidate);
                        break;
                    }
                }
                remove(&git)?;
                if let Some(original) = original {
                    std::fs::rename(original, &git)?;
                } else {
                    std::fs::create_dir(&git)?;
                }
            }
            for (name, node) in &self.controls {
                let path = git.join(name);
                if read(&path, &mut (BYTES, ENTRIES), true).ok().as_ref() != Some(node) {
                    changed = true;
                    restore_node(&path, node)?;
                }
            }
            if let Node::Directory(_, readonly) = self.entry {
                let mut permissions = std::fs::metadata(&git)?.permissions();
                if permissions.readonly() != readonly.readonly {
                    changed = true;
                    #[cfg(windows)]
                    permissions.set_readonly(readonly.readonly);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        permissions.set_mode(if readonly.readonly {
                            permissions.mode() & !0o222
                        } else {
                            permissions.mode() | 0o200
                        });
                    }
                    std::fs::set_permissions(&git, permissions)?;
                }
            }
        } else if read(&git, &mut (BYTES, ENTRIES), false).ok().as_ref() != Some(&self.entry) {
            changed = true;
            restore_node(&git, &self.entry)?;
        }
        Ok(changed)
    }
}

#[cfg(any(windows, test))]
impl Drop for Snapshot {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.restore_inner();
        }
    }
}

#[cfg(any(windows, test))]
pub(crate) fn read(
    path: &Path,
    budget: &mut (usize, usize),
    recursive: bool,
) -> std::io::Result<Node> {
    #[cfg(unix)]
    {
        crate::git_snapshot_unix::read(
            path.parent()
                .ok_or_else(|| std::io::Error::other("snapshot has no parent"))?,
            Path::new(
                path.file_name()
                    .ok_or_else(|| std::io::Error::other("snapshot has no name"))?,
            ),
            budget,
            recursive,
        )
    }
    #[cfg(not(unix))]
    crate::snapshot_paths::at_path(path, |path| read_inner(path, budget, recursive))
        .map(|value| value.unwrap_or(Node::Missing))
}

pub(crate) fn read_below(
    root: &Path,
    relative: &Path,
    budget: &mut (usize, usize),
) -> std::io::Result<Node> {
    #[cfg(unix)]
    {
        crate::git_snapshot_unix::read(root, relative, budget, false)
    }
    #[cfg(not(unix))]
    crate::snapshot_paths::below(root, relative, false, |path| {
        read_inner(path, budget, false)
    })
    .map(|value| value.unwrap_or(Node::Missing))
}

#[cfg(not(unix))]
fn read_inner(path: &Path, budget: &mut (usize, usize), recursive: bool) -> std::io::Result<Node> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Node::Missing),
        Err(error) => return Err(error),
    };
    if linked(&meta) {
        return Err(std::io::Error::other(
            "linked Git control files cannot be snapshotted safely",
        ));
    }
    if budget.1 == 0 {
        return Err(std::io::Error::other("Git snapshot exceeds 4096 entries"));
    }
    budget.1 -= 1;
    if meta.is_file() {
        if meta.len() > budget.0 as u64 {
            return Err(std::io::Error::other("Git snapshot exceeds 16 MiB"));
        }
        use std::io::Read;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        if linked(&file.metadata()?) {
            return Err(std::io::Error::other("linked Git control file"));
        }
        let mut bytes = Vec::new();
        file.take(budget.0 as u64 + 1).read_to_end(&mut bytes)?;
        budget.0 = budget
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("Git snapshot exceeds 16 MiB"))?;
        Ok(Node::File(bytes, Mode::from(&meta)))
    } else if meta.is_dir() {
        let mut children = BTreeMap::new();
        if recursive {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                children.insert(entry.file_name(), read_inner(&entry.path(), budget, true)?);
            }
        }
        Ok(Node::Directory(children, Mode::from(&meta)))
    } else {
        Err(std::io::Error::other(
            "Git control entry is not a regular file or directory",
        ))
    }
}

#[cfg(any(windows, test))]
fn remove(path: &Path) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if meta.permissions().readonly() && !linked(&meta) {
        let mut permissions = meta.permissions();
        // Windows only clears the read-only attribute; the Unix path sets just the owner bit.
        #[cfg(windows)]
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(permissions.mode() | 0o200);
        }
        std::fs::set_permissions(path, permissions)?;
    }
    if meta.is_dir() && !linked(&meta) {
        std::fs::remove_dir_all(path)
    } else {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x10 != 0 {
                return std::fs::remove_dir(path);
            }
        }
        std::fs::remove_file(path)
    }
}

#[cfg(any(windows, test))]
pub(crate) fn restore_node(path: &Path, node: &Node) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        crate::git_snapshot_unix::write(
            path.parent()
                .ok_or_else(|| std::io::Error::other("snapshot has no parent"))?,
            Path::new(
                path.file_name()
                    .ok_or_else(|| std::io::Error::other("snapshot has no name"))?,
            ),
            node,
            false,
        )
    }
    #[cfg(not(unix))]
    crate::snapshot_paths::at_path(path, |path| restore_inner(path, node))?
        .ok_or_else(|| std::io::Error::other("snapshot parent disappeared"))
}

pub(crate) fn restore_below(root: &Path, relative: &Path, node: &Node) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        crate::git_snapshot_unix::write(root, relative, node, true)
    }
    #[cfg(not(unix))]
    crate::snapshot_paths::below(root, relative, true, |path| restore_inner(path, node))?
        .ok_or_else(|| std::io::Error::other("snapshot parent disappeared"))
}

#[cfg(not(unix))]
fn restore_inner(path: &Path, node: &Node) -> std::io::Result<()> {
    remove(path)?;
    match node {
        Node::Missing => return Ok(()),
        Node::File(bytes, _) => std::fs::write(path, bytes)?,
        Node::Directory(children, _) => {
            std::fs::create_dir(path)?;
            for (name, node) in children {
                restore_inner(&path.join(name), node)?;
            }
        }
    }
    let mode = match node {
        Node::File(_, value) | Node::Directory(_, value) => *value,
        Node::Missing => return Ok(()),
    };
    mode.apply(path)
}

#[cfg(any(windows, test))]
pub(crate) fn linked(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

#[cfg(any(windows, test))]
fn directory_id(path: &Path) -> std::io::Result<(u64, u64)> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() || linked(&meta) {
        return Err(std::io::Error::other("Git directory was replaced"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok((meta.dev(), meta.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::*;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let mut info = std::mem::MaybeUninit::uninit();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let info = unsafe { info.assume_init() };
        Ok((
            u64::from(info.dwVolumeSerialNumber),
            u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow),
        ))
    }
}

#[cfg(any(windows, test))]
fn failed(error: std::io::Error) -> ExecError {
    ExecError::Unavailable(format!("cannot snapshot Git control files: {error}"))
}

#[cfg(test)]
#[path = "git_snapshot_tests.rs"]
mod tests;
