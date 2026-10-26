//! Runs `medha gateway run` at login and again after a crash: a launchd agent on macOS, a systemd
//! user unit on Linux, a start-up entry for `medhaw` on Windows. Each starts the installed
//! `medha`, so an update is picked up.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

#[cfg(target_os = "macos")]
#[path = "gateway_service/launchd.rs"]
mod system;
#[cfg(target_os = "linux")]
#[path = "gateway_service/systemd.rs"]
mod system;
#[cfg(windows)]
#[path = "gateway_service/windows.rs"]
mod system;
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
#[path = "gateway_service/unsupported.rs"]
mod system;

pub(crate) fn install() -> Result<()> {
    let place = system::register(&program()?)?;
    println!("Installed: the gateway starts at login and restarts if it crashes.\n{place}");
    Ok(())
}

pub(crate) fn uninstall() -> Result<()> {
    if system::registered().is_none() {
        println!("The gateway service is not installed.");
        return Ok(());
    }
    system::unregister()?;
    println!("Removed the gateway service.");
    Ok(())
}

pub(crate) fn start() -> Result<()> {
    installed()?;
    system::start()
}

pub(crate) fn stop() -> Result<()> {
    installed()?;
    system::stop()
}

pub(crate) fn restart() -> Result<()> {
    installed()?;
    system::restart()
}

/// After an update or a new setup, so the service runs on them; quiet when there is no service.
pub(crate) fn restart_if_installed() {
    if system::registered().is_some()
        && let Err(error) = system::restart()
    {
        eprintln!("The messaging gateway could not be restarted: {error:#}");
    }
}

pub(crate) fn describe() -> Option<String> {
    system::registered().map(|place| format!("installed ({place})"))
}

fn installed() -> Result<()> {
    if system::registered().is_none() {
        bail!("The gateway service is not installed. Install it: medha gateway install");
    }
    Ok(())
}

/// The installed link when there is one; a copy built or placed by hand runs as itself.
fn program() -> Result<PathBuf> {
    match crate::self_update::installed_command().filter(|path| path.exists()) {
        Some(path) => Ok(path),
        None => std::env::current_exe().context("finding medha"),
    }
}

/// A home other than the default, which the service must be told about.
fn medha_home() -> Option<String> {
    std::env::var("MEDHA_HOME")
        .ok()
        .filter(|home| !home.is_empty())
}

#[cfg_attr(windows, allow(dead_code))]
fn run(program: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))
}

#[cfg_attr(windows, allow(dead_code))]
fn checked(program: &str, args: &[&str]) -> Result<()> {
    let output = run(program, args)?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
