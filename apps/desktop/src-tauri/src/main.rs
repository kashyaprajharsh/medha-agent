mod backend;
mod files;
mod git;
mod kept;
mod live;
mod outputs;
mod sleep;
mod terminal;
mod update;
mod view;
mod workspaces;

use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Manager, State, ipc::Channel};
use workspaces::{Runtime, Workspaces};

struct DesktopState {
    workspaces: Arc<Workspaces>,
    terminals: Mutex<HashMap<String, Arc<Runtime>>>,
    views: Arc<view::Views>,
}
#[tauri::command]
fn workspace_list(state: State<'_, DesktopState>) -> Result<Value, String> {
    state.workspaces.list()
}
#[tauri::command]
async fn workspace_choose(
    state: State<'_, DesktopState>,
) -> Result<Option<workspaces::Workspace>, String> {
    let Some(folder) = rfd::AsyncFileDialog::new()
        .set_title("Open a project folder")
        .pick_folder()
        .await
    else {
        return Ok(None);
    };
    state.workspaces.add(folder.path().to_path_buf()).map(Some)
}
#[tauri::command]
fn workspace_info(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
) -> Result<String, String> {
    Ok(state
        .workspaces
        .resolve(&workspace_id, chat_key.as_deref(), session_id.as_deref())?
        .path
        .to_string_lossy()
        .into_owned())
}
#[tauri::command]
fn workspace_reveal(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
) -> Result<(), String> {
    files::open_path(
        &state
            .workspaces
            .start(&workspace_id, chat_key.as_deref(), session_id.as_deref())?
            .path,
    )
}
#[tauri::command]
fn workspace_branch(
    state: State<'_, DesktopState>,
    workspace_id: String,
) -> Result<Option<String>, String> {
    Ok(git::branch(
        &state.workspaces.workspace(&workspace_id)?.path,
    ))
}
#[tauri::command]
async fn list_sessions(
    state: State<'_, DesktopState>,
    workspace_id: String,
) -> Result<Value, String> {
    let workspaces = state.workspaces.clone();
    tauri::async_runtime::spawn_blocking(move || workspaces.list_sessions(&workspace_id))
        .await
        .map_err(|error| error.to_string())?
}
#[tauri::command]
async fn usage_summary(
    state: State<'_, DesktopState>,
    workspace_id: String,
    days: u64,
) -> Result<Value, String> {
    let workspaces = state.workspaces.clone();
    let days = days.clamp(1, 365);
    tauri::async_runtime::spawn_blocking(move || workspaces.usage_summary(&workspace_id, days))
        .await
        .map_err(|error| error.to_string())?
}
#[tauri::command]
async fn session_defaults(
    state: State<'_, DesktopState>,
    workspace_id: String,
) -> Result<Value, String> {
    let runtime = state.workspaces.resolve(&workspace_id, None, None)?;
    tauri::async_runtime::spawn_blocking(move || runtime.request("settings.defaults", None, None))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn session_events(
    state: State<'_, DesktopState>,
    workspace_id: String,
    session_id: String,
    cursor: Option<String>,
) -> Result<Value, String> {
    let runtime = state
        .workspaces
        .resolve(&workspace_id, None, Some(&session_id))?;
    tauri::async_runtime::spawn_blocking(move || {
        runtime
            .request("sessions.events", Some(&session_id), cursor.as_deref())
            .map(live::render_history)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn session_changes(
    state: State<'_, DesktopState>,
    workspace_id: String,
    session_id: String,
) -> Result<Value, String> {
    let runtime = state
        .workspaces
        .resolve(&workspace_id, None, Some(&session_id))?;
    tauri::async_runtime::spawn_blocking(move || {
        runtime.request("sessions.changes", Some(&session_id), None)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn live_open(
    app: AppHandle,
    state: State<'_, DesktopState>,
    workspace_id: String,
    key: String,
    session_id: Option<String>,
) -> Result<(), String> {
    state
        .workspaces
        .start(&workspace_id, Some(&key), session_id.as_deref())?
        .live
        .open(&app, &key, session_id.as_deref())
}
#[tauri::command]
fn live_request(
    state: State<'_, DesktopState>,
    workspace_id: String,
    key: String,
    method: String,
    params: Value,
) -> Result<u64, String> {
    state
        .workspaces
        .resolve(&workspace_id, Some(&key), None)?
        .live
        .request(&key, &method, params)
}
#[tauri::command]
fn live_focus(key: Option<String>) {
    sleep::focus(key.clone());
    if let Some(key) = key {
        live::wake(&key);
    }
}
#[tauri::command]
fn live_close(
    state: State<'_, DesktopState>,
    workspace_id: String,
    key: String,
) -> Result<(), String> {
    state
        .workspaces
        .resolve(&workspace_id, Some(&key), None)?
        .live
        .close(&key)
}
// Tauri receives workspace, chat, dimensions and the output channel independently.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn terminal_open(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
    key: String,
    cols: u16,
    rows: u16,
    output: Channel<Value>,
) -> Result<Value, String> {
    let runtime =
        state
            .workspaces
            .start(&workspace_id, chat_key.as_deref(), session_id.as_deref())?;
    let mut terminals = state.terminals.lock().map_err(|e| e.to_string())?;
    if terminals.len() >= 16 {
        return Err("Close an unused terminal before opening another".into());
    }
    let result = runtime.terminals.open(&key, cols, rows, move |frame| {
        let _ = output.send(frame);
    })?;
    terminals.insert(key, runtime);
    Ok(result)
}
fn terminal_runtime(state: &DesktopState, key: &str) -> Result<Arc<Runtime>, String> {
    state
        .terminals
        .lock()
        .map_err(|e| e.to_string())?
        .get(key)
        .cloned()
        .ok_or("Terminal is closed".into())
}
#[tauri::command]
async fn terminal_write(
    state: State<'_, DesktopState>,
    key: String,
    data: String,
) -> Result<(), String> {
    terminal_runtime(&state, &key)?.terminals.write(&key, &data)
}
#[tauri::command]
async fn terminal_resize(
    state: State<'_, DesktopState>,
    key: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    terminal_runtime(&state, &key)?
        .terminals
        .resize(&key, cols, rows)
}
#[tauri::command]
fn terminal_close(state: State<'_, DesktopState>, key: String) -> Result<(), String> {
    let runtime = state
        .terminals
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&key);
    if let Some(runtime) = runtime {
        runtime.terminals.close(&key)?;
    }
    Ok(())
}
#[tauri::command]
async fn files_list(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
    directory: String,
) -> Result<Value, String> {
    let runtime =
        state
            .workspaces
            .resolve(&workspace_id, chat_key.as_deref(), session_id.as_deref())?;
    tauri::async_runtime::spawn_blocking(move || files::list(&runtime.path, &directory))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn file_preview(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
    path: String,
) -> Result<Value, String> {
    let runtime =
        state
            .workspaces
            .resolve(&workspace_id, chat_key.as_deref(), session_id.as_deref())?;
    tauri::async_runtime::spawn_blocking(move || files::read(&runtime.path, &path))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
fn file_open(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
    path: String,
) -> Result<(), String> {
    files::open_default(
        &state
            .workspaces
            .resolve(&workspace_id, chat_key.as_deref(), session_id.as_deref())?
            .path,
        &path,
    )
}
#[tauri::command]
async fn git_status(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
) -> Result<Value, String> {
    let runtime =
        state
            .workspaces
            .resolve(&workspace_id, chat_key.as_deref(), session_id.as_deref())?;
    tauri::async_runtime::spawn_blocking(move || git::status(&runtime.path))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn git_diff(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
    path: String,
    staged: bool,
) -> Result<Value, String> {
    let runtime =
        state
            .workspaces
            .resolve(&workspace_id, chat_key.as_deref(), session_id.as_deref())?;
    tauri::async_runtime::spawn_blocking(move || git::diff(&runtime.path, &path, staged))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn extension_request(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
    method: String,
    params: Value,
) -> Result<Value, String> {
    if ![
        "extensions.list",
        "extensions.enable",
        "extensions.disable",
        "extensions.install",
        "extensions.skill.configure",
        "extensions.skill.install",
        "extensions.skill.remove",
        "extensions.skill.use",
        "extensions.skills.search",
        "extensions.skills.sources",
        "extensions.skills.sources.add",
        "extensions.skills.sources.remove",
        "extensions.skills.updates",
        "extensions.skills.lockfile",
        "extensions.skills.lock",
        "extensions.skills.sync",
        "extensions.remove",
        "extensions.rollback",
        "extensions.update.preview",
        "extensions.update.apply",
        "extensions.mcp.registry",
        "extensions.connectors",
        "extensions.hooks.list",
        "extensions.hooks.add",
        "extensions.hooks.remove",
        "extensions.marketplace.list",
        "extensions.marketplace.add",
        "extensions.marketplace.refresh",
        "extensions.marketplace.remove",
        "extensions.doctor",
    ]
    .contains(&method.as_str())
    {
        return Err("Unsupported extension action".into());
    }
    let runtime = state.workspaces.for_request(
        &workspace_id,
        chat_key.as_deref(),
        session_id.as_deref(),
        &method,
    )?;
    tauri::async_runtime::spawn_blocking(move || runtime.request_params(&method, params))
        .await
        .map_err(|error| error.to_string())?
}
#[tauri::command]
fn open_link(url: String) -> Result<(), String> {
    let parsed = tauri::Url::parse(&url).map_err(|e| e.to_string())?;
    if !matches!(parsed.scheme(), "http" | "https" | "mailto")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err("Unsupported link".into());
    }
    files::open_path(std::path::Path::new(&url))
}
#[tauri::command]
async fn settings_request(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
    method: String,
    params: Value,
) -> Result<Value, String> {
    if ![
        "settings.list",
        "settings.model.save",
        "settings.model.discover",
        "settings.model.context",
        "settings.model.default",
        "settings.model.remove",
        "settings.key.remove",
        "settings.search.save",
        "settings.health",
        "settings.health.fix",
        "settings.tools",
        "settings.tools.save",
        "settings.keys",
        "settings.keys.store",
        "settings.keys.set",
        "settings.keys.remove",
        "settings.mcp.save",
        "settings.mcp.update",
        "settings.mcp.signout",
        "settings.mcp.remove",
        "instructions.list",
        "instructions.save",
    ]
    .contains(&method.as_str())
    {
        return Err("Unsupported settings action".into());
    }
    let runtime = state.workspaces.for_request(
        &workspace_id,
        chat_key.as_deref(),
        session_id.as_deref(),
        &method,
    )?;
    tauri::async_runtime::spawn_blocking(move || runtime.request_params(&method, params))
        .await
        .map_err(|error| error.to_string())?
}
#[tauri::command]
async fn image_admit(request: tauri::ipc::Request<'_>) -> Result<Value, String> {
    let bytes = match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) if bytes.len() <= media::MAX_SOURCE_BYTES => {
            bytes.clone()
        }
        _ => return Err("Choose an image below 64 MB".into()),
    };
    tauri::async_runtime::spawn_blocking(move || {
        use base64::Engine;
        let image = media::normalize_for_transport(bytes, 2 * 1024 * 1024).map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "mime": image.mime, "data": base64::engine::general_purpose::STANDARD.encode(&image.bytes), "note": image.note }))
    }).await.map_err(|e| e.to_string())?
}
fn main() {
    let explicit = workspaces::explicit_workspace();
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(update::Updates::default())
        .setup(move |app| {
            app.manage(DesktopState {
                workspaces: Arc::new(
                    Workspaces::new(app.path().app_data_dir()?, explicit)
                        .map_err(std::io::Error::other)?,
                ),
                terminals: Mutex::new(HashMap::new()),
                views: Arc::default(),
            });
            Ok(())
        })
        .register_asynchronous_uri_scheme_protocol(view::SCHEME, outputs::serve)
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                window.state::<DesktopState>().workspaces.close_terminals();
            }
        })
        .invoke_handler(tauri::generate_handler![
            workspace_list,
            workspace_choose,
            workspace_info,
            workspace_reveal,
            workspace_branch,
            list_sessions,
            usage_summary,
            session_defaults,
            session_events,
            session_changes,
            live_open,
            live_request,
            live_focus,
            live_close,
            terminal_open,
            terminal_write,
            terminal_resize,
            terminal_close,
            files_list,
            file_preview,
            file_open,
            git_status,
            git_diff,
            extension_request,
            settings_request,
            open_link,
            image_admit,
            outputs::screen_put,
            outputs::screen_drop,
            outputs::screen_keep,
            outputs::screen_kept,
            outputs::media_link,
            outputs::file_reveal,
            outputs::output_save,
            update::update_check,
            update::update_apply
        ])
        .run(tauri::generate_context!())
        .expect("Medha desktop failed to open");
}
