//! Private-tty terminal init (PART 0.1).
//!
//! Some dependencies print to stdout unconditionally — e.g. a PDF text
//! extractor emits a "Unicode mismatch" line on ligatures, which any `web.fetch`
//! of an academic PDF triggers. On the alternate screen that spray corrupts the
//! display (the long-standing "input box jumps / gibberish" bug).
//!
//! The fix is structural and covers the whole class, not one crate: build the
//! ratatui terminal on a *duplicated* tty handle, then redirect the real fd 1/2
//! to `.medha/logs/stray-stdout.log`. The terminal keeps drawing to the private
//! handle (immune to the redirection), while any stray `println!`/`eprintln!`
//! from anywhere lands in the log instead of on screen. Restored on exit and via
//! a panic hook.

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::path::Path;

#[cfg(unix)]
pub type TuiTerminal = Terminal<CrosstermBackend<std::fs::File>>;
#[cfg(not(unix))]
pub type TuiTerminal = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Restores fd 1/2 (unix) on normal exit; the panic hook covers the panic path.
pub struct StrayRedirect {
    #[cfg(unix)]
    saved_out: std::os::fd::RawFd,
    #[cfg(unix)]
    saved_err: std::os::fd::RawFd,
    #[cfg(unix)]
    restored: bool,
}

/// Enter the alternate screen (on a private tty handle where possible), redirect
/// stray stdout/stderr to `stray_log`, and install a panic hook that restores
/// the terminal. Mirrors `ratatui::init` (raw mode + alt screen + panic hook)
/// but adds the fd redirection.
pub fn init(stray_log: &Path) -> anyhow::Result<(TuiTerminal, StrayRedirect)> {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        // Private dup of the real stdout (the tty): the terminal draws here, so
        // its output survives the fd 1/2 redirection installed just below.
        let tty_fd = unsafe { libc::dup(1) };
        if tty_fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // Safety: `tty_fd` is a fresh, exclusively-owned fd from dup(2).
        let mut tty = unsafe { std::fs::File::from_raw_fd(tty_fd) };

        let redirect = StrayRedirect::install(stray_log)?;
        install_panic_hook(redirect.saved_out, redirect.saved_err);

        enable_raw_mode()?;
        execute!(tty, EnterAlternateScreen, EnableBracketedPaste)?;
        let terminal = Terminal::new(CrosstermBackend::new(tty))?;
        Ok((terminal, redirect))
    }
    #[cfg(not(unix))]
    {
        let _ = stray_log; // no fd redirection off unix
        set_panic_hook_plain();
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
        Ok((terminal, StrayRedirect {}))
    }
}

/// Restore the terminal on normal exit: leave the alternate screen on the
/// terminal's own (private) handle, then undo the fd redirection.
pub fn restore(terminal: &mut TuiTerminal, redirect: &mut StrayRedirect) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableBracketedPaste);
    redirect.restore();
}

#[cfg(unix)]
impl StrayRedirect {
    fn install(stray_log: &Path) -> anyhow::Result<Self> {
        use std::os::fd::AsRawFd;
        if let Some(dir) = stray_log.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(stray_log)?;

        // Save the current fd 1/2 so they can be restored on exit.
        let saved_out = unsafe { libc::dup(1) };
        let saved_err = unsafe { libc::dup(2) };
        if saved_out < 0 || saved_err < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // Point fd 1/2 at the log. dup2 makes them share the log's open file
        // description, so `log` can drop without closing them.
        unsafe {
            libc::dup2(log.as_raw_fd(), 1);
            libc::dup2(log.as_raw_fd(), 2);
        }
        drop(log);

        Ok(Self { saved_out, saved_err, restored: false })
    }

    /// Restore fd 1/2 to the real tty. Idempotent.
    pub fn restore(&mut self) {
        if self.restored {
            return;
        }
        unsafe {
            libc::dup2(self.saved_out, 1);
            libc::dup2(self.saved_err, 2);
            libc::close(self.saved_out);
            libc::close(self.saved_err);
        }
        self.restored = true;
    }
}

#[cfg(not(unix))]
impl StrayRedirect {
    /// No-op off unix (no fd redirection was installed).
    pub fn restore(&mut self) {}
}

/// Panic hook (unix): restore fd 1/2 *first* so the panic message reaches the
/// tty (not the redirected log), then leave the alternate screen and disable raw
/// mode, then chain to the previous hook.
#[cfg(unix)]
fn install_panic_hook(saved_out: std::os::fd::RawFd, saved_err: std::os::fd::RawFd) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        unsafe {
            libc::dup2(saved_out, 1);
            libc::dup2(saved_err, 2);
        }
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableBracketedPaste);
        prev(info);
    }));
}

/// Panic hook (non-unix): standard ratatui-style terminal restore.
#[cfg(not(unix))]
fn set_panic_hook_plain() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableBracketedPaste);
        prev(info);
    }));
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A stray write to fd 1 must land in the log while redirected, then reach
    /// the real tty again after restore. Uses a raw `write(2)` (not `println!`)
    /// so it bypasses the test harness's thread-local stdout capture and truly
    /// exercises fd-level redirection — the exact path a dependency's stray
    /// `println!` takes at runtime. Does not install the global panic hook.
    #[test]
    fn stray_stdout_is_redirected_then_restored() {
        let dir = std::env::temp_dir().join(format!("medha-tty-test-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stray-stdout.log");

        let mut redirect = StrayRedirect::install(&log).unwrap();
        let msg = b"Unicode mismatch stray line\n";
        unsafe { libc::write(1, msg.as_ptr() as *const libc::c_void, msg.len()) };
        redirect.restore();

        let contents = std::fs::read_to_string(&log).unwrap();
        assert!(
            contents.contains("Unicode mismatch stray line"),
            "stray write should be captured in the log, got: {contents:?}"
        );

        // restore() is idempotent — a second call must not panic or re-close.
        redirect.restore();

        std::fs::remove_dir_all(&dir).ok();
    }
}
