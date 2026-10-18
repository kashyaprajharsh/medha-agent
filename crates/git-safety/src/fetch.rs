//! The explicit plugin fetch keeps operator authentication, never ambient agent keys.

use std::{ffi::OsString, path::Path};

pub fn environment() -> Vec<(OsString, OsString)> {
    let mut env = super::environment();
    for key in ["SSH_AUTH_SOCK", "SSH_AGENT_PID"] {
        if let Some(value) = std::env::var_os(key) {
            env.push((key.into(), value));
        }
    }
    env.extend([
        (
            "GIT_SSH_COMMAND".into(),
            "ssh -oBatchMode=yes -oForwardAgent=no".into(),
        ),
        ("GCM_INTERACTIVE".into(), "never".into()),
    ]);
    env
}

pub fn arguments(dir: &Path, args: &[String]) -> Result<Vec<String>, String> {
    arguments_with_environment(dir, args, environment())
}

fn arguments_with_environment(
    dir: &Path,
    args: &[String],
    env: Vec<(OsString, OsString)>,
) -> Result<Vec<String>, String> {
    let mut hardened = super::configured_arguments(dir, args, env, true, true)?;
    // The caller supplies a private clone directory, never a project checkout.
    // Retain operator authentication only for this explicitly requested fetch.
    let settings = [
        "protocol.allow=never",
        "protocol.https.allow=always",
        "protocol.http.allow=always",
        "protocol.ssh.allow=always",
        "protocol.file.allow=always",
        "submodule.recurse=false",
        "fetch.recurseSubmodules=false",
        "advice.detachedHead=false",
        "core.sshCommand=ssh -oBatchMode=yes -oForwardAgent=no",
    ];
    hardened.splice(
        1..1,
        settings
            .into_iter()
            .flat_map(|setting| ["-c".into(), setting.into()]),
    );
    Ok(hardened)
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
