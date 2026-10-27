//! A newer Medha is fetched and verified quietly, and installed only when the user asks.
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_updater::{Update, Updater, UpdaterExt};

/// The same release described with GitHub's own download links, read only when the mirror fails.
const FALLBACK: &str =
    "https://github.com/kashyaprajharsh/medha-agent/releases/latest/download/latest-github.json";
/// Shared across a name's addresses, so one that never answers costs a few seconds before the next.
const CONNECT: Duration = Duration::from_secs(20);
/// A download that sends nothing for this long is given up; a slow one that keeps going is not.
const STALLED: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Updates {
    ready: Mutex<Option<(Update, Vec<u8>)>>,
}

impl Updates {
    fn version(&self) -> Option<String> {
        let ready = self.ready.lock().ok()?;
        ready.as_ref().map(|(update, _)| update.version.clone())
    }
}

/// Only an installed copy that the updater knows how to replace is ever offered one.
fn replaceable() -> bool {
    if cfg!(debug_assertions) {
        return false;
    }
    !cfg!(target_os = "linux") || std::env::var_os("APPIMAGE").is_some()
}

#[tauri::command]
pub async fn update_check(
    app: AppHandle,
    updates: State<'_, Updates>,
) -> Result<Option<Value>, String> {
    if !replaceable() {
        return Ok(None);
    }
    if let Some(version) = updates.version() {
        return Ok(Some(json!({ "version": version })));
    }
    // The mirror first; when its manifest or its download fails, GitHub's own links once.
    let fetched = match fetch(&updater(&app, None)?).await {
        Ok(fetched) => fetched,
        Err(mirror) => {
            let github = updater(&app, Some(FALLBACK))?;
            fetch(&github)
                .await
                .map_err(|error| format!("{mirror}; from GitHub too: {error}"))?
        }
    };
    let Some((update, bytes)) = fetched else {
        return Ok(None);
    };
    let found = json!({ "version": update.version });
    *updates.ready.lock().map_err(|error| error.to_string())? = Some((update, bytes));
    Ok(Some(found))
}

/// The configured endpoints, or `only` this one.
fn updater(app: &AppHandle, only: Option<&str>) -> Result<Updater, String> {
    let exiting = app.clone();
    let mut builder = app
        .updater_builder()
        .configure_client(|client| client.connect_timeout(CONNECT).read_timeout(STALLED))
        .on_before_exit(move || {
            // The Windows installer exits directly, bypassing RunEvent. Keep
            // native children owned through reap before preserving its cleanup.
            crate::reap_terminals(&exiting);
            exiting.cleanup_before_exit();
        });
    if let Some(only) = only {
        let url = only.parse().map_err(|error| format!("{only}: {error}"))?;
        builder = builder
            .endpoints(vec![url])
            .map_err(|error| error.to_string())?;
    }
    builder.build().map_err(|error| error.to_string())
}

/// The newer release and its bytes, refused unless its signature matches the key built into the app.
async fn fetch(updater: &Updater) -> Result<Option<(Update, Vec<u8>)>, String> {
    let Some(update) = updater.check().await.map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|error| error.to_string())?;
    Ok(Some((update, bytes)))
}

#[tauri::command]
pub async fn update_apply(app: AppHandle) -> Result<(), String> {
    let ready = app
        .state::<Updates>()
        .ready
        .lock()
        .map_err(|error| error.to_string())?
        .take();
    let (update, bytes) = ready.ok_or("No update is ready")?;
    tauri::async_runtime::spawn_blocking(move || update.install(bytes))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    let exiting = app.clone();
    tauri::async_runtime::spawn_blocking(move || crate::reap_terminals(&exiting))
        .await
        .map_err(|error| error.to_string())?;
    app.restart()
}
