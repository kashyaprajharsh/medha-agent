use std::path::{Path, PathBuf};

pub fn project_root(folder: &Path, home: &Path) -> std::io::Result<Option<PathBuf>> {
    let folder = folder.canonicalize()?;
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    if folder == home {
        return Ok(None);
    }
    for parent in folder.ancestors() {
        if parent == home {
            break;
        }
        if std::fs::symlink_metadata(parent.join(".git")).is_ok() {
            return Ok(Some(parent.to_path_buf()));
        }
    }
    Ok(Some(folder))
}
