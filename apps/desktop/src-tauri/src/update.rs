//! A newer Medha is fetched and verified quietly, and installed only when the user asks.
use serde_json::{Value, json};
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

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
    let exiting = app.clone();
    let updater = app
        .updater_builder()
        .on_before_exit(move || {
            // The Windows installer exits directly, bypassing RunEvent. Keep
            // native children owned through reap before preserving its cleanup.
            crate::reap_terminals(&exiting);
            exiting.cleanup_before_exit();
        })
        .build()
        .map_err(|error| error.to_string())?;
    let Some(update) = updater.check().await.map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    // The download is refused here unless its signature matches the key built into the app.
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|error| error.to_string())?;
    let found = json!({ "version": update.version });
    *updates.ready.lock().map_err(|error| error.to_string())? = Some((update, bytes));
    Ok(Some(found))
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
