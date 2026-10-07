//! Desktop executable discovery over the shared backend client.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

#[cfg(test)]
pub(crate) use medha_client::OPEN_ELSEWHERE;
pub(crate) use medha_client::{Connection, Hear, NOT_TAKING, Said, TOO_LARGE, WRAPPER, size};

pub(crate) struct Backend(Arc<medha_client::Backend>);

impl Backend {
    pub(crate) fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<Backend>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| {
            Arc::new(Self(medha_client::Backend::for_surface(
                executable,
                &["shared-live-state"],
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

pub fn executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("MEDHA_DESKTOP_BACKEND") {
        return Ok(PathBuf::from(path));
    }
    let name = if cfg!(windows) { "medha.exe" } else { "medha" };
    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
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
