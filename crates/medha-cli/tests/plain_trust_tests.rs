#![cfg(unix)]

use protocol::Command;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "common/backend.rs"]
mod backend;

struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    input: Box<dyn Write + Send>,
    output: Arc<Mutex<test_support::TerminalCapture>>,
}

impl Terminal {
    fn until(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let output = self.output.lock().unwrap();
            if output.contains(marker) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {marker:?}: {}",
                output.evidence()
            );
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "chat exited: {}",
                output.evidence()
            );
            drop(output);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.input, "{line}").unwrap();
        self.input.flush().unwrap();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn plain_chat_trusts_and_revokes_without_a_dialog_or_model_request() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("checkout");
    let folder = workspace.join("subfolder");
    let home = root.path().join("home");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(workspace.join(".git")).unwrap();
    std::fs::write(
        folder.join("medha.lock"),
        "[pricing]\ninput_per_mtok = 1.25\noutput_per_mtok = 2.5\n",
    )
    .unwrap();
    let mut backend = backend::Backend::start(&home, &[]);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_medha"));
    command.arg("--plain");
    command.cwd(&folder);
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("MEDHA_")) {
        command.env_remove(key);
    }
    command.env("TERM", "xterm-256color");
    command.env("MEDHA_HOME", &home);
    command.env(
        "MEDHA_BASE_URL",
        format!("http://{}/v1", listener.local_addr().unwrap()),
    );
    command.env("MEDHA_MODEL", "test-model");
    command.env("MEDHA_AUTH", "none");
    command.env("MEDHA_PROTOCOL", "open-ai-chat");
    command.env("MEDHA_MAX_CTX", "128000");
    command.env("MEDHA_CATALOGUE_OFFLINE", "1");
    command.env("MEDHA_CRED_STORE", "file");
    let child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let input = pair.master.take_writer().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let output = Arc::new(Mutex::new(test_support::TerminalCapture::new(40, 120)));
    let captured = output.clone();
    std::thread::spawn(move || {
        let mut bytes = [0u8; 8192];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            captured.lock().unwrap().record(&bytes[..count]);
        }
    });
    let mut terminal = Terminal {
        child,
        _master: pair.master,
        input,
        output,
    };
    terminal.until("interactive session");
    terminal.send("/trust");
    terminal.until("Checkout trusted;");
    terminal.until("pricing.input_per_mtok = \"1.25\"");
    let review = |backend: &mut backend::Backend| {
        let reply = backend
            .ask(
                &folder,
                protocol::ReadResource::METHOD,
                serde_json::json!(protocol::ReadResource::ProjectTrust),
            )
            .unwrap();
        let protocol::ResourceResult::ProjectTrust(Some(review)) =
            serde_json::from_value(reply).unwrap()
        else {
            panic!("missing project review")
        };
        review
    };
    let trusted = review(&mut backend);
    assert_eq!(trusted.root, workspace.canonicalize().unwrap());
    assert!(trusted.trusted);
    assert!(
        trusted.ignored.is_empty(),
        "the shown exact value was not accepted"
    );
    terminal.send("/trust --revoke");
    terminal.until("Revoked checkout trust and accepted settings;");
    let revoked = review(&mut backend);
    assert!(!revoked.trusted);
    assert_eq!(
        revoked.ignored,
        ["pricing.input_per_mtok", "pricing.output_per_mtok"]
    );
    assert!(
        terminal.child.try_wait().unwrap().is_none(),
        "revoke ended the chat"
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let output = terminal.output.lock().unwrap().evidence();
    assert!(
        !output.contains("approve "),
        "trust introduced a dialog: {output}"
    );
    terminal.send("/exit");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = terminal.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "plain chat did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}
