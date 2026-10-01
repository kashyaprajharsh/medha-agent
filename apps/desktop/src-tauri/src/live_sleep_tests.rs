use super::*;
use std::io::Read;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};

/// Answers every chat request with "ok" and records how many messages it carried.
fn stand_in_model() -> (String, Arc<Mutex<Vec<usize>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let record = Arc::clone(&record);
            std::thread::spawn(move || answer(stream, &record));
        }
    });
    (url, seen)
}

fn answer(mut stream: std::net::TcpStream, record: &Mutex<Vec<usize>>) {
    let mut request = Vec::new();
    let mut buffer = [0u8; 8192];
    let body = loop {
        let Ok(read) = stream.read(&mut buffer) else {
            return;
        };
        if read == 0 {
            return;
        }
        request.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&request);
        let Some(split) = text.find("\r\n\r\n") else {
            continue;
        };
        let length = text[..split]
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        if request.len() >= split + 4 + length {
            break serde_json::from_slice::<Value>(&request[split + 4..split + 4 + length])
                .unwrap_or_default();
        }
    };
    let Some(messages) = body["messages"].as_array() else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    record.lock().unwrap().push(messages.len());
    let usage = json!({ "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6 });
    let response = if body["stream"] == true {
        let chunks = [
            json!({ "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "ok" } }] }),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }], "usage": usage }),
        ];
        let events: String = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{events}data: [DONE]\n\n"
        )
    } else {
        let json = json!({ "choices": [{ "index": 0, "finish_reason": "stop",
            "message": { "role": "assistant", "content": "ok" } }], "usage": usage })
        .to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
            json.len()
        )
    };
    let _ = stream.write_all(response.as_bytes());
}

struct Chat {
    sessions: LiveSessions,
    frames: Arc<Mutex<Vec<Value>>>,
    key: &'static str,
    root: PathBuf,
}

impl Chat {
    fn open(key: &'static str, model: &str) -> Option<Self> {
        let backend =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/debug/medha");
        if !backend.is_file() {
            eprintln!("skipped: build medha first (cargo build -p medha-cli)");
            return None;
        }
        let root = std::env::temp_dir().join(format!("medha-{key}-{}", std::process::id()));
        let (home, workspace) = (root.join("home"), root.join("workspace"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let env = [
            ("MEDHA_HOME", home.display().to_string()),
            ("MEDHA_BASE_URL", model.to_owned()),
            ("MEDHA_MODEL", "stand-in".into()),
            ("MEDHA_API_KEY", "unused".into()),
            ("MEDHA_MAX_CTX", "100000".into()),
        ];
        let sessions = LiveSessions::launching(
            workspace,
            Some(backend),
            env.iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect(),
        );
        let frames = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&frames);
        sessions
            .open_to(
                Arc::new(move |frame| sink.lock().unwrap().push(frame)),
                key,
                None,
            )
            .unwrap();
        let chat = Self {
            sessions,
            frames,
            key,
            root,
        };
        chat.wait("the greeting", |frame| frame["method"] == "ready");
        Some(chat)
    }

    fn wait(&self, what: &str, found: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(frame) = self
                .frames
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|frame| found(frame))
            {
                return frame.clone();
            }
            assert!(Instant::now() < deadline, "never saw {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn count(&self, found: impl Fn(&Value) -> bool) -> usize {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| found(frame))
            .count()
    }

    fn call(&self, method: &str, params: Value) -> Value {
        let id = self.sessions.request(self.key, method, params).unwrap();
        self.wait(method, |frame| {
            frame["id"] == id && (frame.get("result").is_some() || frame.get("error").is_some())
        })
    }

    fn turn(&self, text: &str) {
        let done = self.count(|frame| frame["params"]["kind"] == "turn.done");
        self.call("message.send", json!({ "content": text }));
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.count(|frame| frame["params"]["kind"] == "turn.done") == done {
            assert!(Instant::now() < deadline, "the turn never finished");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn exited(&self) -> bool {
        let mut open = self.sessions.inner.open.lock().unwrap();
        open.get_mut(self.key)
            .is_some_and(|live| matches!(live.child.try_wait(), Ok(Some(_))))
    }

    fn sleep(&self) {
        self.sessions.inner.sweep(Instant::now() + sleep::GRACE);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.exited() {
            assert!(
                Instant::now() < deadline,
                "the idle chat never went to sleep"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

impl Drop for Chat {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn an_empty_chat_sleeps_quietly_and_wakes_fresh_with_its_settings_and_new_id() {
    let Some(chat) = Chat::open("sleep-empty", "http://127.0.0.1:9/v1") else {
        return;
    };
    let first =
        chat.wait("the greeting", |frame| frame["method"] == "ready")["params"]["session"].clone();
    chat.call("session.configure", json!({ "mode": "yolo" }));

    sleep::focus(Some(chat.key.into()));
    chat.sessions.inner.sweep(Instant::now() + sleep::GRACE);
    std::thread::sleep(Duration::from_millis(500));
    assert!(!chat.exited(), "the chat on screen never sleeps");
    sleep::focus(None);

    chat.sleep();
    assert_eq!(
        chat.count(|frame| frame["method"] == "exit"),
        0,
        "a sleep is not a stop"
    );
    assert_eq!(
        chat.count(|frame| frame["id"] == "medha.sleep"),
        0,
        "the question stays inside"
    );

    let settings = chat.call("session.settings", json!({}));
    assert_eq!(
        settings["result"]["mode"], "yolo",
        "settings survive the sleep"
    );
    let again = chat.wait("the fresh greeting", |frame| {
        frame["method"] == "ready" && frame["params"]["session"] != first
    });
    assert!(
        again["params"]["session"].is_string(),
        "the window learns the new session id"
    );
}

#[test]
fn a_chat_with_history_wakes_into_the_same_conversation_without_greeting_again() {
    let (model, seen) = stand_in_model();
    let Some(chat) = Chat::open("sleep-history", &model) else {
        return;
    };
    chat.turn("first");
    let before = *seen.lock().unwrap().last().unwrap();
    chat.sleep();
    chat.turn("second");
    let after = *seen.lock().unwrap().last().unwrap();
    assert!(
        after >= before + 2,
        "the model sees the earlier turn: {before} -> {after}"
    );
    assert_eq!(
        chat.count(|frame| frame["method"] == "ready"),
        1,
        "a resumed chat does not greet again"
    );
    assert_eq!(chat.count(|frame| frame["method"] == "exit"), 0);
}

#[test]
fn requests_racing_a_sleep_question_are_never_lost() {
    let Some(chat) = Chat::open("sleep-race", "http://127.0.0.1:9/v1") else {
        return;
    };
    let stop = Arc::new(AtomicBool::new(false));
    let sweeping = {
        let (inner, stop) = (Arc::clone(&chat.sessions.inner), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                inner.sweep(Instant::now() + sleep::GRACE);
                std::thread::yield_now();
            }
        })
    };
    let sent: Vec<u64> = (0..12)
        .map(|_| {
            chat.sessions
                .request(chat.key, "session.settings", json!({}))
                .unwrap()
        })
        .collect();
    for id in sent {
        chat.wait("every request's answer", |frame| frame["id"] == id);
    }
    stop.store(true, Ordering::Relaxed);
    sweeping.join().unwrap();
}
