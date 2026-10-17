//! Shared policy for unattended Git processes.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
mod workspace;
pub use workspace::project_root;

pub fn environment() -> Vec<(OsString, OsString)> {
    let mut env: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| {
            matches!(
                key.to_str(),
                Some(
                    "PATH"
                        | "HOME"
                        | "USERPROFILE"
                        | "SystemRoot"
                        | "SYSTEMROOT"
                        | "WINDIR"
                        | "TMPDIR"
                        | "TEMP"
                        | "TMP"
                        | "LANG"
                        | "LC_ALL"
                        | "LC_CTYPE"
                )
            )
        })
        .collect();
    env.extend(
        [("GIT_TERMINAL_PROMPT", "0"), ("GIT_OPTIONAL_LOCKS", "0")]
            .map(|(key, value)| (key.into(), value.into())),
    );
    env
}

fn fixed() -> Vec<String> {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    [
        "core.fsmonitor=false",
        "credential.helper=",
        "core.pager=cat",
        "core.editor=false",
        "core.askPass=false",
    ]
    .map(str::to_owned)
    .into_iter()
    .chain([format!("core.hooksPath={null}")])
    .collect()
}

pub fn arguments(dir: &Path, args: &[String]) -> Result<Vec<String>, String> {
    arguments_with_environment(dir, args, environment())
}

fn arguments_with_environment(
    dir: &Path,
    args: &[String],
    env: Vec<(OsString, OsString)>,
) -> Result<Vec<String>, String> {
    let fixed = fixed();
    let mut child = Command::new("git")
        .arg("--no-pager")
        .args(fixed.iter().flat_map(|value| ["-c", value]))
        .args(["config", "--list", "--show-scope", "--show-origin", "-z"])
        .current_dir(dir)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Git configuration could not be inspected: {error}"))?;
    let pipe = child
        .stdout
        .take()
        .ok_or("Git configuration output unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Git configuration inspection exceeded its limit".into());
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| "Git configuration reader failed")?
        .map_err(|error| error.to_string())?;
    if !status.success() || bytes.len() > 1024 * 1024 {
        return Err("Git configuration could not be safely inspected".into());
    }
    let root = dir.canonicalize().map_err(|error| error.to_string())?;
    let fields: Vec<_> = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    if fields.len() % 3 != 0 {
        return Err("Git configuration inspection returned an unsupported format".into());
    }
    let mut blocked = BTreeSet::new();
    let mut drivers = BTreeSet::new();
    let mut trusted = std::collections::BTreeMap::new();
    for row in fields.chunks_exact(3) {
        let record = std::str::from_utf8(row[2]).map_err(|_| "Git configuration is not UTF-8")?;
        let (key, value) = record.split_once('\n').unwrap_or((record, ""));
        let Some((driver, setting)) = key
            .strip_prefix("filter.")
            .and_then(|rest| rest.rsplit_once('.'))
        else {
            continue;
        };
        if !matches!(setting, "clean" | "smudge" | "process" | "required") {
            continue;
        }
        drivers.insert(driver.to_owned());
        if drivers.len() > 256 {
            return Err("Git configuration exceeds 256 filter drivers".into());
        }
        if trusted_origin(row[0], row[1], &root) {
            trusted.insert(key.to_owned(), (driver.to_owned(), value.to_owned()));
        } else if setting != "required" {
            blocked.insert(driver.to_owned());
        }
    }
    let mut settings = fixed;
    let reading = args.first().is_some_and(|command| {
        matches!(
            command.as_str(),
            "status"
                | "diff"
                | "show"
                | "log"
                | "blame"
                | "rev-parse"
                | "ls-files"
                | "ls-tree"
                | "for-each-ref"
                | "symbolic-ref"
                | "check-ignore"
                | "merge-base"
        )
    });
    for (key, (driver, value)) in trusted {
        if !blocked.contains(&driver) {
            settings.push(format!("{key}={value}"));
        }
    }
    for driver in blocked {
        settings.extend(
            [
                "clean=",
                "smudge=",
                "process=",
                if reading {
                    "required=false"
                } else {
                    "required=true"
                },
            ]
            .map(|value| format!("filter.{driver}.{value}")),
        );
    }
    let mut hardened = vec!["--no-pager".into()];
    for setting in settings {
        hardened.extend(["-c".into(), setting]);
    }
    if let Some((subcommand, rest)) = args.split_first() {
        hardened.push(subcommand.clone());
        if matches!(subcommand.as_str(), "diff" | "show" | "log" | "blame") {
            hardened.extend(["--no-ext-diff".into(), "--no-textconv".into()]);
        }
        hardened.extend_from_slice(rest);
    }
    Ok(hardened)
}

fn trusted_origin(scope: &[u8], origin: &[u8], workspace: &Path) -> bool {
    if !matches!(scope, b"global" | b"system") {
        return false;
    }
    let Some(path) = std::str::from_utf8(origin)
        .ok()
        .and_then(|value| value.strip_prefix("file:"))
    else {
        return false;
    };
    let path = workspace.join(path);
    let Ok(real) = path.canonicalize() else {
        return false;
    };
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let checkout = project_root(workspace, &home)
        .ok()
        .flatten()
        .unwrap_or_else(|| workspace.to_path_buf());
    if real.starts_with(checkout) {
        return false;
    }
    let Ok(meta) = std::fs::metadata(&real) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        std::env::var_os("USERPROFILE")
            .and_then(|home| std::fs::canonicalize(home).ok())
            .is_some_and(|home| real.starts_with(home))
    }
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
