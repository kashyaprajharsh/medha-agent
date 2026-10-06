use super::*;
#[cfg(unix)]
use std::sync::mpsc::{Receiver, channel};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[test]
fn rejects_invalid_dimensions_keys_and_oversized_input() {
    let terminals = Terminals::new(PathBuf::from("."));
    assert!(size(0, 24).is_err());
    assert!(size(80, 0).is_err());
    assert!(size(501, 24).is_err());
    for key in ["", "../shell", "shell with spaces"] {
        assert!(validate_key(key).is_err());
    }
    assert!(
        terminals
            .write("missing", &"x".repeat(65537))
            .unwrap_err()
            .contains("too large")
    );
    assert!(terminals.resize("missing", 80, 24).is_err());
}

#[cfg(unix)]
fn shell(terminals: &Terminals, key: &str) -> Receiver<Value> {
    let (tx, rx) = channel();
    let mut command = CommandBuilder::new("/bin/sh");
    command.arg("-i");
    command.cwd(&terminals.workspace);
    command.env("TERM", "xterm-256color");
    command.env("PS1", "__PROMPT__ ");
    terminals
        .spawn(key, 80, 24, command, move |frame| {
            let _ = tx.send(frame);
        })
        .unwrap();
    rx
}

#[cfg(unix)]
fn until(rx: &Receiver<Value>, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut bytes = Vec::new();
    while Instant::now() < deadline {
        let frame = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!(
                    "Waiting for {needle:?}: {error}; received {:?}",
                    String::from_utf8_lossy(&bytes)
                )
            });
        assert_ne!(frame["kind"], "exit", "shell exited before expected output");
        bytes.extend(
            frame["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|byte| byte.as_u64().unwrap() as u8),
        );
        let output = String::from_utf8_lossy(&bytes);
        if output.contains(needle) {
            return output.into_owned();
        }
    }
    panic!("Did not receive {needle}");
}

#[cfg(unix)]
#[test]
fn real_shell_has_a_tty_workspace_unicode_resize_and_interrupts() {
    let workspace = std::env::temp_dir().canonicalize().unwrap();
    let terminals = Terminals::new(workspace.clone());
    let rx = shell(&terminals, "interactive");
    // An interactive shell may discard input typed before its first prompt.
    until(&rx, "__PROMPT__ ");
    terminals
        .write("interactive", "stty -echo; printf '__READY__\\n'\r")
        .unwrap();
    until(&rx, "__READY__\r\n");
    terminals
        .write(
            "interactive",
            "pwd; test -t 0 && test -t 1 && printf '__TTY__\\n'\r",
        )
        .unwrap();
    let output = until(&rx, "__TTY__\r\n");
    assert!(output.contains(workspace.to_str().unwrap()), "{output}");
    terminals.resize("interactive", 97, 31).unwrap();
    terminals
        .write("interactive", "stty size; printf 'नमस्ते\\n__RESIZED__\\n'\r")
        .unwrap();
    let output = until(&rx, "__RESIZED__\r\n");
    assert!(output.contains("31 97"), "{output}");
    assert!(output.contains("नमस्ते"), "{output}");
    terminals
        .write(
            "interactive",
            "sh -c \"printf '__BUSY__\\n'; exec sleep 20\"\r",
        )
        .unwrap();
    until(&rx, "__BUSY__\r\n");
    terminals.write("interactive", "\u{0003}").unwrap();
    until(&rx, "__PROMPT__ ");
    terminals
        .write("interactive", "printf '__INTERRUPTED__\\n'\r")
        .unwrap();
    until(&rx, "__INTERRUPTED__\r\n");
    terminals.write("interactive", "exit 7\r").unwrap();
    loop {
        let frame = rx.recv_timeout(Duration::from_secs(8)).unwrap();
        if frame["kind"] == "exit" {
            assert_eq!(frame["code"], 7);
            break;
        }
    }
    assert!(terminals.open.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn closing_a_tab_releases_its_shell_and_rejects_further_input() {
    let began = Instant::now();
    let terminals = Terminals::new(std::env::temp_dir());
    let rx = shell(&terminals, "closing");
    terminals.close("closing").unwrap();
    assert!(terminals.write("closing", "pwd\r").is_err());
    loop {
        if rx.recv_timeout(Duration::from_secs(8)).unwrap()["kind"] == "exit" {
            break;
        }
    }
    assert!(
        began.elapsed() < Duration::from_secs(4),
        "terminal close stalled: {:?}",
        began.elapsed()
    );
}
