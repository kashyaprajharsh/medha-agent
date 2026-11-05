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

/// What always starts the installed version, even after an update: the link on PATH on Unix,
/// the `bin` junction's copy on Windows. `None` for a copy no installer put in place.
pub(crate) fn installed_command() -> Option<PathBuf> {
    let install = Install::current()?;
    match cfg!(windows) {
        true => Some(install.root.join("bin").join("medha.exe")),
        false => Some(install.install_dir?.join("medha")),
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

/// Checks are off in debug builds, by `MEDHA_NO_UPDATE_CHECK` set to anything but `0`, `false` or nothing,
/// and by `[updates] check = false`.
fn enabled() -> bool {
    let switched_off = std::env::var("MEDHA_NO_UPDATE_CHECK").is_ok_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false"
        )
    });
    !cfg!(debug_assertions)
        && !switched_off
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

/// Where releases are downloaded first; a fork named by `MEDHA_REPO` has no mirror and uses GitHub alone.
fn mirror() -> Option<&'static str> {
    std::env::var_os("MEDHA_REPO")
        .is_none()
        .then_some("https://download.manojavamlabs.com")
}

/// The public half of the update key, the same one built into the desktop app.
const UPDATE_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IENBQTNGNTUxOTIwOTU4MzgKUldRNFdBbVNVZldqeXNwVHNKSURMSGNDaUp2YmlucUI4R1pUS2NqREVzZ0lKcUFGRjJPS0RSL3UK";

/// A client whose connect limit is shared across a name's addresses, so one that never answers costs
/// a few seconds before the next is tried.
fn client(connect: Duration, total: Duration) -> Result<reqwest::Client> {
    Ok(client_builder(connect, total).build()?)
}

fn client_builder(connect: Duration, total: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent("medha")
        .connect_timeout(connect)
        .timeout(total)
}

async fn body(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("could not reach {url}"))?
        .error_for_status()
        .with_context(|| format!("could not download {url}"))?;
    Ok(response.bytes().await?.to_vec())
}

/// The latest release tag: the mirror's manifest first, then GitHub's.
pub(crate) async fn latest() -> Result<String> {
    let client = client(Duration::from_secs(10), Duration::from_secs(15))?;
    let mirrored = match mirror() {
        Some(base) => latest_mirrored(&client, base).await.map_err(Some),
        None => Err(None),
    };
    let tag = match mirrored {
        Ok(tag) => tag,
        Err(error) => {
            if let Some(error) = error {
                tracing::debug!("the download mirror gave no release: {error:#}");
            }
            latest_on_github(&client).await?
        }
    };
    parse(&tag)
        .map(|_| tag.clone())
        .with_context(|| format!("the latest release tag {tag} is not a version"))
}

async fn latest_mirrored(client: &reqwest::Client, base: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Manifest {
        version: String,
    }
    let manifest: Manifest =
        serde_json::from_slice(&body(client, &format!("{base}/latest/latest.json")).await?)?;
    Ok(format!("v{}", manifest.version.trim_start_matches('v')))
}

async fn latest_on_github(client: &reqwest::Client) -> Result<String> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
    }
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo());
    let release: Release = serde_json::from_slice(&body(client, &url).await?)?;
    Ok(release.tag_name)
}

/// Where release `tag`'s file `name` can be downloaded, the mirror first.
fn sources(tag: &str, name: &str) -> Vec<String> {
    let mut sources: Vec<String> = mirror()
        .map(|base| format!("{base}/{tag}/{name}"))
        .into_iter()
        .collect();
    sources.push(format!(
        "https://github.com/{}/releases/download/{tag}/{name}",
        repo()
    ));
    sources
}

/// Release `tag`'s installer and its signature, from the first source that serves both.
async fn download_installer(
    client: &reqwest::Client,
    sources: &[String],
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut failures = Vec::new();
    for url in sources {
        let fetched = async {
            let script = body(client, url).await?;
            let signature = body(client, &format!("{url}.minisig")).await?;
            anyhow::Ok((script, signature))
        };
        match fetched.await {
            Ok(fetched) => return Ok(fetched),
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    bail!(
        "the installer could not be downloaded: {}",
        failures.join("; ")
    )
}

/// `script` runs only if `key` signed exactly it, as release `tag`'s `name`. Read after `verify`, the
/// signed comment names the file and version, so an older genuine script cannot pass for a newer one.
fn verify(script: &[u8], signature: &[u8], key: &str, name: &str, tag: &str) -> Result<()> {
    use base64::Engine as _;
    let text = |encoded: &[u8]| -> Result<String> {
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim_ascii())?;
        Ok(String::from_utf8(decoded)?)
    };
    let key = minisign_verify::PublicKey::decode(&text(key.as_bytes())?)
        .context("the update key is not readable")?;
    let signature = minisign_verify::Signature::decode(&text(signature)?)
        .context("the installer's signature is not readable")?;
    key.verify(script, &signature, false)
        .context("the installer's signature does not match Medha's update key")?;
    let signed: Vec<&str> = signature.trusted_comment().split('\t').collect();
    let version = format!("version:{}", tag.trim_start_matches('v'));
    if !signed.contains(&format!("file:{name}").as_str()) || !signed.contains(&version.as_str()) {
        bail!("the installer was signed as another file or release, not {name} of {tag}");
    }
    Ok(())
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
        .env_remove("MEDHA_INSTALL_DIR")
        .env("MEDHA_STORE", &install.root);
    if let Some(dir) = &install.install_dir {
        command.env("MEDHA_INSTALL_DIR", dir);
    }
    command
}

/// Downloads release `tag`'s installer, checks its signature, and runs it in this terminal.
pub(crate) async fn install(tag: &str, install: &Install) -> Result<()> {
    let name = if cfg!(windows) {
        "install.ps1"
    } else {
        "install.sh"
    };
    let client = client(Duration::from_secs(20), Duration::from_secs(60))?;
    let (script, signature) = download_installer(&client, &sources(tag, name)).await?;
    // A bad signature stops the update; it is never a reason to try another source.
    verify(&script, &signature, UPDATE_KEY, name, tag)?;
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
    // A messaging gateway left running would keep answering with the old version.
    crate::gateway_service::restart_if_installed();
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
