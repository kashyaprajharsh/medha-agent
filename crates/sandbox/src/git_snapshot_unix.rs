//! Snapshot IO stays relative to held directories, including during repair.

use crate::git_snapshot::{Mode, Node};
use std::{
    ffi::{CStr, OsStr, OsString},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, IntoRawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path},
};

fn io(error: crate::SandboxError) -> std::io::Error {
    std::io::Error::other(error)
}

fn names(directory: &File, limit: usize) -> std::io::Result<Vec<OsString>> {
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    // A fresh description has its own directory cursor; dup shares the cursor.
    let fd = crate::open_dir_at(directory, OsStr::new("."))?.into_raw_fd();
    let raw = unsafe { libc::fdopendir(fd) };
    if raw.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(std::io::Error::last_os_error());
    }
    let directory = Directory(raw);
    let mut result = Vec::new();
    loop {
        errno::set_errno(errno::Errno(0));
        let entry = unsafe { libc::readdir(directory.0) };
        if entry.is_null() {
            let error = errno::errno().0;
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if result.len() >= limit {
            return Err(std::io::Error::other(
                "snapshot directory exceeds its entry limit",
            ));
        }
        result.push(OsStr::from_bytes(name).to_owned());
    }
    Ok(result)
}

fn capture(
    parent: &File,
    leaf: &OsStr,
    budget: &mut (usize, usize),
    recursive: bool,
) -> std::io::Result<Node> {
    if let Ok(directory) = crate::open_dir_at(parent, leaf) {
        budget.1 = budget
            .1
            .checked_sub(1)
            .ok_or_else(|| std::io::Error::other("snapshot entry limit exceeded"))?;
        let mode = Mode::from(&directory.metadata()?);
        let mut children = std::collections::BTreeMap::new();
        if recursive {
            for name in names(&directory, budget.1)? {
                children.insert(name.clone(), capture(&directory, &name, budget, true)?);
            }
        }
        return Ok(Node::Directory(children, mode));
    }
    let Some(file) = crate::open_regular_file_at(parent, leaf).map_err(io)? else {
        return Ok(Node::Missing);
    };
    let metadata = file.metadata()?;
    if metadata.len() > budget.0 as u64 {
        return Err(std::io::Error::other("snapshot byte limit exceeded"));
    }
    budget.1 = budget
        .1
        .checked_sub(1)
        .ok_or_else(|| std::io::Error::other("snapshot entry limit exceeded"))?;
    let mut bytes = Vec::new();
    file.take(budget.0 as u64 + 1).read_to_end(&mut bytes)?;
    budget.0 = budget
        .0
        .checked_sub(bytes.len())
        .ok_or_else(|| std::io::Error::other("snapshot byte limit exceeded"))?;
    Ok(Node::File(bytes, Mode::from(&metadata)))
}

fn unlink(parent: &File, leaf: &OsStr) -> std::io::Result<()> {
    let flags = if let Ok(directory) = crate::open_dir_at(parent, leaf) {
        for name in names(&directory, 1_000_000)? {
            unlink(&directory, &name)?;
        }
        libc::AT_REMOVEDIR
    } else {
        0
    };
    let name = crate::component_cstring(leaf).map_err(io)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error)
    }
}

fn restore(parent: &File, leaf: &OsStr, node: &Node) -> std::io::Result<()> {
    unlink(parent, leaf)?;
    match node {
        Node::Missing => return Ok(()),
        Node::File(bytes, mode) => {
            let mut file = crate::create_file_at(parent, leaf, 0o666).map_err(io)?;
            file.write_all(bytes)?;
            mode.apply_file(&file)?;
            file.sync_all()?;
        }
        Node::Directory(children, mode) => {
            crate::mkdir_at(parent, leaf)?;
            let directory = crate::open_dir_at(parent, leaf)?;
            for (name, node) in children {
                restore(&directory, name, node)?;
            }
            mode.apply_file(&directory)?;
        }
    }
    parent.sync_all()
}

fn below<T>(
    root: &Path,
    relative: &Path,
    repair: bool,
    operation: impl FnOnce(&File, &OsStr) -> std::io::Result<T>,
) -> std::io::Result<Option<T>> {
    let mut parts = relative
        .components()
        .map(|part| match part {
            Component::Normal(value) => Ok(value),
            _ => Err(std::io::Error::other(
                "snapshot path must stay below its root",
            )),
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let leaf = parts
        .pop()
        .ok_or_else(|| std::io::Error::other("snapshot path is empty"))?;
    let Some(mut parent) = crate::open_absolute_dir(root, false).map_err(io)? else {
        return Ok(None);
    };
    for name in parts {
        match crate::open_dir_at(&parent, name) {
            Ok(next) => parent = next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !repair => {
                return Ok(None);
            }
            Err(error)
                if repair
                    && matches!(
                        error.raw_os_error(),
                        Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                    ) =>
            {
                unlink(&parent, name)?;
                crate::mkdir_at(&parent, name)?;
                parent = crate::open_dir_at(&parent, name)?;
            }
            Err(error) => return Err(error),
        }
    }
    operation(&parent, leaf).map(Some)
}

pub(crate) fn read(
    root: &Path,
    relative: &Path,
    budget: &mut (usize, usize),
    recursive: bool,
) -> std::io::Result<Node> {
    below(root, relative, false, |parent, leaf| {
        capture(parent, leaf, budget, recursive)
    })
    .map(|value| value.unwrap_or(Node::Missing))
}

pub(crate) fn write(
    root: &Path,
    relative: &Path,
    node: &Node,
    repair: bool,
) -> std::io::Result<()> {
    below(root, relative, repair, |parent, leaf| {
        restore(parent, leaf, node)
    })?
    .ok_or_else(|| std::io::Error::other("snapshot parent disappeared"))
}
