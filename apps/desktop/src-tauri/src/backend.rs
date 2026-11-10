//! Desktop executable discovery over the shared backend client.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

pub(crate) use medha_client::{Connection, Hear, NOT_TAKING, Said, TOO_LARGE, WRAPPER, size};

pub(crate) struct Backend(Arc<medha_client::Backend>);

impl Backend {
    pub(crate) fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<Backend>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| {
            Arc::new(Self(medha_client::Backend::for_surface(
                executable,
                &[
                    "shared-live-state",
                    "workspace-images",
                    "model-providers",
                    "artifact-revisions",
                ],
            )))
        }))
    }

    pub(crate) fn connection(&self) -> Result<Arc<Connection>, String> {
        self.0.connection()
    }

    #[cfg(test)]
    pub(crate) fn owned(executable: PathBuf, env: Vec<(String, String)>) -> Arc<Self> {
        Arc::new(Self(medha_client::Backend::owned(executable, env)))
    }

    #[cfg(test)]
    pub(crate) fn stop(&self) {
        self.0.stop();
    }

    #[cfg(test)]
    pub(crate) fn joined_to(address: String, token: &str) -> Arc<Self> {
        Arc::new(Self(medha_client::Backend::joined_to(address, token)))
    }
}

/// Tells the window whenever the backend says a folder's chats changed, from here or from the terminal.
pub(crate) fn follow_chat_lists(app: tauri::AppHandle) {
    use tauri::Emitter;
    tauri::async_runtime::spawn(async move {
        let mut retry = false;
        loop {
            let reached = tauri::async_runtime::spawn_blocking(move || {
                if retry {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
                Backend::shared().connection()
            })
            .await;
            retry = true;
            let Ok(Ok(connection)) = reached else {
                continue;
            };
            let (mut changed, mut closed) = (connection.chats_changed(), connection.closed());
            drop(connection);
            loop {
                tokio::select! {
                    told = changed.changed() => if told.is_err() { break },
                    gone = closed.wait_for(|closed| *closed) => { let _ = gone; break }
                }
                let _ = app.emit("medha-chats-changed", ());
            }
        }
    });
}

pub fn executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("MEDHA_DESKTOP_BACKEND") {
        return Ok(PathBuf::from(path));
    }
    let name = if cfg!(windows) { "medha.exe" } else { "medha" };
    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|path| dunce::canonicalize(path).ok())
        .and_then(|path| path.parent().map(|parent| parent.join(name)))
        .filter(|path| path.is_file())
    {
        return Ok(path);
    }
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let binary = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join(format!(
            "medha-{}{}",
            env!("TAURI_ENV_TARGET_TRIPLE"),
            suffix
        ));
    binary
        .is_file()
        .then_some(binary)
        .ok_or_else(|| "Medha backend is missing. Run npm run prepare:backend.".into())
}

#[cfg(test)]
pub(crate) use medha_client::test_support as tests;
