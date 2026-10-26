//! A systemd user unit; lingering keeps it running after the last login ends, as on a server.

use super::{checked, medha_home, run};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const UNIT: &str = "medha-gateway.service";

pub(super) fn register(program: &Path) -> Result<String> {
    if run("systemctl", &["--user", "show-environment"])
        .map_or(true, |output| !output.status.success())
    {
        bail!(
            "systemd is not running for this user, so there is no service to install. Run `medha gateway run` under your own supervisor instead (tmux, a container restart policy)."
        );
    }
    let path = definition()?;
    std::fs::create_dir_all(path.parent().context("a service file has a folder")?)?;
    std::fs::write(&path, text(program)).with_context(|| format!("writing {}", path.display()))?;
    let _ = run("systemctl", &["--user", "daemon-reload"]);
    checked("systemctl", &["--user", "enable", "--now", UNIT])?;
    if let Ok(user) = std::env::var("USER")
        && run("loginctl", &["enable-linger", &user]).is_ok_and(|output| !output.status.success())
    {
        eprintln!(
            "Note: the gateway stops when you log out. To keep it running, ask an administrator to run: loginctl enable-linger {user}"
        );
    }
    Ok(format!("Service file: {}", path.display()))
}

pub(super) fn unregister() -> Result<()> {
    let _ = run("systemctl", &["--user", "disable", "--now", UNIT]);
    // Otherwise a unit whose last run failed stays listed after it is gone.
    let _ = run("systemctl", &["--user", "reset-failed", UNIT]);
    let path = definition()?;
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    let _ = run("systemctl", &["--user", "daemon-reload"]);
    Ok(())
}

pub(super) fn registered() -> Option<String> {
    let path = definition().ok().filter(|path| path.exists())?;
    Some(path.display().to_string())
}

pub(super) fn start() -> Result<()> {
    checked("systemctl", &["--user", "start", UNIT])
}

pub(super) fn stop() -> Result<()> {
    checked("systemctl", &["--user", "stop", UNIT])
}

pub(super) fn restart() -> Result<()> {
    checked("systemctl", &["--user", "restart", UNIT])
}

fn definition() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("no config folder")?
        .join("systemd/user")
        .join(UNIT))
}

fn text(program: &Path) -> String {
    // A service starts with a bare PATH; the agent's commands need the user's tools.
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let mut environment = format!("Environment={}\n", quote(&format!("PATH={path}")));
    if let Some(home) = medha_home() {
        environment.push_str(&format!(
            "Environment={}\n",
            quote(&format!("MEDHA_HOME={home}"))
        ));
    }
    // Restarted after a crash only: an exit means a setup problem a restart cannot fix.
    format!(
        "[Unit]\nDescription=Medha messaging gateway\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nType=simple\nExecStart={} gateway run\n{environment}Restart=on-abnormal\nRestartSec=10\n\n\
         [Install]\nWantedBy=default.target\n",
        quote(&program.to_string_lossy())
    )
}

/// systemd's own quoting: a double-quoted word with `\` and `"` escaped and `%` doubled.
fn quote(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_path_with_spaces_quotes_and_percent_reaches_systemd_as_written() {
        assert_eq!(
            super::quote(r#"/home/a b/50%"x"\y"#),
            r#""/home/a b/50%%\"x\"\\y""#
        );
    }
}
