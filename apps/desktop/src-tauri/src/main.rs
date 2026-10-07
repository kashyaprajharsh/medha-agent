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
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tauri::{AppHandle, Manager, State, ipc::Channel};
use workspaces::{Runtime, Workspaces};

struct DesktopState {
    workspaces: Arc<Workspaces>,
    terminals: Arc<TerminalTickets>,
    views: Arc<view::Views>,
}
struct TerminalTicket {
    runtime: Option<Arc<Runtime>>,
    generation: Arc<()>,
}
#[derive(Default)]
struct TerminalTickets {
    open: Mutex<HashMap<String, TerminalTicket>>,
    closed: AtomicBool,
    opening: AtomicUsize,
    quitting: AtomicBool,
}
impl TerminalTickets {
    fn close_all(&self) {
        self.closed.store(true, Ordering::Release);
        let tickets = std::mem::take(
            &mut *self
                .open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (key, ticket) in tickets {
            if let Some(runtime) = ticket.runtime {
                let _ = runtime.terminals.close_generation(&key, &ticket.generation);
            }
        }
    }

    fn wait_until_reaped(&self) {
        // Kept off the app thread. The process must not exit while a native
        // spawn can still return a child, or before cleanup has reaped it.
        while self.opening.load(Ordering::Acquire) != 0 || !terminal::all_reaped() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

pub(crate) fn reap_terminals(app: &AppHandle) {
    if let Some(state) = app.try_state::<DesktopState>() {
        state.terminals.close_all();
        state.workspaces.close_all();
        state.terminals.wait_until_reaped();
    }
}
impl Drop for DesktopState {
    fn drop(&mut self) {
        self.terminals.close_all();
    }
}
// A failed or cancelled opening must retire only its own incarnation. In
// particular, a late native spawn cannot claim a key reused by a new tab.
struct TerminalOpening {
    tickets: Arc<TerminalTickets>,
    key: String,
    generation: Arc<()>,
    runtime: Option<Arc<Runtime>>,
    keep: bool,
}
impl Drop for TerminalOpening {
    fn drop(&mut self) {
        if !self.keep {
            let mut tickets = self
                .tickets
                .open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if tickets
                .get(&self.key)
                .is_some_and(|ticket| Arc::ptr_eq(&ticket.generation, &self.generation))
            {
                tickets.remove(&self.key);
            }
            drop(tickets);
            if let Some(runtime) = &self.runtime {
                let _ = runtime
                    .terminals
                    .close_generation(&self.key, &self.generation);
            }
        }
        self.tickets.opening.fetch_sub(1, Ordering::Release);
    }
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
    let generation = Arc::new(());
    {
        let mut terminals = state.terminals.open.lock().map_err(|e| e.to_string())?;
        if state.terminals.closed.load(Ordering::Acquire) {
            return Err("The window is closing".into());
        }
        if terminals.len() >= 16 {
            return Err("Close an unused terminal before opening another".into());
        }
        if terminals.contains_key(&key) {
            return Err("This terminal is already open".into());
        }
        terminals.insert(
            key.clone(),
            TerminalTicket {
                runtime: None,
                generation: Arc::clone(&generation),
            },
        );
        state.terminals.opening.fetch_add(1, Ordering::Release);
    }
    let opening = TerminalOpening {
        tickets: Arc::clone(&state.terminals),
        key,
        generation,
        runtime: None,
        keep: false,
    };
    let workspaces = Arc::clone(&state.workspaces);
    tauri::async_runtime::spawn_blocking(move || {
        let mut opening = opening;
        let runtime =
            workspaces.start(&workspace_id, chat_key.as_deref(), session_id.as_deref())?;
        opening.runtime = Some(Arc::clone(&runtime));
        {
            let mut tickets = opening.tickets.open.lock().map_err(|e| e.to_string())?;
            let ticket = tickets
                .get_mut(&opening.key)
                .filter(|ticket| Arc::ptr_eq(&ticket.generation, &opening.generation))
                .ok_or("This terminal was closed while its shell was starting")?;
            ticket.runtime = Some(Arc::clone(&runtime));
        }
        let result = runtime.terminals.open(
            &opening.key,
            cols,
            rows,
            Arc::clone(&opening.generation),
            move |frame| {
                let _ = output.send(frame);
            },
        )?;
        let tickets = opening.tickets.open.lock().map_err(|e| e.to_string())?;
        if !tickets
            .get(&opening.key)
            .is_some_and(|ticket| Arc::ptr_eq(&ticket.generation, &opening.generation))
        {
            return Err("This terminal was closed while its shell was starting".into());
        }
        opening.keep = true;
        Ok(result)
    })
    .await
    .map_err(|error| error.to_string())?
}
fn terminal_runtime(state: &DesktopState, key: &str) -> Result<Arc<Runtime>, String> {
    state
        .terminals
        .open
        .lock()
        .map_err(|e| e.to_string())?
        .get(key)
        .and_then(|ticket| ticket.runtime.clone())
        .ok_or("Terminal is closed".into())
}
#[tauri::command]
async fn terminal_write(
    state: State<'_, DesktopState>,
    key: String,
    data: String,
) -> Result<(), String> {
    let runtime = terminal_runtime(&state, &key)?;
    tauri::async_runtime::spawn_blocking(move || runtime.terminals.write(&key, &data))
        .await
        .map_err(|error| error.to_string())?
}
#[tauri::command]
async fn terminal_resize(
    state: State<'_, DesktopState>,
    key: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let runtime = terminal_runtime(&state, &key)?;
    tauri::async_runtime::spawn_blocking(move || runtime.terminals.resize(&key, cols, rows))
        .await
        .map_err(|error| error.to_string())?
}
#[tauri::command]
fn terminal_close(state: State<'_, DesktopState>, key: String) -> Result<(), String> {
    let runtime = state
        .terminals
        .open
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&key);
    if let Some(ticket) = runtime
        && let Some(runtime) = ticket.runtime
    {
        runtime
            .terminals
            .close_generation(&key, &ticket.generation)?;
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
                terminals: Arc::default(),
                views: Arc::default(),
            });
            Ok(())
        })
        .register_asynchronous_uri_scheme_protocol(view::SCHEME, outputs::serve)
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                window.state::<DesktopState>().terminals.close_all();
                window.state::<DesktopState>().workspaces.close_all();
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
        .build(tauri::generate_context!())
        .expect("Medha desktop failed to open")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event
                && let Some(state) = app.try_state::<DesktopState>()
            {
                state.terminals.close_all();
                state.workspaces.close_all();
                if state.terminals.opening.load(Ordering::Acquire) != 0 || !terminal::all_reaped() {
                    api.prevent_exit();
                    if !state.terminals.quitting.swap(true, Ordering::AcqRel) {
                        let app = app.clone();
                        std::thread::spawn(move || {
                            reap_terminals(&app);
                            app.exit(code.unwrap_or(0));
                        });
                    }
                }
            }
        });
}

#[cfg(test)]
mod terminal_ticket_tests {
    use super::*;

    #[test]
    fn closing_a_window_still_tracks_a_pending_or_committed_opening_until_it_returns() {
        for keep in [false, true] {
            let tickets = Arc::new(TerminalTickets::default());
            let generation = Arc::new(());
            tickets.open.lock().unwrap().insert(
                "pending".into(),
                TerminalTicket {
                    runtime: None,
                    generation: Arc::clone(&generation),
                },
            );
            tickets.opening.fetch_add(1, Ordering::Release);
            let opening = TerminalOpening {
                tickets: Arc::clone(&tickets),
                key: "pending".into(),
                generation,
                runtime: None,
                keep,
            };
            tickets.close_all();
            assert!(tickets.open.lock().unwrap().is_empty());
            assert_eq!(tickets.opening.load(Ordering::Acquire), 1);
            drop(opening);
            assert_eq!(tickets.opening.load(Ordering::Acquire), 0);
        }
    }
}
