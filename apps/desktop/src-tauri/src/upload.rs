//! A file dropped on the Add sheet: the webview hands over its bytes, not its path, so it is kept in a private folder.
use std::path::PathBuf;

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

/// Each drop gets its own unguessable folder, readable by this user only.
fn keep(name: &str, bytes: &[u8]) -> Result<String, String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    let folder: PathBuf = std::env::temp_dir().join(format!(
        "medha-drop-{}",
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
