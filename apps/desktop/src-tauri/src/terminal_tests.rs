use super::*;
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::sync::mpsc::{Receiver, channel};
#[cfg(unix)]
use std::time::{Duration, Instant};

// Real shells share the host terminal subsystem; serialize functional checks.
#[cfg(unix)]
static SHELL_TEST: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone)]
struct SlowChild {
    release: Arc<(Mutex<bool>, Condvar)>,
    waited: Arc<AtomicBool>,
    caller: std::thread::ThreadId,
}
impl portable_pty::ChildKiller for SlowChild {
    fn kill(&mut self) -> std::io::Result<()> {
        assert_ne!(std::thread::current().id(), self.caller);
        let (lock, changed) = &*self.release;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = changed.wait(released).unwrap();
        }
        Ok(())
    }
    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}
impl Child for SlowChild {
    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        assert_ne!(std::thread::current().id(), self.caller);
        Ok(None)
    }
    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        self.waited.store(true, Ordering::Release);
        Ok(portable_pty::ExitStatus::with_exit_code(0))
    }
    fn process_id(&self) -> Option<u32> {
        None
    }
    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

struct CheckedWriter {
    waited: Arc<AtomicBool>,
    dropped: std::sync::mpsc::Sender<bool>,
}
impl Write for CheckedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Drop for CheckedWriter {
    fn drop(&mut self) {
        let _ = self.dropped.send(self.waited.load(Ordering::Acquire));
    }
}

#[test]
fn slow_teardown_never_blocks_close_and_keeps_handles_until_reaped() {
    let terminals = Terminals::new(PathBuf::from("."));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let unblock = Release(Arc::clone(&release));
    let (dropped, received) = std::sync::mpsc::channel();
    for key in ["one", "two", "three", "four"] {
        let waited = Arc::new(AtomicBool::new(false));
        let slot = reserve_slot(&terminals.occupied).unwrap();
        terminals.open.lock().unwrap().insert(
            key.into(),
            Terminal {
                master: None,
                input: Arc::new(Mutex::new(Box::new(CheckedWriter {
                    waited: Arc::clone(&waited),
                    dropped: dropped.clone(),
                }))),
                child: Some(Box::new(SlowChild {
                    release: Arc::clone(&release),
                    waited,
                    caller: std::thread::current().id(),
                })),
                generation: Arc::new(()),
                reaped: Arc::default(),
                slot: Some(slot),
            },
        );
    }
    let started = std::time::Instant::now();
    terminals.close("one").unwrap();
    terminals.close_all();
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    assert!(terminals.open.lock().unwrap().is_empty());
    assert_eq!(terminals.occupied.load(Ordering::Acquire), 4);
    assert!(
        received.try_recv().is_err(),
        "a writer closed before its child was reaped"
    );
    let error = terminals
        .spawn("five", 80, 24, CommandBuilder::new("unused"), |_| {})
        .unwrap_err();
    assert!(error.contains("closing"), "{error}");
    drop(unblock);
    for _ in 0..4 {
        assert!(
            received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
    }
    // Slot release follows writer destruction; wait for that final instruction.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while terminals.occupied.load(Ordering::Acquire) != 0 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
}

#[test]
fn closing_during_spawn_reaps_the_late_child_and_preserves_a_reused_key() {
    let terminals = Arc::new(Terminals::new(PathBuf::from(".")));
    let generation = Arc::new(());
    let (started, starting) = std::sync::mpsc::sync_channel(1);
    let (resume, resumed) = std::sync::mpsc::sync_channel(1);
    let (dropped, received) = std::sync::mpsc::channel();
    let waited = Arc::new(AtomicBool::new(false));
    let caller = std::thread::current().id();
    let opening = {
        let terminals = Arc::clone(&terminals);
        let generation = Arc::clone(&generation);
        std::thread::spawn(move || {
            terminals.spawn_in(
                "reused",
                (80, 24),
                generation,
                move |_| {
                    started.send(()).unwrap();
                    resumed.recv().unwrap();
                    Ok((
                        Terminal {
                            master: None,
                            input: Arc::new(Mutex::new(Box::new(CheckedWriter {
                                waited: Arc::clone(&waited),
                                dropped,
                            }))),
                            child: Some(Box::new(SlowChild {
                                release: Arc::new((Mutex::new(true), Condvar::new())),
                                waited,
                                caller,
                            })),
                            generation: Arc::new(()),
                            reaped: Arc::default(),
                            slot: None,
                        },
                        Box::new(std::io::empty()) as Output,
                    ))
                },
                |_| {},
            )
        })
    };
    starting
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    let closing = std::time::Instant::now();
    terminals.close_generation("reused", &generation).unwrap();
    assert!(closing.elapsed() < std::time::Duration::from_millis(500));
    let replacement = Arc::new(());
    terminals.open.lock().unwrap().insert(
        "reused".into(),
        Terminal {
            master: None,
            input: Arc::new(Mutex::new(Box::new(std::io::sink()))),
            child: None,
            generation: Arc::clone(&replacement),
            reaped: Arc::default(),
            slot: None,
        },
    );
    resume.send(()).unwrap();
    assert!(opening.join().unwrap().unwrap_err().contains("closed"));
    assert!(
        received
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
    );
    assert!(Arc::ptr_eq(
        &terminals.open.lock().unwrap()["reused"].generation,
        &replacement
    ));
    terminals.close_all();
}

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
    let _alone = SHELL_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    // Output precedes the shell's job/terminal restoration. Its next prompt
    // establishes that the previous foreground job has settled before resize.
    if !output
        .rsplit_once("__TTY__\r\n")
        .unwrap()
        .1
        .contains("__PROMPT__ ")
    {
        until(&rx, "__PROMPT__ ");
    }
    terminals.resize("interactive", 97, 31).unwrap();
    {
        let open = terminals.open.lock().unwrap();
        let actual = open
            .get("interactive")
            .unwrap()
            .master
            .as_ref()
            .unwrap()
            .get_size()
            .unwrap();
        assert_eq!((actual.cols, actual.rows), (97, 31));
    }
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
    let _alone = SHELL_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let terminals = Terminals::new(std::env::temp_dir());
    let rx = shell(&terminals, "closing");
    let pid = terminals.open.lock().unwrap()["closing"]
        .child
        .as_ref()
        .unwrap()
        .process_id()
        .unwrap();
    let started = Instant::now();
    terminals.close("closing").unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "close blocked the app thread"
    );
    assert!(terminals.write("closing", "pwd\r").is_err());
    // This bounds cleanup from the request, including OS signal/reap time.
    // The old eight-second receive began *after* a blocking close. macOS can
    // spend about thirteen seconds signaling a newly started PTY shell.
    let cleanup = started + Duration::from_secs(20);
    loop {
        if rx
            .recv_timeout(cleanup.saturating_duration_since(Instant::now()))
            .unwrap()["kind"]
            == "exit"
        {
            break;
        }
    }
    assert_eq!(
        unsafe { libc::kill(pid as i32, 0) },
        -1,
        "the shell survived cleanup"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert_eq!(terminals.occupied.load(Ordering::Acquire), 0);
}
