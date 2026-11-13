//! A file dropped on the Add sheet: the webview hands over its bytes, not its path, so it is kept in a private folder.
use std::path::{Path, PathBuf};
use std::time::Duration;

const PREFIX: &str = "medha-drop-";
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// The largest plugin or skill package the backend accepts.
const MAX_BYTES: usize = 32 * 1024 * 1024;
const KINDS: [&str; 4] = ["zip", "skill", "plugin", "md"];

#[tauri::command]
pub async fn extension_upload(request: tauri::ipc::Request<'_>) -> Result<String, String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("Drop a file".into());
    };
    if bytes.len() > MAX_BYTES {
        return Err("Drop a file below 32 MB".into());
    }
    let name = request
        .headers()
        .get("x-medha-name")
        .and_then(|value| value.to_str().ok())
        .and_then(|name| name.rsplit(['/', '\\']).next())
        .filter(|name| !name.starts_with('.'))
        .ok_or("The dropped file has no name")?
        .to_string();
    let kind = name
        .rsplit_once('.')
        .map(|(_, kind)| kind.to_ascii_lowercase());
    if !kind.is_some_and(|kind| KINDS.contains(&kind.as_str())) {
        return Err("Drop a .zip, .skill, .plugin or .md file, or choose a folder".into());
    }
    let bytes = bytes.clone();
    tauri::async_runtime::spawn_blocking(move || keep(&name, &bytes))
        .await
        .map_err(|error| error.to_string())?
}

fn keep(name: &str, bytes: &[u8]) -> Result<String, String> {
    keep_in(&std::env::temp_dir(), name, bytes)
}

/// Each drop gets its own unguessable folder, readable by this user only; drops older than a day are cleared.
fn keep_in(root: &Path, name: &str, bytes: &[u8]) -> Result<String, String> {
    clear_old(root);
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    let folder: PathBuf = root.join(format!(
        "{PREFIX}{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&folder).map_err(|error| error.to_string())?;
    let path = folder.join(name);
    std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "This path cannot be represented as text".to_owned())
}

/// A day is long past any preview and add that followed the drop.
fn clear_old(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry.file_name().to_string_lossy().starts_with(PREFIX)
            && entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > DAY);
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// A new drop clears drops older than a day and leaves recent ones.
    #[test]
    fn a_drop_clears_old_drops_only() {
        let root = test_support::scratch("medha-drops");
        let old = keep_in(&root, "old.zip", b"old").unwrap();
        let recent = keep_in(&root, "recent.zip", b"recent").unwrap();
        let old_folder = Path::new(&old).parent().unwrap();
        std::fs::File::open(old_folder)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60))
            .unwrap();
        let new = keep_in(&root, "new.zip", b"new").unwrap();
        assert!(!old_folder.exists(), "an old drop was kept");
        assert!(Path::new(&recent).is_file() && Path::new(&new).is_file());
    }
}
