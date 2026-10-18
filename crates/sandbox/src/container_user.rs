//! Resolve one command's bind ownership without changing host permissions.

use crate::exec::ExecError;
#[cfg(unix)]
use crate::exec::run_command_bounded;
#[cfg(unix)]
use std::time::Duration;

pub(crate) fn explicit(user: &str) -> Result<Vec<String>, ExecError> {
    if user.is_empty()
        || user.starts_with('-')
        || user.len() > 128
        || !user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_:-.".contains(&byte))
    {
        return Err(ExecError::Unavailable(
            "container user must be a user or UID[:GID]".into(),
        ));
    }
    Ok(vec!["--user".into(), user.into()])
}

#[cfg(unix)]
pub(crate) async fn resolve(runtime: &str, user: Option<&str>) -> Result<Vec<String>, ExecError> {
    if let Some(user) = user {
        return explicit(user);
    }
    let mut command = tokio::process::Command::new(runtime);
    command.args(["info", "--format", "json"]);
    if !std::path::Path::new(runtime)
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains("podman"))
    {
        command = tokio::process::Command::new(runtime);
        command.args(["info", "--format", "{{json .}}"]);
    }
    let result = run_command_bounded(command, Duration::from_secs(5), 128 * 1024, None).await?;
    if !result.passed() {
        return Err(ExecError::Unavailable("cannot determine container daemon ownership; check the runtime or explicitly configure sandbox.user; no workload was started".into()));
    }
    let info: serde_json::Value = serde_json::from_str(&result.output).map_err(|_| {
        ExecError::Unavailable(
            "container runtime returned no usable ownership information; configure sandbox.user"
                .into(),
        )
    })?;
    let podman = info.get("host").is_some();
    let os = info["OSType"]
        .as_str()
        .or_else(|| info["host"]["os"].as_str());
    if os != Some("linux") {
        return Err(ExecError::Unavailable("automatic ownership requires a Linux container daemon; configure sandbox.user for this runtime".into()));
    }
    let security = info["SecurityOptions"].as_array();
    let rootless = info["host"]["security"]["rootless"]
        .as_bool()
        .unwrap_or_else(|| {
            security.is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.as_str().is_some_and(|item| item.contains("rootless")))
            })
        });
    let remapped = security.is_some_and(|items| {
        items
            .iter()
            .any(|item| item.as_str().is_some_and(|item| item.contains("userns")))
    });
    mapping(
        podman,
        rootless,
        remapped,
        unsafe { libc::geteuid() },
        unsafe { libc::getegid() },
    )
}

#[cfg(not(unix))]
pub(crate) async fn resolve(_runtime: &str, user: Option<&str>) -> Result<Vec<String>, ExecError> {
    user.map(explicit)
        .transpose()
        .map(Option::unwrap_or_default)
}

#[cfg(any(unix, test))]
fn mapping(
    podman: bool,
    rootless: bool,
    remapped: bool,
    uid: u32,
    gid: u32,
) -> Result<Vec<String>, ExecError> {
    if remapped && !rootless {
        return Err(ExecError::Unavailable("daemon user-namespace remapping cannot automatically preserve bind ownership; configure the image/user mapping explicitly; host permissions were not changed".into()));
    }
    if rootless && !podman {
        return explicit("0:0");
    }
    if uid == 0 {
        return Err(ExecError::Unavailable("automatic container ownership needs an unprivileged host user; explicitly configure sandbox.user if running Medha as root".into()));
    }
    let mut arguments = explicit(&format!("{uid}:{gid}"))?;
    if podman {
        arguments.push("--userns=keep-id".into());
    }
    Ok(arguments)
}

#[cfg(test)]
#[path = "container_user_tests.rs"]
mod tests;
