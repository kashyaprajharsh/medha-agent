//! A desktop follow-up sent while the model writes its final answer lands after
//! the last steer boundary. It must still reach the model, as the next run, and
//! a stop must hand it back instead of running it.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0u8; 8192];
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "incomplete HTTP request");
        request.extend_from_slice(&chunk[..read]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                let line = headers.lines().next().unwrap_or_default().to_string();
                return (line, request[end + 4..end + 4 + length].to_vec());
            }
        }
    }
}

fn respond(stream: &mut TcpStream, kind: &str, body: &str) {
    // A cancelled request may already have hung up.
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Holds the first chat request open until `release` fires; reports every chat body.
fn fake_provider(listener: TcpListener, seen: mpsc::Sender<Value>, release: mpsc::Receiver<()>) {
    let mut held = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => return,
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (line, body) = read_request(&mut stream);
        if !line.starts_with("post /v1/chat/completions") {
            respond(
                &mut stream,
                "application/json",
                r#"{"data":[{"id":"test-model","context_length":128000}]}"#,
            );
            continue;
        }
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if seen.send(body).is_err() {
            return;
        }
        if !held {
            held = true;
            let _ = release.recv_timeout(Duration::from_secs(30));
        }
        respond(
            &mut stream,
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Done.\"}}]}\n\ndata: [DONE]\n\n",
        );
    }
}

fn user_texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "user")
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect()
}

fn kind(frame: &Value) -> Option<&str> {
    frame["params"]["kind"].as_str()
}

struct Desktop {
    _root: tempfile::TempDir,
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    frames: mpsc::Receiver<Value>,
    log: Vec<Value>,
    seen: mpsc::Receiver<Value>,
    release: mpsc::Sender<()>,
}

impl Desktop {
    /// A desktop backend whose first model answer is held open while a
    /// follow-up is sent and acknowledged.
    fn follow_up_during_answer() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (seen_tx, seen) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        std::thread::spawn(move || fake_provider(listener, seen_tx, release_rx));

        let mut child = Command::new(env!("CARGO_BIN_EXE_medha"))
            .arg("--acp")
            .current_dir(&workspace)
            .env("MEDHA_HOME", &home)
            .env("MEDHA_BASE_URL", format!("http://{address}/v1"))
            .env("MEDHA_MODEL", "test-model")
            .env("MEDHA_API_KEY", "test-key")
            .env("MEDHA_PROTOCOL", "open-ai-chat")
            .env("MEDHA_TOKEN_ACCOUNTING", "adaptive")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (frame_tx, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(frame) = serde_json::from_str::<Value>(&line)
                    && frame_tx.send(frame).is_err()
                {
                    return;
                }
            }
        });
        let mut desktop = Desktop {
            _root: root,
            child,
            stdin,
            frames,
            log: Vec::new(),
            seen,
            release,
        };

        desktop.until(|frame| frame["method"] == "ready");
        desktop.send(json!({"jsonrpc": "2.0", "id": 1, "method": "message.send", "params": {"content": "first task"}}));
        let first = desktop
            .seen
            .recv_timeout(Duration::from_secs(30))
            .expect("first model request");
        assert_eq!(
            user_texts(&first).last().map(String::as_str),
            Some("first task")
        );
        // The model is answering: past the last boundary where it reads steers.
        desktop.send(json!({"jsonrpc": "2.0", "id": 2, "method": "message.send", "params": {"content": "late follow-up"}}));
        desktop.until(|frame| frame["id"] == 2);
        assert_eq!(desktop.log.last().unwrap()["result"]["accepted"], true);
        desktop
    }

    fn send(&mut self, request: Value) {
        writeln!(self.stdin, "{request}").unwrap();
    }

    fn until(&mut self, done: impl Fn(&Value) -> bool) {
        loop {
            let frame = self
                .frames
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("timed out; frames so far: {:#?}", self.log));
            self.log.push(frame);
            if done(self.log.last().unwrap()) {
                return;
            }
        }
    }

    fn events(&self, name: &str) -> Vec<&Value> {
        self.log
            .iter()
            .filter(|frame| kind(frame) == Some(name))
            .collect()
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        let _ = self.release.send(());
        let _ = writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 99, "method": "shutdown"})
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_follow_up_sent_during_the_final_answer_runs_next() {
    let mut desktop = Desktop::follow_up_during_answer();
    desktop.release.send(()).unwrap();

    let next = desktop
        .seen
        .recv_timeout(Duration::from_secs(30))
        .expect("the follow-up never reached the model");
    assert_eq!(
        user_texts(&next).last().map(String::as_str),
        Some("late follow-up")
    );
    desktop.until(|frame| kind(frame) == Some("turn.done"));
    assert!(
        desktop
            .events("message.steered")
            .iter()
            .any(|frame| frame["params"]["content"] == "late follow-up"),
        "delivery was not reported: {:#?}",
        desktop.log
    );
    assert!(
        desktop.events("message.returned").is_empty(),
        "a delivered follow-up was also handed back"
    );
    assert_eq!(
        desktop.events("turn.done").len(),
        1,
        "the first run must not report a stop while the follow-up runs"
    );
}

#[test]
fn stopping_hands_an_unread_follow_up_back_instead_of_running_it() {
    let mut desktop = Desktop::follow_up_during_answer();
    desktop.send(json!({"jsonrpc": "2.0", "id": 3, "method": "cancel"}));
    desktop.until(|frame| kind(frame) == Some("turn.cancelled"));

    let returned = desktop.events("message.returned");
    assert_eq!(returned.len(), 1, "{:#?}", desktop.log);
    assert_eq!(returned[0]["params"]["contents"], json!(["late follow-up"]));
    assert!(desktop.events("message.steered").is_empty());
    assert!(
        desktop
            .seen
            .recv_timeout(Duration::from_millis(500))
            .is_err(),
        "a stopped session ran the follow-up anyway"
    );
}
