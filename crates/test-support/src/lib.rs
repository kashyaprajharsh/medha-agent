//! Test fixtures shared across the workspace.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A temporary folder removed when dropped, pass or fail; usable wherever a path is.
pub struct Scratch {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

/// A fresh folder named after `tag`, e.g. `scratch("medha-store")`.
pub fn scratch(tag: &str) -> Scratch {
    let dir = tempfile::Builder::new()
        .prefix(&format!("{tag}-"))
        .tempdir()
        .expect("create a scratch folder");
    let path = dir.path().to_path_buf();
    Scratch { _dir: dir, path }
}

impl Scratch {
    /// The same folder, pointing at `relative` inside it.
    pub fn at(self, relative: impl AsRef<Path>) -> Scratch {
        let path = self.path.join(relative);
        Scratch {
            _dir: self._dir,
            path,
        }
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<OsStr> for Scratch {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}

/// Bounded PTY evidence decoded as a terminal screen, including incremental draws.
/// Raw bytes remain available for explicit escape-sequence assertions at exit.
#[cfg(unix)]
pub struct TerminalCapture {
    raw: Vec<u8>,
    parser: vt100::Parser,
}

#[cfg(unix)]
impl TerminalCapture {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            raw: Vec::new(),
            parser: vt100::Parser::new(rows, cols, 0),
        }
    }

    pub fn record(&mut self, bytes: &[u8]) {
        assert!(
            self.raw.len() + bytes.len() <= 2 * 1024 * 1024,
            "test PTY output exceeded its bound"
        );
        self.raw.extend_from_slice(bytes);
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    pub fn contains(&self, marker: &str) -> bool {
        if marker.starts_with('\x1b') {
            self.raw
                .windows(marker.len())
                .any(|bytes| bytes == marker.as_bytes())
        } else {
            self.parser.screen().contents().contains(marker)
        }
    }

    pub fn evidence(&self) -> String {
        format!(
            "screen {:?}; raw tail {:?}",
            self.parser.screen().contents(),
            String::from_utf8_lossy(&self.raw[self.raw.len().saturating_sub(8192)..])
        )
    }
}

#[cfg(all(test, unix))]
mod terminal_tests {
    use super::TerminalCapture;

    #[test]
    fn cursor_addressed_reply_is_found_across_split_escape_sequences() {
        let mut output = TerminalCapture::new(40, 120);
        output.record(b"\x1b[8;5Hecho:\x1b[8;");
        assert!(!output.contains("echo: FROM_OTHER_VIEWER"));
        output.record(b"11HFROM_OTHER_VIEWER");
        assert!(output.contains("echo: FROM_OTHER_VIEWER"));
        output.record(b"\x1b[8;5H\x1b[2K");
        assert!(!output.contains("echo: FROM_OTHER_VIEWER"));
    }

    #[test]
    fn resizing_and_exit_evidence_do_not_depend_on_old_screen_contents() {
        let mut output = TerminalCapture::new(40, 120);
        output.record(b"\x1b[?1049h\x1b[40;1Hold screen");
        assert!(output.contains("old screen"));
        output.resize(24, 80);
        output.record(b"\x1b[2J\x1b[1;1Hnew screen\x1b[?1049");
        assert!(output.contains("new screen"));
        assert!(!output.contains("old screen"));
        output.record(b"l");
        assert!(output.contains("\x1b[?1049l"));
    }
}
