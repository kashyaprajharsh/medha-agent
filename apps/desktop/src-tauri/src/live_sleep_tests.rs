use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;

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

fn answer(stream: std::net::TcpStream, record: &Mutex<Vec<usize>>) {
    answer_held(stream, record, None);
}

struct HeldReply {
    arrived: std::sync::mpsc::Sender<()>,
    released: Mutex<std::sync::mpsc::Receiver<()>>,
}

fn held_model() -> (
    String,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let model = format!("http://{}/v1", listener.local_addr().unwrap());
    let (arrived, waiting) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let held = Arc::new(HeldReply {
        arrived,
        released: Mutex::new(released),
    });
    std::thread::spawn(move || {
        let record = Arc::new(Mutex::new(Vec::new()));
        for stream in listener.incoming().flatten() {
            let (record, held) = (Arc::clone(&record), Arc::clone(&held));
            std::thread::spawn(move || answer_held(stream, &record, Some(&held)));
        }
    });
    (model, waiting, release)
}

fn answer_held(
    mut stream: std::net::TcpStream,
    record: &Mutex<Vec<usize>>,
    held: Option<&HeldReply>,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
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
    let last_user = messages
        .iter()
        .rev()
        .find(|message| message["role"] == "user");
    if last_user.is_some_and(|message| message["content"].to_string().contains("HOLD"))
        && let Some(held) = held
    {
        let _ = held.arrived.send(());
        let _ = held
            .released
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(20));
    }
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

/// One backend at a time: six starting together slow every other test that starts a process.
static ONE_BACKEND: Mutex<()> = Mutex::new(());

struct Chat {
    sessions: LiveSessions,
    frames: Arc<Mutex<Vec<Value>>>,
    key: &'static str,
    root: PathBuf,
    /// While set, the window draws nothing more: each frame it is given holds it up.
    stuck: Arc<AtomicBool>,
    _alone: MutexGuard<'static, ()>,
}

impl Chat {
    fn open(key: &'static str, model: &str) -> Option<Self> {
        let chat = Self::start(key, model)?;
        chat.wait("the greeting", |frame| frame["method"] == "ready");
        Some(chat)
    }

    fn emit(&self) -> Emit {
        let (sink, stuck) = (Arc::clone(&self.frames), Arc::clone(&self.stuck));
        Arc::new(move |frame| {
            sink.lock().unwrap().push(frame);
            while stuck.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    }

    /// Asked to open, and returned before it has.
    fn start(key: &'static str, model: &str) -> Option<Self> {
        let name = if cfg!(windows) { "medha.exe" } else { "medha" };
        let backend = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target/debug")
            .join(name);
        if !backend.is_file() {
            // Where the checks are the gate, a missing program must not pass for a passing test.
            assert!(
                std::env::var_os("CI").is_none(),
                "the backend was not built before the desktop tests ran"
            );
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
            ("MEDHA_CRED_STORE", "file".into()),
        ];
        let env = env
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect();
        let alone = ONE_BACKEND.lock().unwrap_or_else(|held| held.into_inner());
        let sessions = LiveSessions::on(workspace, Backend::owned(backend, env));
        let chat = Self {
            sessions,
            frames: Arc::default(),
            key,
            root,
            stuck: Arc::default(),
            _alone: alone,
        };
        chat.sessions.open_to(chat.emit(), key, None).unwrap();
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
        let open = self.sessions.inner.open.lock().unwrap();
        open.get(self.key).is_some_and(Live::has_ended)
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
        self.sessions.inner.backend.stop();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
struct TuiClient {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    input: Box<dyn Write + Send>,
    output: Arc<Mutex<test_support::TerminalCapture>>,
}

#[cfg(unix)]
impl TuiClient {
    fn start(chat: &Chat, model: &str, session: &str) -> Self {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let executable =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target/debug/medha");
        let mut command = portable_pty::CommandBuilder::new(executable);
        command.args(["--resume", session]);
        command.cwd(&chat.sessions.inner.workspace);
        command.env("MEDHA_HOME", chat.root.join("home"));
        command.env("MEDHA_BASE_URL", model);
        command.env("MEDHA_MODEL", "stand-in");
        command.env("MEDHA_API_KEY", "unused");
        command.env("MEDHA_MAX_CTX", "100000");
        command.env("MEDHA_CRED_STORE", "file");
        command.env("MEDHA_MODE", "careful");
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let input = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let output = Arc::new(Mutex::new(test_support::TerminalCapture::new(40, 120)));
        let captured = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(length) = reader.read(&mut buffer) {
                if length == 0 {
                    break;
                }
                captured.lock().unwrap().record(&buffer[..length]);
            }
        });
        Self {
            child,
            _master: pair.master,
            input,
            output,
        }
    }
    fn write(&mut self, text: &str) {
        self.input.write_all(text.as_bytes()).unwrap();
        self.input.flush().unwrap();
    }
    fn until(&self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let output = self.output.lock().unwrap();
            if output.contains(marker) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "TUI never showed {marker:?}: {}",
                output.evidence()
            );
            drop(output);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn exit(&mut self) {
        self.write("\x04");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "TUI failed: {status:?}");
                self.until("\x1b[?1049l");
                return;
            }
            assert!(Instant::now() < deadline, "TUI did not exit after Ctrl-D");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(unix)]
impl Drop for TuiClient {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            if let Some(pid) = self.child.process_id() {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            } else {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
        }
    }
}

