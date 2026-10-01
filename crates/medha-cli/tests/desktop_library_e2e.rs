use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

/// One `desktop-service` for the whole library, as the desktop runs it.
struct Library {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Library {
    fn start(library: &Path, home: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_medha"))
            .args(["desktop-service", "--workspace"])
            .arg(library)
            .env("MEDHA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        writeln!(
            self.input,
            "{}",
            json!({"id": 1, "method": method, "params": params})
        )
        .unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert!(reply["error"].is_null(), "{reply}");
        reply["result"].clone()
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A model that refuses at once, so a turn ends without retrying.
fn refusing_model() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = std::io::Read::read(&mut stream, &mut [0u8; 8192]);
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    format!("http://{address}/v1")
}

/// A chat whose first message was saved; the model refusing it is enough.
fn used_chat(folder: &Path, home: &Path) {
    std::fs::create_dir_all(folder).unwrap();
    Command::new(env!("CARGO_BIN_EXE_medha"))
        .arg("hello there")
        .current_dir(folder)
        .env("MEDHA_HOME", home)
        .env("MEDHA_BASE_URL", refusing_model())
        .env("MEDHA_MODEL", "test-model")
        .env("MEDHA_API_KEY", "test-key")
        .env("MEDHA_PROTOCOL", "open-ai-chat")
        .output()
        .unwrap();
}

/// A chat that was opened, so it has a store, but never got a message.
fn started_chat(folder: &Path, home: &Path) {
    std::fs::create_dir_all(folder).unwrap();
    Library::start(folder, home).call("sessions.list", Value::Null);
}

fn stores(home: &Path) -> usize {
    std::fs::read_dir(home.join("projects")).unwrap().count()
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[test]
fn one_process_lists_the_library_and_only_unused_chats_are_pruned() {
    let root = tempfile::tempdir().unwrap();
    let (home, library) = (root.path().join("home"), root.path().join("library"));
    std::fs::create_dir_all(&home).unwrap();
    used_chat(&library.join("chat-used"), &home);
    started_chat(&library.join("chat-started"), &home);
    std::fs::create_dir_all(library.join("chat-never")).unwrap();
    std::fs::create_dir_all(library.join("chat-files")).unwrap();
    std::fs::write(library.join("chat-files/notes.md"), "kept").unwrap();

    let mut service = Library::start(&library, &home);
    service.call("hello", Value::Null);
    let before = stores(&home);
    let rows = service.call("library.sessions", json!({"prune_before": now() - 600.0}));
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["title"], "hello there");
    assert!(rows[0]["folder"].as_str().unwrap().ends_with("chat-used"));
    assert_eq!(stores(&home), before, "listing created a store for a chat");
    for chat in ["chat-used", "chat-started", "chat-never", "chat-files"] {
        assert!(
            library.join(chat).is_dir(),
            "{chat} was pruned inside its grace period"
        );
    }

    service.call("library.sessions", json!({"prune_before": now() + 60.0}));
    assert!(!library.join("chat-never").exists());
    assert!(!library.join("chat-started").exists());
    assert_eq!(stores(&home), before - 1, "the unused chat's store stayed");
    assert!(
        library.join("chat-files/notes.md").is_file(),
        "a person's file was lost"
    );
    assert!(
        library.join("chat-used").is_dir(),
        "a chat with history was pruned"
    );
    let rows = service.call("library.sessions", Value::Null);
    assert_eq!(rows.as_array().unwrap().len(), 1);

    let usage = service.call("library.usage", json!({"days": 7}));
    assert!(usage["total"].is_object(), "{usage}");
}
