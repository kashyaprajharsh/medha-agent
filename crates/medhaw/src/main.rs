//! `medhaw` runs `medha` with the given arguments and no window, and starts it again after a crash.
//! Windows starts it at login for the messaging gateway: a program with a console of its own is
//! shown on screen and can be closed by Windows at logon.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

/// A run this long resets the wait before the next restart.
const STEADY: Duration = Duration::from_secs(5 * 60);
const LONGEST_WAIT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let Some(medha) = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(MEDHA)))
    else {
        return ExitCode::FAILURE;
    };
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // A home other than the default cannot come from Windows' start-up list, so it comes as an argument.
    let home = match args.first().map(String::as_str) {
        Some("--medha-home") if args.len() > 1 => Some(args.drain(..2).nth(1).unwrap_or_default()),
        _ => None,
    };
    let mut wait = Duration::from_secs(10);
    loop {
        let started = Instant::now();
        let mut command = Command::new(&medha);
        command.args(&args);
        if let Some(home) = &home {
            command.env("MEDHA_HOME", home);
        }
        hidden(&mut command);
        let Ok(status) = command.status() else {
            return ExitCode::FAILURE;
        };
        if !restarts(status.code()) {
            return ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1));
        }
        if started.elapsed() >= STEADY {
            wait = Duration::from_secs(10);
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(LONGEST_WAIT);
    }
}

/// `medha` exits 0 when stopped and 1 when it reported a problem, such as missing setup, that a
/// restart cannot fix. Any other end is a crash or a kill, and it runs again.
fn restarts(code: Option<i32>) -> bool {
    !matches!(code, Some(0 | 1))
}

#[cfg(windows)]
const MEDHA: &str = "medha.exe";
#[cfg(not(windows))]
const MEDHA: &str = "medha";

/// No window of its own; the programs it starts share its hidden console instead of opening theirs.
#[cfg(windows)]
fn hidden(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hidden(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    #[test]
    fn a_crash_restarts_but_a_stop_or_a_reported_problem_does_not() {
        assert!(!super::restarts(Some(0)));
        assert!(!super::restarts(Some(1)));
        // A Rust panic, Windows' Ctrl+C end, and a kill by signal on Unix.
        assert!(super::restarts(Some(101)));
        assert!(super::restarts(Some(0xC000_013Au32 as i32)));
        assert!(super::restarts(None));
    }
}
