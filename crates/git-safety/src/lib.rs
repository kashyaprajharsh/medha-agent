//! Shared policy for unattended Git processes.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
pub mod fetch;
mod workspace;
pub use workspace::project_root;

/// A `git` command that opens no console window when started from a windowed app on Windows.
pub fn git() -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = Command::new("git");
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    Command::new("git")
}

/// Whether a `git` program starts on this machine at all; checked once, since installing Git needs a restart anyway.
pub fn installed() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        git()
            .arg("--version")
            .env_clear()
            .envs(environment())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

pub fn environment() -> Vec<(OsString, OsString)> {
    // Looked up one name at a time: Windows matches `PATH` to its `Path`, an exact filter would not.
    let mut env: Vec<_> = [
        "PATH",
        "HOME",
        "USERPROFILE",
        "SystemRoot",
        "WINDIR",
        "TMPDIR",
        "TEMP",
        "TMP",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
    ]
    .into_iter()
    .filter_map(|key| Some((key.into(), std::env::var_os(key)?)))
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
    configured_arguments(dir, args, env, false, false)
}

/// Raw content operations never execute even an operator-configured filter.
pub fn raw_arguments(dir: &Path, args: &[String]) -> Result<Vec<String>, String> {
    configured_arguments(dir, args, environment(), true, false)
}

fn configured_arguments(
    dir: &Path,
    args: &[String],
    env: Vec<(OsString, OsString)>,
    raw: bool,
    operator_auth: bool,
) -> Result<Vec<String>, String> {
    let mut fixed = fixed();
    if operator_auth {
        fixed.retain(|setting| setting != "credential.helper=");
    }
    let mut child = git()
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
    // A busy machine or a process scanner can take seconds to start Git; a stuck one is still stopped.
    let deadline = Instant::now() + Duration::from_secs(10);
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
    if raw {
        blocked.extend(drivers);
        trusted.clear();
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
                if reading || raw {
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
        // SAFETY: geteuid takes no arguments, cannot fail and touches no memory of ours.
        meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o002 == 0
            && (meta.mode() & 0o020 == 0 || private_group(meta.gid()))
    }
    #[cfg(not(unix))]
    {
        std::env::var_os("USERPROFILE")
            .and_then(|home| std::fs::canonicalize(home).ok())
            .is_some_and(|home| real.starts_with(home))
    }
}

/// A user-private group (the Ubuntu default): the user's primary group, named after
/// them and listing no members, so group write grants no one else.
#[cfg(unix)]
fn private_group(gid: libc::gid_t) -> bool {
    use std::ffi::CStr;
    // SAFETY: getegid takes no arguments, cannot fail and touches no memory of ours.
    if gid != unsafe { libc::getegid() } {
        return false;
    }
    let mut user_buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: passwd is plain C data (integers and pointers); all-zero is a valid value, filled in below.
    let mut user: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    // SAFETY: every pointer is to a live local, the buffer length is its real length, and
    // the strings written into `user` point into `user_buffer`, which outlives their use.
    let looked_up = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut user,
            user_buffer.as_mut_ptr(),
            user_buffer.len(),
            &mut found,
        )
    };
    if looked_up != 0 || found.is_null() {
        return false;
    }
    let mut group_buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: group is plain C data (integers and pointers); all-zero is a valid value, filled in below.
    let mut group: libc::group = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    // SAFETY: as for getpwuid_r above: live locals, true buffer length, results pointing into `group_buffer`.
    let looked_up = unsafe {
        libc::getgrgid_r(
            gid,
            &mut group,
            group_buffer.as_mut_ptr(),
            group_buffer.len(),
            &mut found,
        )
    };
    if looked_up != 0 || found.is_null() {
        return false;
    }
    // SAFETY: both lookups succeeded, so each name is a NUL-terminated string inside its
    // buffer, and both buffers are still alive here.
    let named = unsafe { CStr::from_ptr(group.gr_name) == CStr::from_ptr(user.pw_name) };
    // SAFETY: gr_mem was checked non-null, and on success it points to a NULL-ended array in `group_buffer`.
    let alone = group.gr_mem.is_null() || unsafe { (*group.gr_mem).is_null() };
    named && alone
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
