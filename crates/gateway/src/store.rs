//! Small owner-only JSON files changed under a cross-process lock, so the running gateway
//! and a `medha pairing` command never lose each other's writes.

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The gateway's own folder in Medha's home.
pub fn home() -> Result<PathBuf> {
    Ok(runtime::config::medha_home()?.join("gateway"))
}

/// Held while a gateway runs; a second one is refused, since two would answer every message twice.
pub fn hold_running() -> Result<std::fs::File> {
    let path = home()?.join("running.lock");
    std::fs::create_dir_all(home()?)?;
    let file = private(&path, false)?;
    match file.try_lock() {
        Ok(()) => {
            // Read by `medha gateway stop` on Windows, which has no service manager to ask.
            std::fs::write(home()?.join("running.pid"), std::process::id().to_string())?;
            Ok(file)
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!("a Medha gateway is already running on this computer")
        }
        Err(std::fs::TryLockError::Error(error)) => Err(error).context("locking the gateway"),
    }
}

/// Whether a gateway runs now, by trying the lock it holds.
pub fn running() -> bool {
    let Ok(home) = home() else { return false };
    std::fs::File::open(home.join("running.lock"))
        .is_ok_and(|file| matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
}

/// The running gateway's process id; `None` when no gateway runs.
pub fn running_pid() -> Option<u32> {
    if !running() {
        return None;
    }
    std::fs::read_to_string(home().ok()?.join("running.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Reads the file, lets `change` edit it, and writes it back only when the value changed.
pub fn change<T, R>(path: &Path, change: impl FnOnce(&mut T) -> R) -> Result<R>
where
    T: Default + Serialize + DeserializeOwned,
{
    holding(path, || {
        let (mut value, before) = load::<T>(path)?;
        let result = change(&mut value);
        let after = serde_json::to_vec_pretty(&value)?;
        if after != before {
            let temporary = path.with_extension("tmp");
            let mut file = private(&temporary, true)?;
            file.write_all(&after)?;
            file.sync_all()?;
            drop(file);
            file_ops::replace(&temporary, path)
                .with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(result)
    })
}

/// Reads the file as it is now; a missing file is the empty value.
pub fn read<T>(path: &Path) -> Result<T>
where
    T: Default + DeserializeOwned,
{
    holding(path, || Ok(load::<T>(path)?.0))
}

fn holding<R>(path: &Path, operation: impl FnOnce() -> Result<R>) -> Result<R> {
    let folder = path.parent().context("a store file has a folder")?;
    std::fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
    let mut lock = fd_lock::RwLock::new(private(&path.with_extension("lock"), false)?);
    let _held = lock.write().context("locking the gateway store")?;
    operation()
}

fn load<T: Default + DeserializeOwned>(path: &Path) -> Result<(T, Vec<u8>)> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let value = if bytes.is_empty() {
        T::default()
    } else {
        serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))?
    };
    Ok((value, bytes))
}

fn private(path: &Path, truncate: bool) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}