#[cfg(unix)]
#[test]
fn real_tui_and_desktop_follow_one_conversation_and_either_viewer_can_leave() {
    let (model, waiting, release) = held_model();
    let Some(chat) = Chat::open("tui-desktop", &model) else {
        return;
    };
    chat.turn("seed for shared history");
    let session = chat.wait("ready", |frame| frame["method"] == "ready")["params"]["session"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut tui = TuiClient::start(&chat, &model, &session);
    tui.until("seed for shared history");
    chat.sleep();
    assert!(chat.call("session.settings", json!({}))["result"].is_object());
    tui.write("HOLD from actual TUI\r");
    waiting.recv_timeout(Duration::from_secs(20)).unwrap();
    // Detach the original desktop while the TUI owns an active turn.
    chat.sessions.close(chat.key).unwrap();
    let home = chat.root.join("home/serve");
    let backend = Backend::joined_to(
        std::fs::read_to_string(home.join("address")).unwrap(),
        &std::fs::read_to_string(home.join("token")).unwrap(),
    );
    let follower = LiveSessions::on(chat.sessions.inner.workspace.clone(), backend);
    let frames: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&frames);
    follower
        .open_to(
            Arc::new(move |frame| sink.lock().unwrap().push(frame)),
            "following",
            Some(&session),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if frames.lock().unwrap().iter().any(|frame| {
            frame["method"] == "session.presentation" && frame["params"]["running"] == true
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "desktop never attached to the active TUI turn: {:?}",
            frames.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let id = follower.request("following", "cancel", json!({})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if frames
            .lock()
            .unwrap()
            .iter()
            .any(|frame| frame["id"] == id && frame["error"].is_null())
            && frames
                .lock()
                .unwrap()
                .iter()
                .any(|frame| frame["params"]["kind"] == "turn.cancelled")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "desktop cancellation did not settle the TUI turn"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    release.send(()).unwrap();
    tui.exit();
    let id = follower
        .request(
            "following",
            "message.send",
            json!({"content":"desktop after TUI exit"}),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if frames
            .lock()
            .unwrap()
            .iter()
            .any(|frame| frame["id"] == id && frame["error"].is_null())
            && frames
                .lock()
                .unwrap()
                .iter()
                .any(|frame| frame["params"]["kind"] == "turn.done")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "desktop chat stopped when the TUI left: {:?}",
            frames.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !frames
            .lock()
            .unwrap()
            .iter()
            .any(|frame| frame["method"] == "exit")
    );
    follower.close("following").unwrap();
}

#[test]
fn an_independent_desktop_connection_attaches_to_an_active_turn_and_survives_owner_detach() {
    let (model, waiting, release) = held_model();
    let Some(chat) = Chat::open("independent-viewer", &model) else {
        return;
    };
    chat.turn("saved history");
    let session = chat.wait("ready", |frame| frame["method"] == "ready")["params"]["session"]
        .as_str()
        .unwrap()
        .to_owned();
    chat.call(
        "message.send",
        json!({"content":"HOLD while another desktop opens"}),
    );
    waiting.recv_timeout(Duration::from_secs(20)).unwrap();
    let home = chat.root.join("home/serve");
    let backend = Backend::joined_to(
        std::fs::read_to_string(home.join("address")).unwrap(),
        &std::fs::read_to_string(home.join("token")).unwrap(),
    );
    let follower = LiveSessions::on(chat.sessions.inner.workspace.clone(), backend);
    let frames: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&frames);
    follower
        .open_to(
            Arc::new(move |frame| sink.lock().unwrap().push(frame)),
            "following",
            Some(&session),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if frames.lock().unwrap().iter().any(|frame| {
            frame["method"] == "session.presentation" && frame["params"]["running"] == true
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the follower never received active state: {:?}",
            frames.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    chat.sessions.close_all();
    assert!(
        chat.sessions
            .open_to(chat.emit(), "after-window-quit", None)
            .is_err()
    );
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if frames
            .lock()
            .unwrap()
            .iter()
            .any(|frame| frame["params"]["kind"] == "turn.done")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the original viewer interrupted the follower's turn"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let id = follower
        .request("following", "session.settings", json!({}))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if frames
            .lock()
            .unwrap()
            .iter()
            .any(|frame| frame["id"] == id && frame["result"].is_object())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the detached owner stopped the live chat"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !frames
            .lock()
            .unwrap()
            .iter()
            .any(|frame| frame["method"] == "exit")
    );
    follower.close("following").unwrap();
}

#[test]
fn sleeping_one_viewer_preserves_another_and_waking_rejoins_the_same_live_chat() {
    let (model, _) = stand_in_model();
    let Some(chat) = Chat::open("shared-sleep", &model) else {
        return;
    };
    chat.turn("history before either viewer sleeps");
    let session = chat.wait("ready", |frame| frame["method"] == "ready")["params"]["session"]
        .as_str()
        .unwrap()
        .to_owned();
    let home = chat.root.join("home/serve");
    let backend = Backend::joined_to(
        std::fs::read_to_string(home.join("address")).unwrap(),
        &std::fs::read_to_string(home.join("token")).unwrap(),
    );
    let follower = LiveSessions::on(chat.sessions.inner.workspace.clone(), backend);
    let frames: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&frames);
    follower
        .open_to(
            Arc::new(move |frame| sink.lock().unwrap().push(frame)),
            "awake-viewer",
            Some(&session),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| frame["method"] == "session.presentation")
    {
        assert!(Instant::now() < deadline, "the other viewer never attached");
        std::thread::sleep(Duration::from_millis(10));
    }
    chat.sleep();
    let id = follower
        .request("awake-viewer", "session.settings", json!({}))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| frame["id"] == id && frame["result"].is_object())
    {
        assert!(
            Instant::now() < deadline,
            "sleep stopped the other viewer's chat"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Wake the sleeping desktop while the other viewer still holds this
    // incarnation. It must attach, not wait for or kill that viewer's chat.
    assert!(chat.call("session.settings", json!({}))["result"].is_object());
    chat.turn("continue after rejoining the same live incarnation");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| frame["params"]["kind"] == "turn.done")
    {
        assert!(
            Instant::now() < deadline,
            "the awake viewer did not see the new turn"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !frames
            .lock()
            .unwrap()
            .iter()
            .any(|frame| frame["method"] == "exit")
    );
    assert_eq!(chat.count(|frame| frame["method"] == "exit"), 0);
    follower.close("awake-viewer").unwrap();
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

#[test]
fn what_is_asked_before_the_chat_has_opened_is_answered_in_the_order_asked() {
    let Some(chat) = Chat::start("early", "http://127.0.0.1:9/v1") else {
        return;
    };
    let asked: Vec<u64> = (0..3)
        .map(|_| {
            chat.sessions
                .request(chat.key, "session.settings", json!({}))
                .unwrap()
        })
        .collect();
    chat.wait("the last answer", |frame| frame["id"] == asked[2]);
    let answered: Vec<Value> = chat
        .frames
        .lock()
        .unwrap()
        .iter()
        .filter(|frame| frame.get("result").is_some())
        .map(|frame| frame["id"].clone())
        .collect();
    assert_eq!(answered, asked);
}

/// The window's requests wait for their answers on the app's own runtime, more
/// of them at once than it has threads.
#[test]
fn answers_arrive_while_every_thread_of_the_apps_runtime_waits_for_one() {
    let Some(chat) = Chat::open("waits", "http://127.0.0.1:9/v1") else {
        return;
    };
    let connection = chat.sessions.inner.backend.connection().unwrap();
    let folder = chat.sessions.inner.workspace.clone();
    let (answered, answers) = std::sync::mpsc::channel();
    for _ in 0..64 {
        let (connection, folder, answered) =
            (Arc::clone(&connection), folder.clone(), answered.clone());
        tauri::async_runtime::spawn(async move {
            let _ = answered.send(connection.about(&folder, json!({ "method": "hello" })));
        });
    }
    for _ in 0..64 {
        let answer = answers
            .recv_timeout(Duration::from_secs(20))
            .expect("a request waited for an answer nobody was left to read");
        assert_eq!(answer.unwrap()["protocol_version"], 1);
    }
}

#[test]
fn a_backend_that_dies_stops_its_chats_and_the_next_one_resumes_them() {
    let (model, seen) = stand_in_model();
    let Some(chat) = Chat::open("dies", &model) else {
        return;
    };
    chat.turn("first");
    let before = *seen.lock().unwrap().last().unwrap();
    let ready = chat.wait("the greeting", |frame| frame["method"] == "ready");
    let session = ready["params"]["session"].as_str().unwrap().to_owned();

    chat.sessions.inner.backend.stop();
    chat.wait("the exit", |frame| frame["method"] == "exit");
    let refused = chat
        .sessions
        .request(chat.key, "session.settings", json!({}));
    assert_eq!(refused, Err(STOPPED.to_string()));

    chat.sessions
        .open_to(chat.emit(), chat.key, Some(&session))
        .unwrap();
    chat.turn("second");
    let after = *seen.lock().unwrap().last().unwrap();
    assert!(
        after >= before + 2,
        "the resumed chat lost its history: {before} -> {after}"
    );
}

#[test]
fn a_window_that_cannot_keep_up_stops_following_the_chat_and_holds_only_so_much() {
    // The chat here says more than a window may hold, all at once.
    let said = INBOX_FRAMES + 2000;
    let address = crate::backend::tests::address();
    let asked = crate::backend::tests::scripted_backend(&address, "token", Duration::ZERO, said);
    let sessions = LiveSessions::on(
        std::env::temp_dir(),
        Backend::joined_to(address.clone(), "token"),
    );
    let (drawn, stuck) = (
        Arc::<Mutex<Vec<Value>>>::default(),
        Arc::new(AtomicBool::new(true)),
    );
    let (sink, held) = (Arc::clone(&drawn), Arc::clone(&stuck));
    let emit: Emit = Arc::new(move |frame| {
        sink.lock().unwrap().push(frame);
        while held.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    sessions.open_to(emit, "behind", None).unwrap();

    let told = |asked: &[Value], method: &str| {
        asked
            .iter()
            .any(|frame| frame["method"] == method && frame["session"] == "late")
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !told(&asked.lock().unwrap(), "session.detach") {
        assert!(
            Instant::now() < deadline,
            "the window went on being sent a chat it had stopped following"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let refused = sessions.request("behind", "session.settings", json!({}));
    assert_eq!(refused, Err(STOPPED.to_string()));
    // Letting go is all it does: the chat is not this window's to stop for anyone else watching.
    for stops in ["session.close", "shutdown"] {
        assert!(
            !told(&asked.lock().unwrap(), stops),
            "a window that fell behind sent {stops} to a chat others may be watching"
        );
    }

    stuck.store(false, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(30);
    let exit = loop {
        let exit = drawn
            .lock()
            .unwrap()
            .iter()
            .find(|frame| frame["method"] == "exit")
            .cloned();
        if let Some(exit) = exit {
            break exit;
        }
        assert!(Instant::now() < deadline, "the window was never told");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(exit["params"]["stderr"], FELL_BEHIND);
    let held = drawn.lock().unwrap().len();
    assert!(
        held <= INBOX_FRAMES + 8,
        "the window held {held} of {said} frames"
    );
    if let Some(folder) = std::path::Path::new(&address)
        .parent()
        .filter(|_| cfg!(unix))
    {
        let _ = std::fs::remove_dir_all(folder);
    }
}

#[test]
fn a_request_too_large_to_send_is_refused_where_it_was_asked_and_no_chat_is_lost() {
    let (model, _) = stand_in_model();
    let Some(chat) = Chat::open("large", &model) else {
        return;
    };
    let beside: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&beside);
    let emit: Emit = Arc::new(move |frame| sink.lock().unwrap().push(frame));
    chat.sessions.open_to(emit, "beside", None).unwrap();

    // More than one frame may carry, as several attached images are once they are text.
    let images = vec!["x".repeat(wire::MAX_FRAME / 2 + 1); 2];
    let send = json!({ "content": "look", "images": images });
    let refused = chat
        .sessions
        .request(chat.key, "message.send", send.clone());
    assert_eq!(refused, Err(crate::backend::TOO_LARGE.to_string()));

    // The connection itself refuses one too, whoever hands it over.
    let connection = chat.sessions.inner.backend.connection().unwrap();
    let workspace = &chat.sessions.inner.workspace;
    let direct = connection
        .open_chat(workspace, None, false, None, Arc::new(|_| {}))
        .unwrap();
    let frame = json!({ "id": 1, "method": "message.send", "params": send });
    assert_eq!(
        connection.tell(&direct, frame),
        Err(crate::backend::TOO_LARGE.to_string())
    );

    let id = chat
        .sessions
        .request("beside", "session.settings", json!({}))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !beside.lock().unwrap().iter().any(|frame| frame["id"] == id) {
        assert!(
            Instant::now() < deadline,
            "the chat beside it stopped answering"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    chat.turn("after");
    let exits = |frames: &[Value]| frames.iter().any(|frame| frame["method"] == "exit");
    assert!(!exits(&beside.lock().unwrap()) && !exits(&chat.frames.lock().unwrap()));
}

#[test]
fn a_chat_holds_only_so_many_bytes_that_wait_to_be_sent() {
    // This backend is slow to start a chat, so what the chat is asked meanwhile waits here.
    let address = crate::backend::tests::address();
    let slow = Duration::from_secs(120);
    crate::backend::tests::scripted_backend(&address, "token", slow, 0);
    let sessions = LiveSessions::on(
        std::env::temp_dir(),
        Backend::joined_to(address.clone(), "token"),
    );
    sessions.open_to(Arc::new(|_| {}), "piled", None).unwrap();

    let each = OUTBOX_BYTES / 8;
    let send = || json!({ "content": "x".repeat(each) });
    let taken = (0..OUTBOX_FRAMES)
        .take_while(|_| sessions.request("piled", "message.send", send()).is_ok())
        .count();
    assert!(
        (1..8).contains(&taken),
        "{taken} requests of {each} bytes were held"
    );
    let refused = sessions.request("piled", "message.send", send());
    assert_eq!(refused, Err(WAITING.to_string()));
    // Something small still fits beside them.
    assert!(
        sessions
            .request("piled", "session.settings", json!({}))
            .is_ok()
    );
    if let Some(folder) = std::path::Path::new(&address)
        .parent()
        .filter(|_| cfg!(unix))
    {
        let _ = std::fs::remove_dir_all(folder);
    }
}

#[test]
fn a_chat_that_sleeps_keeps_no_thread_waiting_to_send_to_it() {
    let (model, _) = stand_in_model();
    let Some(chat) = Chat::open("idle", &model) else {
        return;
    };
    chat.turn("first");
    chat.sleep();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let open = chat.sessions.inner.open.lock().unwrap();
        let asleep = open.get(chat.key).expect("the tab is still open");
        // Its sender is told there is nobody listening once the thread that sent for it has gone.
        if matches!(
            asleep.requests.try_send((0, Value::Null)),
            Err(TrySendError::Disconnected(_))
        ) {
            break;
        }
        drop(open);
        assert!(
            Instant::now() < deadline,
            "the sleeping chat kept its thread"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    chat.turn("second");
}

#[test]
fn opening_a_chat_that_is_live_in_another_window_is_refused_and_leaves_it_running() {
    let (model, _) = stand_in_model();
    let Some(chat) = Chat::open("shared", &model) else {
        return;
    };
    chat.turn("first");
    let ready = chat.wait("the greeting", |frame| frame["method"] == "ready");
    let session = ready["params"]["session"].as_str().unwrap().to_owned();

    let second: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&second);
    chat.sessions
        .open_to(
            Arc::new(move |frame| sink.lock().unwrap().push(frame)),
            "second",
            Some(&session),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let refused = loop {
        let exit = second
            .lock()
            .unwrap()
            .iter()
            .find(|frame| frame["method"] == "exit")
            .cloned();
        if let Some(exit) = exit {
            break exit;
        }
        assert!(
            Instant::now() < deadline,
            "the second window was never told"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(refused["params"]["stderr"], crate::backend::OPEN_ELSEWHERE);

    chat.turn("second");
    assert_eq!(
        chat.count(|frame| frame["method"] == "exit"),
        0,
        "the first window's chat was stopped"
    );
}

#[test]
fn closing_a_chat_never_reaches_a_later_chat_resumed_under_its_id() {
    let (model, _) = stand_in_model();
    let Some(chat) = Chat::open("later", &model) else {
        return;
    };
    chat.turn("first");
    let ready = chat.wait("the greeting", |frame| frame["method"] == "ready");
    let session = ready["params"]["session"].as_str().unwrap().to_owned();
    chat.sessions.close(chat.key).unwrap();
    chat.wait("the exit", |frame| frame["method"] == "exit");

    let connection = chat.sessions.inner.backend.connection().unwrap();
    let heard = |frames: &Arc<Mutex<Vec<Value>>>| -> crate::backend::Hear {
        let frames = Arc::clone(frames);
        Arc::new(move |said| {
            let frame = match said {
                Said::Frame(frame) | Said::Event { frame, .. } => frame,
                Said::Ended(_) => json!({ "method": "ended" }),
            };
            frames.lock().unwrap().push(frame);
        })
    };
    let (earlier, later) = (Arc::default(), Arc::default());
    let workspace = &chat.sessions.inner.workspace;
    let first = connection
        .open_chat(workspace, Some(&session), false, None, heard(&earlier))
        .unwrap();
    connection
        .tell(&first, json!({ "method": "session.close" }))
        .unwrap();
    let second = connection
        .open_chat(workspace, Some(&session), false, None, heard(&later))
        .unwrap();

    let shutdown = json!({ "id": 0, "method": "shutdown" });
    assert!(
        connection.tell(&first, shutdown).is_err(),
        "a chat that is over was sent a request under an id another chat now has"
    );
    connection
        .tell(
            &second,
            json!({ "id": 7, "method": "session.settings", "params": {} }),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !later.lock().unwrap().iter().any(|frame| frame["id"] == 7) {
        assert!(Instant::now() < deadline, "the later chat never answered");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !later
            .lock()
            .unwrap()
            .iter()
            .any(|frame| frame["method"] == "ended")
    );
}
#[test]
fn controls_pass_a_full_outbox_and_exhausted_ordinary_reply_slots() {
    let address = crate::backend::tests::address();
    let asked = crate::backend::tests::scripted_backend(&address, "token", Duration::ZERO, 0);
    let backend = Backend::joined_to(address.clone(), "token");
    let sessions = LiveSessions::on(std::env::temp_dir(), backend.clone());
    sessions.open_to(Arc::new(|_| {}), "control", None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !asked
        .lock()
        .unwrap()
        .iter()
        .any(|frame| frame["method"] == "session.attach")
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let connection = backend.connection().unwrap();
    crate::backend::tests::exhaust_ordinary_replies(&connection);
    let mut full = false;
    for _ in 0..OUTBOX_FRAMES + 2 {
        if sessions
            .request("control", "session.settings", json!({}))
            .is_err()
        {
            full = true;
            break;
        }
    }
    assert!(
        full,
        "ordinary outbox admission must stay bounded during retries"
    );
    let controls = [
        "cancel",
        "approval.respond",
        "question.respond",
        "agent.control",
    ];
    for method in controls {
        sessions.request("control", method, json!({})).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while !controls.iter().all(|method| {
        asked
            .lock()
            .unwrap()
            .iter()
            .any(|frame| frame["method"] == *method)
    }) {
        assert!(
            Instant::now() < deadline,
            "a control could not pass ordinary congestion"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    crate::backend::tests::disconnect(&connection);
    sessions.close("control").unwrap();
    if let Some(folder) = std::path::Path::new(&address)
        .parent()
        .filter(|_| cfg!(unix))
    {
        let _ = std::fs::remove_dir_all(folder);
    }
}
