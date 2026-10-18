//! Commands behind the things a chat makes: showing, saving and revealing them.

use crate::{DesktopState, artifacts, files, kept, view};
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
    bundle: Option<String>,
) -> Result<String, String> {
    // The window passes `online` only for a page the person allowed, one page at a
    // time, and `app` only for a connected server's screen, with what it declared.
    let reach = match (app, online, run) {
        (Some(origins), ..) => view::Reach::App(origins),
        (None, true, _) => view::Reach::Online,
        (None, false, true) => view::Reach::Running,
        (None, false, false) => view::Reach::Still,
    };
    match bundle {
        Some(bundle) => state.views.put_bundle(html, reach, Some(&bundle)),
        None => state.views.put(html, reach),
    }
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
pub fn artifact_release(state: State<'_, DesktopState>, bundle: String) {
    state.views.drop_bundle(&bundle);
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
    file_ops::opening::reveal(&path)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveOutput {
    name: String,
    content: SaveContent,
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SaveContent {
    Bytes { data: Vec<u8> },
    File { path: String },
    Revision { revision_id: String },
}

/// Saves one explicit source where the person chooses. `false` when they cancel.
#[tauri::command]
pub async fn output_save(
    state: State<'_, DesktopState>,
    workspace_id: String,
    chat_key: Option<String>,
    session_id: Option<String>,
    request: SaveOutput,
) -> Result<bool, String> {
    enum Source {
        Bytes(Vec<u8>),
        File(PathBuf),
    }
    let source = match request.content {
        SaveContent::Revision {
            revision_id: revision,
        } => {
            let session = session_id
                .as_deref()
                .ok_or("The artifact has no conversation.")?;
            let runtime = state
                .workspaces
                .resolve(&workspace_id, None, Some(session))?;
            let session = session.to_owned();
            let data = tauri::async_runtime::spawn_blocking(move || {
                let manifest = artifacts::manifest(&runtime, &session, &revision)?;
                artifacts::bytes(
                    &runtime,
                    &session,
                    &revision,
                    None,
                    manifest.size,
                    &manifest.hash,
                )
            })
            .await
            .map_err(|error| error.to_string())??;
            Source::Bytes(data)
        }
        SaveContent::Bytes { data } => Source::Bytes(data),
        SaveContent::File { path } => Source::File(within(
            &state,
            &workspace_id,
            chat_key.as_deref(),
            session_id.as_deref(),
            &path,
        )?),
    };
    let name = request.name.replace(['/', '\\', ':'], "-");
    let Some(target) = rfd::AsyncFileDialog::new()
        .set_title("Save")
        .set_file_name(name)
        .save_file()
        .await
    else {
        return Ok(false);
    };
    let target = target.path().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || match source {
        Source::File(from) => std::fs::copy(from, target).map(|_| ()),
        Source::Bytes(data) => std::fs::write(target, data),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_historical_export_cannot_silently_choose_a_current_file_instead() {
        let saved = json!({"name":"old.html","content":{"kind":"revision","revision_id":"old"}});
        assert!(serde_json::from_value::<SaveOutput>(saved.clone()).is_ok());
        let mut conflicting = saved;
        conflicting["content"]["path"] = json!("current.html");
        assert!(serde_json::from_value::<SaveOutput>(conflicting).is_err());
        for content in [
            json!({}),
            json!({"kind":"bytes","data":[1],"revision_id":"old"}),
        ] {
            assert!(
                serde_json::from_value::<SaveOutput>(json!({"name":"old.html","content":content}))
                    .is_err()
            );
        }
    }
}
