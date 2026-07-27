//! Is the terminal's own background light or dark?
//!
//! Two tiers. **OSC 11** is what modern terminals actually answer; `COLORFGBG`
//! is an xterm/rxvt convention macOS Terminal.app never sets, so a
//! `COLORFGBG`-only probe fell through to dark and painted the dark palette onto
//! a white canvas. Either tier may decline, and `None` means "would not say" —
//! never a guess.

#[cfg(test)]
#[path = "termbg_tests.rs"]
mod tests;

/// Rec. 601 luma above this counts as a light canvas. Only a deliberately grey
/// terminal is ambiguous at the midpoint.
const LIGHT_LUMA: f32 = 0.5;

const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(120);

pub fn is_light() -> Option<bool> {
    query_luma()
        .map(|luma| luma > LIGHT_LUMA)
        .or_else(colorfgbg_is_light)
}

/// `COLORFGBG` is `fg;bg` (occasionally `fg;def;bg`). The final field is the
/// background palette index; 7 and 15 are the standard light-background signals.
fn colorfgbg_is_light() -> Option<bool> {
    let cfb = std::env::var("COLORFGBG").ok()?;
    let bg = cfb.rsplit(';').next()?.trim().parse::<u8>().ok()?;
    Some(matches!(bg, 7 | 15))
}

/// Ask the terminal for its background and return its perceived brightness.
/// Bounded: raw mode is held only for the round trip, and a terminal that
/// ignores OSC 11 costs one timeout at startup, not a hang.
fn query_luma() -> Option<f32> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }

    struct RawGuard;
    impl Drop for RawGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    crossterm::terminal::enable_raw_mode().ok()?;
    let _raw = RawGuard;

    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x1b\\").ok()?;
    out.flush().ok()?;

    // Read straight from fd 0, not through `std::io::stdin()`. `Stdin` is
    // BufReader-backed: its first read pulls the WHOLE reply into a userspace
    // buffer and hands back one byte, after which poll(2) sees an empty kernel
    // queue and reports "nothing ready" — so the loop gave up holding just the
    // leading ESC. Unbuffered reads keep poll(2) and the data in the same place.
    let mut buf = Vec::with_capacity(64);
    let deadline = std::time::Instant::now() + QUERY_TIMEOUT;
    while std::time::Instant::now() < deadline && buf.len() < 64 {
        if !stdin_ready(deadline) {
            break;
        }
        match read_stdin_byte() {
            Some(b) => {
                buf.push(b);
                if b == 0x07 || (b == b'\\' && buf.ends_with(b"\x1b\\")) {
                    break;
                }
            }
            None => break,
        }
    }

    parse_osc11_luma(&String::from_utf8_lossy(&buf))
}

#[cfg(unix)]
fn read_stdin_byte() -> Option<u8> {
    let mut b = 0u8;
    // SAFETY: a one-byte read into a live local, on the already-open fd 0.
    let n = unsafe { libc::read(0, std::ptr::addr_of_mut!(b).cast(), 1) };
    (n == 1).then_some(b)
}

#[cfg(not(unix))]
fn read_stdin_byte() -> Option<u8> {
    None
}

/// Perceived brightness of an OSC 11 reply (`ESC ] 11 ; rgb:RRRR/GGGG/BBBB`),
/// split from the I/O so the part that actually varies between terminals is
/// testable without a tty.
pub fn parse_osc11_luma(reply: &str) -> Option<f32> {
    let spec = reply.split("rgb:").nth(1)?;
    let mut parts = spec.split('/');
    // Components are hex of arbitrary width: xterm answers 4 nibbles, some
    // terminals 2. Normalise each by its own width rather than assuming 16-bit.
    let mut channel = || -> Option<f32> {
        let raw: String = parts
            .next()?
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if raw.is_empty() {
            return None;
        }
        let max = 16f32.powi(raw.len() as i32) - 1.0;
        Some(u32::from_str_radix(&raw, 16).ok()? as f32 / max)
    };
    let (r, g, b) = (channel()?, channel()?, channel()?);
    Some(0.299 * r + 0.587 * g + 0.114 * b)
}

/// `true` when stdin has a byte ready before `deadline`. Uses `poll(2)` so a
/// silent terminal cannot block the read.
#[cfg(unix)]
fn stdin_ready(deadline: std::time::Instant) -> bool {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return false;
    }
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: a single well-formed pollfd for stdin, with a bounded timeout.
    unsafe { libc::poll(&mut fd, 1, remaining.as_millis() as i32) == 1 }
}

#[cfg(not(unix))]
fn stdin_ready(_deadline: std::time::Instant) -> bool {
    false
}
