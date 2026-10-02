//! Commands behind the things a chat makes: showing, saving and revealing them.

use crate::{DesktopState, files, kept, view};
use serde_json::{Value, json};
use std::path::PathBuf;
use tauri::{Manager, State, UriSchemeContext, UriSchemeResponder, http};

fn within(
    state: &State<'_, DesktopState>,
    workspace_id: &str,
    chat_key: Option<&str>,
    session_id: Option<&str>,
    path: &str,
) -> Result<PathBuf, String> {
    let runtime = state
        .workspaces
        .resolve(workspace_id, chat_key, session_id)?;
    files::resolve(&runtime.path, path)
}

#[tauri::command]
pub fn screen_put(
    state: State<'_, DesktopState>,
    html: String,
    run: bool,
    online: bool,
    app: Option<view::Origins>,
) -> Result<String, String> {
    // The window passes `online` only for a page the person allowed, one page at a
    // time, and `app` only for a connected server's screen, with what it declared.
    let reach = match (app, online, run) {
        (Some(origins), ..) => view::Reach::App(origins),
        (None, true, _) => view::Reach::Online,
        (None, false, true) => view::Reach::Running,
        (None, false, false) => view::Reach::Still,
    };
    state.views.put(html, reach)
}

fn shelf(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    Ok(data.join("screens"))
}

/// Remembers the page a server sent for its screen.
#[tauri::command]
pub async fn screen_keep(
    app: tauri::AppHandle,
    server: String,
    uri: String,
    page: Value,
) -> Result<(), String> {
    let dir = shelf(&app)?;
    tauri::async_runtime::spawn_blocking(move || kept::keep(&dir, &server, &uri, &page))
        .await
        .map_err(|error| error.to_string())?
}

/// The page last kept for a screen, for when its server cannot be asked.
#[tauri::command]
pub async fn screen_kept(
    app: tauri::AppHandle,
    server: String,
    uri: String,
) -> Result<Option<Value>, String> {
    let dir = shelf(&app)?;
    tauri::async_runtime::spawn_blocking(move || kept::kept(&dir, &server, &uri))
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn screen_drop(state: State<'_, DesktopState>, url: String) {
    state.views.drop_screen(&url);
}

#[tauri::command]
pub fn media_link(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
    path: String,
) -> Result<Value, String> {
    let path = within(
        &state,
        &workspace_id,
        chat_key.as_deref(),
        session_id.as_deref(),
        &path,
    )?;
    let size = std::fs::metadata(&path)
        .map_err(|error| error.to_string())?
        .len();
    Ok(json!({ "url": state.views.link(path)?, "size": size }))
}

#[tauri::command]
pub fn file_reveal(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
    path: String,
) -> Result<(), String> {
    let path = within(
        &state,
        &workspace_id,
        chat_key.as_deref(),
        session_id.as_deref(),
        &path,
    )?;
    #[cfg(target_os = "macos")]
    let shown = std::process::Command::new("open")
        .arg("-R")
        .arg(&path)
        .spawn();
    #[cfg(target_os = "windows")]
    let shown = std::process::Command::new("explorer")
        .arg("/select,")
        .arg(&path)
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let shown = std::process::Command::new("xdg-open")
        .arg(path.parent().unwrap_or(&path))
        .spawn();
    shown.map(|_| ()).map_err(|error| error.to_string())
}

/// Saves an output where the person chooses: its bytes, or a copy of its file. `false` when they cancel.
#[tauri::command]
pub async fn output_save(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
    name: String,
    data: Option<Vec<u8>>,
    path: Option<String>,
) -> Result<bool, String> {
    let from = match &path {
        Some(path) => Some(within(
            &state,
            &workspace_id,
            chat_key.as_deref(),
            session_id.as_deref(),
            path,
        )?),
        None => None,
    };
    let name = name.replace(['/', '\\', ':'], "-");
    let Some(target) = rfd::AsyncFileDialog::new()
        .set_title("Save")
        .set_file_name(name)
        .save_file()
        .await
    else {
        return Ok(false);
    };
    let target = target.path().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || match (from, data) {
        (Some(from), _) => std::fs::copy(from, target).map(|_| ()),
        (None, Some(data)) => std::fs::write(target, data),
        (None, None) => Ok(()),
    })
    .await
    .map_err(|error| error.to_string())?
    .map(|()| true)
    .map_err(|error| error.to_string())
}

pub fn serve<R: tauri::Runtime>(
    context: UriSchemeContext<'_, R>,
    request: http::Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let views = context.app_handle().state::<DesktopState>().views.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let reply = if request.method() == http::Method::GET {
            let range = request
                .headers()
                .get(http::header::RANGE)
                .and_then(|value| value.to_str().ok());
            views.respond(request.uri().path(), range)
        } else {
            view::Reply {
                status: 405,
                headers: Vec::new(),
                body: Vec::new(),
            }
        };
        let mut response = http::Response::builder().status(reply.status);
        for (name, value) in reply.headers {
            response = response.header(name, value);
        }
        if let Ok(response) = response.body(reply.body) {
            responder.respond(response);
        }
    });
}
