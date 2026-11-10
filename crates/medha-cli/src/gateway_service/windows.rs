//! A per-user start-up entry for `medhaw.exe`, which runs the gateway with no window and starts it
//! again after a crash. It needs no administrator, and shows in Task Manager's start-up apps.

use super::medha_home;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey,
    RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess,
};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "MedhaGateway";

pub(super) fn register(program: &Path) -> Result<String> {
    let line = command_line(&launcher(program)?);
    set(&line)?;
    // Restart, not start: a gateway already running picks up the new setup too.
    restart()?;
    Ok(format!("Start-up entry: HKCU\\{RUN}\\{VALUE}"))
}

pub(super) fn unregister() -> Result<()> {
    stop()?;
    let key = wide(VALUE);
    with_run_key(KEY_SET_VALUE, |run| {
        // SAFETY: `run` is the open key with_run_key passes; `key` is NUL-terminated UTF-16 from `wide`.
        let status = unsafe { RegDeleteValueW(run, key.as_ptr()) };
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            bail!("removing the start-up entry failed ({status})");
        }
        Ok(())
    })
}

pub(super) fn registered() -> Option<String> {
    get().map(|_| format!("start-up entry HKCU\\{RUN}\\{VALUE}"))
}

/// Starts the saved command now, as Windows does at login.
pub(super) fn start() -> Result<()> {
    if gateway::running() {
        return Ok(());
    }
    let line =
        get().context("The gateway service is not installed. Install it: medha gateway install")?;
    let (program, args) =
        split(&line).context("the start-up entry is not a command Medha wrote")?;
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    std::process::Command::new(program)
        .args(args)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .context("starting medhaw.exe")?;
    Ok(())
}

/// Ends the gateway; `medhaw` sees it end as a stop, not a crash, and ends too. The backend
/// stays, since the terminal or desktop may be using it.
pub(super) fn stop() -> Result<()> {
    let Some(pid) = gateway::running_pid() else {
        return Ok(());
    };
    // SAFETY: OpenProcess takes integers; a null result is checked just below.
    let process = unsafe {
        OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if process.is_null() {
        return Ok(());
    }
    let ours = image(process).is_some_and(|path| {
        path.file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("medha.exe"))
    });
    // Exit code 1 is the stop `medhaw` does not restart.
    // SAFETY: `process` is the open, non-null handle from OpenProcess, with terminate access.
    let ended = ours && unsafe { TerminateProcess(process, 1) } != 0;
    // SAFETY: the handle is ours and closed once, here.
    unsafe { CloseHandle(process) };
    if ours && !ended {
        bail!(
            "could not stop the gateway: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

pub(super) fn restart() -> Result<()> {
    stop()?;
    // The ended gateway's lock is released as Windows closes its handles.
    for _ in 0..50 {
        if !gateway::running() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    start()
}

/// `medhaw.exe` beside `medha.exe`; installers from the version that brought it put it there.
fn launcher(program: &Path) -> Result<PathBuf> {
    let launcher = program.with_file_name("medhaw.exe");
    if !launcher.is_file() {
        bail!(
            "{} is missing. Install Medha with its installer (0.1.13 or later) to start the gateway at login, or run `medha gateway run` yourself.",
            launcher.display()
        );
    }
    Ok(launcher)
}

fn command_line(launcher: &Path) -> String {
    let mut line = format!("\"{}\"", launcher.display());
    if let Some(home) = medha_home() {
        line.push_str(&format!(" --medha-home \"{home}\""));
    }
    line.push_str(" gateway run");
    line
}

/// The saved line as program and arguments; Windows paths cannot hold a double quote.
fn split(line: &str) -> Option<(String, Vec<String>)> {
    let mut words = Vec::new();
    let mut rest = line.trim();
    while !rest.is_empty() {
        let (word, after) = match rest.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"')?,
            None => rest.split_once(' ').unwrap_or((rest, "")),
        };
        words.push(word.to_owned());
        rest = after.trim_start();
    }
    let program = words.first()?.clone();
    Some((program, words.split_off(1)))
}

fn image(process: windows_sys::Win32::Foundation::HANDLE) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    // SAFETY: `length` holds the buffer's real size in units, so the call writes inside `buffer`.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            buffer.as_mut_ptr(),
            &mut length,
        )
    };
    (ok != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])))
}

fn set(line: &str) -> Result<()> {
    let (key, data) = (wide(VALUE), wide(line));
    with_run_key(KEY_SET_VALUE, |run| {
        let bytes = u32::try_from(data.len() * 2).context("the command is too long")?;
        // SAFETY: `run` is the open key; `key` and `data` are NUL-terminated UTF-16 and `bytes` is data's byte length.
        let status =
            unsafe { RegSetValueExW(run, key.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), bytes) };
        if status != ERROR_SUCCESS {
            bail!("saving the start-up entry failed ({status})");
        }
        Ok(())
    })
}

fn get() -> Option<String> {
    let key = wide(VALUE);
    with_run_key(KEY_QUERY_VALUE, |run| {
        let mut buffer = vec![0u16; 4096];
        let mut bytes = (buffer.len() * 2) as u32;
        // SAFETY: `bytes` is the buffer's real size, so the call writes inside `buffer`; a value
        // too large fails with ERROR_MORE_DATA instead of overrunning.
        let status = unsafe {
            RegGetValueW(
                run,
                std::ptr::null(),
                key.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if status != ERROR_SUCCESS {
            bail!("no start-up entry");
        }
        let units = (bytes as usize / 2).saturating_sub(1);
        Ok(String::from_utf16_lossy(&buffer[..units]))
    })
    .ok()
}

fn with_run_key<T>(access: u32, use_key: impl FnOnce(HKEY) -> Result<T>) -> Result<T> {
    let path = wide(RUN);
    let mut run: HKEY = std::ptr::null_mut();
    // SAFETY: `path` is NUL-terminated UTF-16 and `run` is a live HKEY for the opened key.
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, access, &mut run) };
    if status != ERROR_SUCCESS {
        bail!("opening the start-up list failed ({status})");
    }
    let result = use_key(run);
    // SAFETY: `run` was opened above and is closed once, here.
    unsafe { RegCloseKey(run) };
    result
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_saved_line_reads_back_as_the_same_program_and_arguments() {
        let line = r#""C:\Users\A B\AppData\Local\Medha\bin\medhaw.exe" --medha-home "D:\my home" gateway run"#;
        let (program, args) = super::split(line).unwrap();
        assert_eq!(program, r"C:\Users\A B\AppData\Local\Medha\bin\medhaw.exe");
        assert_eq!(args, ["--medha-home", r"D:\my home", "gateway", "run"]);
        assert!(super::split(r#""unclosed gateway run"#).is_none());
    }
}
