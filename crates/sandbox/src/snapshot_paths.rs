//! Parent-handle paths shared by snapshot capture and repair.

use std::path::{Component, Path};

pub(crate) fn at_path<T>(
    path: &Path,
    operation: impl FnOnce(&Path) -> std::io::Result<T>,
) -> std::io::Result<Option<T>> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("snapshot path has no parent"))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("snapshot path has no filename"))?;
    below(parent, Path::new(leaf), false, operation)
}

pub(crate) fn below<T>(
    root: &Path,
    relative: &Path,
    repair: bool,
    operation: impl FnOnce(&Path) -> std::io::Result<T>,
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
    {
        let mut checked = std::path::PathBuf::new();
        for part in root.components() {
            checked.push(part);
            if !matches!(part, Component::Normal(_)) {
                continue;
            }
            let metadata = match std::fs::symlink_metadata(&checked) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
            if !metadata.is_dir() || crate::git_snapshot::linked(&metadata) {
                return Err(std::io::Error::other(
                    "snapshot root was replaced by a link",
                ));
            }
        }
        let mut parent = root.to_path_buf();
        for name in parts {
            parent.push(name);
            match std::fs::symlink_metadata(&parent) {
                Ok(metadata) if metadata.is_dir() && !crate::git_snapshot::linked(&metadata) => {}
                Ok(_) if repair => {
                    if std::fs::remove_file(&parent).is_err() {
                        std::fs::remove_dir(&parent)?;
                    }
                    std::fs::create_dir(&parent)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && repair => {
                    std::fs::create_dir(&parent)?
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Ok(_) => {
                    return Err(std::io::Error::other(
                        "snapshot parent was replaced by a link",
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        operation(&parent.join(leaf)).map(Some)
    }
}
