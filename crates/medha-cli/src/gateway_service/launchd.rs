//! A launchd agent in the user's login session, where the keychain and the network are theirs.

use super::{checked, medha_home, run};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const LABEL: &str = "com.manojavamlabs.medha.gateway";

pub(super) fn register(program: &Path) -> Result<String> {
    let path = definition()?;
    std::fs::create_dir_all(path.parent().context("a service file has a folder")?)?;
    std::fs::write(&path, text(program)?).with_context(|| format!("writing {}", path.display()))?;
    start()?;
    Ok(format!("Service file: {}", path.display()))
}

pub(super) fn unregister() -> Result<()> {
    stop()?;
    let path = definition()?;
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

pub(super) fn registered() -> Option<String> {
    let path = definition().ok().filter(|path| path.exists())?;
    Some(path.display().to_string())
}

pub(super) fn start() -> Result<()> {
    // A job already loaded keeps its old definition until it is taken out first.
    stop()?;
    checked(
        "launchctl",
        &["bootstrap", &domain(), &definition()?.to_string_lossy()],
    )
}

pub(super) fn stop() -> Result<()> {
    let _ = run("launchctl", &["bootout", &format!("{}/{LABEL}", domain())]);
    Ok(())
}

/// A stopped job was taken out of launchd, so it is loaded again rather than kicked.
pub(super) fn restart() -> Result<()> {
    checked(
        "launchctl",
        &["kickstart", "-k", &format!("{}/{LABEL}", domain())],
    )
    .or_else(|_| start())
}

fn definition() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home folder")?
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn domain() -> String {
    // SAFETY: getuid takes no arguments, cannot fail and touches no memory of ours.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn text(program: &Path) -> Result<String> {
    let logs = runtime::config::medha_home()?.join("logs");
    std::fs::create_dir_all(&logs)?;
    // A service starts with a bare PATH; the agent's commands need the user's tools.
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let mut environment = format!("<key>PATH</key><string>{}</string>", xml(&path));
    if let Some(home) = medha_home() {
        environment.push_str(&format!(
            "<key>MEDHA_HOME</key><string>{}</string>",
            xml(&home)
        ));
    }
    let home = dirs::home_dir().context("no home folder")?;
    let log = xml(&logs.join("gateway.out.log").to_string_lossy());
    // Restarted after a crash only: the gateway waits out network trouble itself, and exits only
    // for a setup problem, which a restart cannot fix. ThrottleInterval spaces restarts.
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array><string>{}</string><string>gateway</string><string>run</string></array>
    <key>WorkingDirectory</key><string>{}</string>
    <key>EnvironmentVariables</key><dict>{environment}</dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><dict><key>Crashed</key><true/></dict>
    <key>ThrottleInterval</key><integer>30</integer>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        xml(&program.to_string_lossy()),
        xml(&home.to_string_lossy()),
    ))
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_path_cannot_break_out_of_the_plist_and_launchd_can_read_it() {
        assert_eq!(
            super::xml("/Users/a&b/<x>\""),
            "/Users/a&amp;b/&lt;x&gt;&quot;"
        );
        let folder = tempfile::tempdir().unwrap();
        let plist = folder.path().join("gateway.plist");
        let text = super::text(std::path::Path::new("/Users/a b/&<medha>")).unwrap();
        std::fs::write(&plist, text).unwrap();
        let lint = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(&plist)
            .output()
            .unwrap();
        assert!(
            lint.status.success(),
            "{}",
            String::from_utf8_lossy(&lint.stdout)
        );
    }
}
