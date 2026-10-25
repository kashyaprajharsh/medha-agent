//! Medha's own updates: a background check for a newer release, and the installer run for the one offered.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const CURRENT: &str = env!("CARGO_PKG_VERSION");
const CHECK_EVERY: Duration = Duration::from_secs(20 * 60 * 60);
const CACHE_FILE: &str = "update-check.json";

/// The release the TUI was told to install once it has closed.
static REQUESTED: Mutex<Option<String>> = Mutex::new(None);

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    latest: String,
    checked_at: u64,
    #[serde(default)]
    skipped: Option<String>,
}

/// A copy an installer put in its `versions/<tag>/` folder; any other copy cannot update itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Install {
    /// The folder that holds `versions`: the Windows install folder, or the Unix version store.
    root: PathBuf,
    /// Where `medha` is found: on Windows the install folder, on Unix the folder of the link on PATH.
    install_dir: Option<PathBuf>,
}

impl Install {
    pub(crate) fn current() -> Option<Self> {
        let canonical = std::env::current_exe().ok()?.canonicalize().ok()?;
        // Without Windows' `\\?\` prefix, which the installer would otherwise write into PATH.
        let exe = file_ops::shown(&canonical).to_path_buf();
        let mut install = Self::from_exe(&exe)?;
        if !cfg!(windows) {
            install.install_dir = link_on_path(&exe);
        }
        Some(install)
    }

    fn from_exe(exe: &Path) -> Option<Self> {
        let release = exe.parent()?;
        let versions = release.parent()?;
        let tag = release.file_name()?.to_str()?;
        if versions.file_name()? != "versions" || parse(tag).is_none() {
            return None;
        }
        let root = versions.parent()?.to_path_buf();
        Some(Self {
            install_dir: cfg!(windows).then(|| root.clone()),
            root,
        })
    }

    fn executable(&self, tag: &str) -> PathBuf {
        let name = if cfg!(windows) { "medha.exe" } else { "medha" };
        self.root.join("versions").join(tag).join(name)
    }
}

/// The folder on PATH whose `medha` link points at this copy, so an update lands where the user installed.
fn link_on_path(exe: &Path) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .find(|dir| std::fs::canonicalize(dir.join("medha")).is_ok_and(|target| target == exe))
}

/// `major.minor.patch`, with or without a leading `v`; a prerelease or anything else is not a release.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.strip_prefix('v').unwrap_or(version).split('.');
    let release = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(release)
}

/// A release tag as people read a version: `v0.1.13` is shown as `0.1.13`, like the running one.
pub(crate) fn shown(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

pub(crate) fn newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(latest), Some(current)) if latest > current)
}

fn repo() -> String {
    std::env::var("MEDHA_REPO").unwrap_or_else(|_| "kashyaprajharsh/medha-agent".into())
}

/// Checks are off in debug builds, by `MEDHA_NO_UPDATE_CHECK`, and by `[updates] check = false`.
fn enabled() -> bool {
    !cfg!(debug_assertions)
        && std::env::var_os("MEDHA_NO_UPDATE_CHECK").is_none()
        && runtime::config::load()
            .ok()
            .flatten()
            .is_none_or(|config| config.updates.check)
}

fn cache_path() -> Option<PathBuf> {
    runtime::config::medha_home()
        .ok()
        .map(|home| home.join(CACHE_FILE))
}

fn read_cache(path: &Path) -> Option<Cache> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_cache(path: &Path, cache: &Cache) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec(cache)?)?;
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The newer release to offer this copy, if one is known and was not skipped.
fn offered(cache: &Cache, current: &str) -> Option<String> {
    (newer(&cache.latest, current) && cache.skipped.as_deref() != Some(cache.latest.as_str()))
        .then(|| cache.latest.clone())
}

/// What the TUI offers at start, from the last check; a stale check is refreshed in the background for next time.
pub(crate) fn offer() -> Option<String> {
    if !enabled() || Install::current().is_none() {
        return None;
    }
    let path = cache_path()?;
    let cache = read_cache(&path);
    let stale = cache
        .as_ref()
        .is_none_or(|cache| now().saturating_sub(cache.checked_at) >= CHECK_EVERY.as_secs());
    if stale {
        let path = path.clone();
        tokio::spawn(async move {
            if let Err(error) = refresh(&path).await {
                tracing::debug!("update check failed: {error:#}");
            }
        });
    }
    offered(&cache?, CURRENT)
}

/// The latest release tag, asked of GitHub now.
pub(crate) async fn latest() -> Result<String> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
    }
    let release: Release = reqwest::Client::builder()
        .user_agent("medha")
        .timeout(Duration::from_secs(15))
        .build()?
        .get(format!(
            "https://api.github.com/repos/{}/releases/latest",
            repo()
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    parse(&release.tag_name)
        .map(|_| release.tag_name.clone())
        .with_context(|| {
            format!(
                "the latest release tag {} is not a version",
                release.tag_name
            )
        })
}

async fn refresh(path: &Path) -> Result<String> {
    let latest = latest().await?;
    let skipped = read_cache(path).and_then(|cache| cache.skipped);
    write_cache(
        path,
        &Cache {
            latest: latest.clone(),
            checked_at: now(),
            skipped,
        },
    )?;
    Ok(latest)
}

pub(crate) fn skip(version: &str) {
    let Some(path) = cache_path() else {
        return;
    };
    let mut cache = read_cache(&path).unwrap_or_default();
    cache.skipped = Some(version.to_owned());
    if let Err(error) = write_cache(&path, &cache) {
        tracing::debug!("could not remember the skipped update: {error:#}");
    }
}

pub(crate) fn request(version: &str) {
    *REQUESTED
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(version.to_owned());
}

pub(crate) fn requested() -> Option<String> {
    REQUESTED
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take()
}

/// The installer of release `tag`, pinned to that tag and to the folder this copy was installed to.
fn installer(script: &Path, tag: &str, install: &Install) -> std::process::Command {
    let mut command = if cfg!(windows) {
        let system = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        // An absolute system path: nothing in the project or on PATH can stand in for PowerShell.
        let mut command = std::process::Command::new(
            PathBuf::from(system).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
        );
        command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
        command.arg(script);
        command
    } else {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg(script);
        command
    };
    command
        .env("MEDHA_VERSION", tag)
        .env_remove("MEDHA_INSTALL_DIR");
    if let Some(dir) = &install.install_dir {
        command.env("MEDHA_INSTALL_DIR", dir);
    }
    command
}

/// Downloads release `tag`'s installer and runs it in this terminal.
pub(crate) async fn install(tag: &str, install: &Install) -> Result<()> {
    let name = if cfg!(windows) {
        "install.ps1"
    } else {
        "install.sh"
    };
    let url = format!(
        "https://github.com/{}/releases/download/{tag}/{name}",
        repo()
    );
    let script = reqwest::Client::builder()
        .user_agent("medha")
        .timeout(Duration::from_secs(60))
        .build()?
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("could not download {url}"))?
        .bytes()
        .await?;
    // A folder of its own: a file in the project cannot be picked up by the installer.
    let folder = tempfile::tempdir()?;
    let path = folder.path().join(name);
    std::fs::write(&path, &script)?;
    let status = installer(&path, tag, install)
        .current_dir(folder.path())
        .status()
        .context("could not start the installer")?;
    if !status.success() {
        bail!("the installer stopped with {status}");
    }
    Ok(())
}

/// The start card's Update now, once the TUI has closed: installs `tag`, then starts it in this terminal.
pub(crate) async fn update_and_restart(tag: &str) {
    let Some(install) = Install::current() else {
        return;
    };
    println!("Updating medha to {}…", shown(tag));
    if let Err(error) = self::install(tag, &install).await {
        eprintln!("The update did not finish: {error:#}");
        return;
    }
    let error = relaunch(&install, tag);
    eprintln!(
        "medha {} is installed, but it could not start ({error}). Start medha again to use it.",
        shown(tag)
    );
}

/// Starts the installed `tag` in this terminal with this run's arguments; returns only when it could not.
fn relaunch(install: &Install, tag: &str) -> std::io::Error {
    let mut command = std::process::Command::new(install.executable(tag));
    command.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.exec()
    }
    #[cfg(not(unix))]
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => error,
    }
}

#[cfg(test)]
#[path = "self_update_tests.rs"]
mod tests;
