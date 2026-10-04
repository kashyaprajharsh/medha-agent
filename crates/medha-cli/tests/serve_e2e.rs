//! `medha serve` as its clients meet it: many chats in one process, two clients
//! on one chat, a message sent mid-turn, an approval answered once, and a chat
//! that outlives the backend that was running it.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

const SECRET: &str = "sk-zz-never-on-the-wire";
const WAIT: Duration = Duration::from_secs(60);

fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0u8; 8192];
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&chunk[..read]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                let line = headers.lines().next().unwrap_or_default().to_string();
                return Some((line, request[end + 4..end + 4 + length].to_vec()));
            }
        }
    }
}

fn respond(stream: &mut TcpStream, kind: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn last_user(body: &Value) -> String {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|message| message["role"] == "user")
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn user_texts(body: &Value) -> String {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Echoes the last user message. One containing HOLD is answered only once
/// released; one containing RUN is answered with a shell command first.
fn answer(mut stream: TcpStream, seen: mpsc::Sender<Value>, release: &Mutex<mpsc::Receiver<()>>) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let Some((line, body)) = read_request(&mut stream) else {
        return;
    };
    if !line.starts_with("post /v1/chat/completions") {
        let models = r#"{"data":[{"id":"test-model","context_length":128000}]}"#;
        return respond(&mut stream, "application/json", models);
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let asked = last_user(&body);
    let ran = body["messages"]
        .as_array()
        .is_some_and(|all| all.iter().any(|message| message["role"] == "tool"));
    let _ = seen.send(body.clone());
    if asked.contains("HOLD") {
        let _ = release.lock().unwrap().recv_timeout(WAIT);
    }
    let events = if asked.contains("RUN") && !ran {
        let shell = body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .find(|name| name.contains("shell") && name.contains("exec"))
            .unwrap_or("shell_exec");
        let arguments = json!({"command": "touch made-by-the-test.txt", "network": false});
        let call = json!({"index": 0, "id": "call_1", "type": "function",
            "function": {"name": shell, "arguments": arguments.to_string()}});
        vec![
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [call]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]
    } else {
        let text = format!("echo: {asked}");
        vec![
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        ]
    };
    let mut stream_body = String::new();
    for event in events {
        stream_body.push_str(&format!("data: {event}\n\n"));
    }
    stream_body.push_str("data: [DONE]\n\n");
    respond(&mut stream, "text/event-stream", &stream_body);
}

struct Provider {
    url: String,
    seen: mpsc::Receiver<Value>,
    release: mpsc::Sender<()>,
}

impl Provider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (seen_tx, seen) = mpsc::channel();
        let (release, held) = mpsc::channel::<()>();
        let held = Arc::new(Mutex::new(held));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (seen, held) = (seen_tx.clone(), held.clone());
                std::thread::spawn(move || answer(stream, seen, &held));
            }
        });
        Self { url, seen, release }
    }

    /// The next request the model was sent, as its user messages joined.
    fn asked(&self) -> String {
        user_texts(
            &self
                .seen
                .recv_timeout(WAIT)
                .expect("no model request arrived"),
        )
    }
}

struct Backend {
    child: Child,
    home: PathBuf,
}

impl Backend {
    fn start(home: &Path, provider: &Provider) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_medha"))
            .arg("serve")
            .env("MEDHA_HOME", home)
            .env("MEDHA_BASE_URL", &provider.url)
            .env("MEDHA_MODEL", "test-model")
            .env("MEDHA_API_KEY", SECRET)
            .env("MEDHA_PROTOCOL", "open-ai-chat")
            .env("MEDHA_TOKEN_ACCOUNTING", "adaptive")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            child,
            home: home.to_path_buf(),
        }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.home.join("serve").join(name)
    }

    async fn connect(&self) -> Client {
        let deadline = Instant::now() + WAIT;
        loop {
            let found = (
                std::fs::read_to_string(self.file("address")),
                std::fs::read_to_string(self.file("token")),
            );
            if let (Ok(address), Ok(token)) = found
                && let Ok(stream) = wire::connect(&address).await
            {
                return Client::meet(stream, &token).await;
            }
            assert!(Instant::now() < deadline, "the backend never listened");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    reader: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    events: std::collections::VecDeque<Value>,
    /// Every frame this client was ever sent, as text.
    heard: String,
    asked: u64,
}

const ROLES: wire::Roles = wire::Roles {
    host: "backend",
    guest: "client",
};

impl Client {
    async fn meet<S: AsyncRead + AsyncWrite + Send + 'static>(stream: S, token: &str) -> Self {
        let (read, write) = tokio::io::split(stream);
        let mut reader: BufReader<Box<dyn AsyncRead + Unpin + Send>> =
            BufReader::new(Box::new(read));
        let mut writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(write);
        let welcome = wire::greet(&mut reader, &mut writer, token, ROLES)
            .await
            .expect("the backend did not admit a client holding its token");
        assert_eq!(welcome["result"]["protocol"], 1);
        Self {
            reader,
            writer,
            events: Default::default(),
            heard: String::new(),
            asked: 0,
        }
    }

    async fn frame(&mut self) -> Value {
        let frame = tokio::time::timeout(WAIT, wire::read_frame(&mut self.reader))
            .await
            .expect("the backend went quiet")
            .expect("the backend closed the connection");
        self.heard.push_str(&frame.to_string());
        frame
    }

    async fn ask(&mut self, method: &str, session: Option<&str>, params: Value) -> Value {
        self.asked += 1;
        let id = self.asked;
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Some(session) = session {
            request["session"] = json!(session);
        }
        assert!(wire::write_frame(&mut self.writer, &request).await);
        loop {
            let frame = self.frame().await;
            if frame["id"] == json!(id) {
                return frame;
            }
            self.events.push_back(frame);
        }
    }

    /// The next frame a chat wrote, with its number.
    async fn event(&mut self) -> (u64, Value) {
        let frame = match self.events.pop_front() {
            Some(frame) => frame,
            None => self.frame().await,
        };
        assert_eq!(frame["method"], "session.event", "{frame}");
        (
            frame["params"]["seq"].as_u64().unwrap(),
            frame["params"]["frame"].clone(),
        )
    }

    async fn until(&mut self, wanted: impl Fn(&Value) -> bool) -> (u64, Value) {
        loop {
            let (seq, frame) = self.event().await;
            if wanted(&frame) {
                return (seq, frame);
            }
        }
    }

    async fn create(&mut self, folder: &Path, resume: Option<&str>) -> Value {
        let mut params = json!({"folder": folder});
        if let Some(id) = resume {
            params["resume"] = json!(id);
        }
        self.ask("session.create", None, params).await
    }

    async fn open(&mut self, folder: &Path) -> String {
        let made = self.create(folder, None).await;
        let session = made["result"]["session"]
            .as_str()
            .unwrap_or_else(|| panic!("no chat was created: {made}"))
            .to_string();
        let attached = self
            .ask("session.attach", Some(&session), json!({"after": 0}))
            .await;
        assert_eq!(attached["result"]["gap"], false, "{attached}");
        session
    }

    async fn send(&mut self, session: &str, content: &str) -> Value {
        self.ask("message.send", Some(session), json!({"content": content}))
            .await
    }
}

fn kind(frame: &Value, wanted: &str) -> bool {
    frame["params"]["kind"] == wanted
}

struct World {
    root: tempfile::TempDir,
    provider: Provider,
}

impl World {
    fn new() -> Self {
        Self {
            root: tempfile::Builder::new().prefix("ms").tempdir().unwrap(),
            provider: Provider::start(),
        }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn folder(&self, name: &str) -> PathBuf {
        let folder = self.root.path().join(name);
        std::fs::create_dir_all(&folder).unwrap();
        folder.canonicalize().unwrap()
    }

    fn backend(&self) -> Backend {
        std::fs::create_dir_all(self.home()).unwrap();
        Backend::start(&self.home(), &self.provider)
    }
}

#[cfg(unix)]
fn resident_mb(pid: u32) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap()
        / 1024
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_chats_in_one_process_answer_their_own_clients_and_leak_no_key() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folders = [world.folder("one"), world.folder("two")];

    let mut chats = Vec::new();
    for n in 0..10 {
        chats.push(client.open(&folders[n % 2]).await);
    }
    for (n, chat) in chats.iter().enumerate() {
        let sent = client.send(chat, &format!("from chat {n}")).await;
        assert_eq!(sent["result"]["accepted"], true, "{sent}");
    }
    let mut done = std::collections::HashSet::new();
    let mut replies = std::collections::HashMap::<String, String>::new();
    while done.len() < chats.len() {
        let frame = client.frame().await;
        let (chat, said) = (
            frame["params"]["session"].as_str(),
            &frame["params"]["frame"],
        );
        let Some(chat) = chat.map(str::to_owned) else {
            continue;
        };
        if kind(said, "model.text") {
            let delta = said["params"]["delta"].as_str().unwrap_or_default();
            replies.entry(chat).or_default().push_str(delta);
        } else if kind(said, "turn.done") {
            done.insert(chat);
        }
    }
    for (n, chat) in chats.iter().enumerate() {
        assert_eq!(
            replies[chat],
            format!("echo: from chat {n}"),
            "a reply crossed chats"
        );
    }
    let mut asked: Vec<String> = (0..10).map(|_| world.provider.asked()).collect();
    asked.sort();
    let expected: Vec<String> = (0..10).map(|n| format!("\"from chat {n}\"")).collect();
    assert_eq!(
        asked, expected,
        "each chat asked the model its own message, once"
    );

    let listed = client.ask("session.list", None, json!({})).await;
    assert_eq!(listed["result"]["sessions"].as_array().unwrap().len(), 10);
    #[cfg(unix)]
    {
        let used = resident_mb(backend.child.id());
        assert!(used < 250, "ten chats took {used} MB");
    }

    // A client that arrives late is replayed the chat from its start.
    let mut late = backend.connect().await;
    let attached = late
        .ask("session.attach", Some(&chats[0]), json!({"after": 0}))
        .await;
    assert_eq!(attached["result"]["gap"], false);
    assert_eq!(late.event().await.1["method"], "ready");
    late.until(|frame| kind(frame, "turn.done")).await;

    let log = std::fs::read_to_string(world.home().join("logs").join("serve.log")).unwrap();
    for (what, text) in [
        ("a client", &client.heard),
        ("a late client", &late.heard),
        ("the log", &log),
    ] {
        assert!(!text.contains(SECRET), "the key reached {what}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_sent_mid_turn_is_steered_whichever_client_sends_it() {
    let world = World::new();
    let backend = world.backend();
    let (mut first, mut second) = (backend.connect().await, backend.connect().await);
    let chat = first.open(&world.folder("w")).await;
    second.ask("session.attach", Some(&chat), json!({})).await;

    first.send(&chat, "HOLD the first task").await;
    assert_eq!(world.provider.asked(), "\"HOLD the first task\"");

    let steered = second.send(&chat, "and this, from the other window").await;
    assert_eq!(
        steered["result"],
        json!({"accepted": true, "steered": true})
    );
    let image = json!({"content": "look", "images": [{"path": "/nowhere.png"}]});
    let refused = second.ask("message.send", Some(&chat), image).await;
    assert!(refused["error"].is_object(), "{refused}");
    assert!(
        world
            .provider
            .seen
            .recv_timeout(Duration::from_millis(500))
            .is_err(),
        "a second turn started beside the running one"
    );

    world.provider.release.send(()).unwrap();
    let next = world.provider.asked();
    assert!(
        next.contains("HOLD the first task") && next.contains("and this, from the other window"),
        "the steered message joined the same conversation: {next}"
    );
    world.provider.release.send(()).unwrap();
    for client in [&mut first, &mut second] {
        client.until(|frame| kind(frame, "turn.done")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_is_shown_to_both_clients_and_answered_once() {
    let world = World::new();
    let backend = world.backend();
    let (mut first, mut second) = (backend.connect().await, backend.connect().await);
    let folder = world.folder("w");
    let chat = first.open(&folder).await;
    second.ask("session.attach", Some(&chat), json!({})).await;

    first.send(&chat, "RUN the command").await;
    let (seq, asked) = first.until(|frame| frame["method"] == "approval").await;
    let (same_seq, same) = second.until(|frame| frame["method"] == "approval").await;
    assert_eq!((seq, &asked), (same_seq, &same));
    let gate = asked["params"]["gate_id"].clone();

    let answer = json!({"gate_id": gate, "approve": true});
    let accepted = second
        .ask("approval.respond", Some(&chat), answer.clone())
        .await;
    assert_eq!(accepted["result"]["accepted"], true, "{accepted}");
    let again = first.ask("approval.respond", Some(&chat), answer).await;
    assert_eq!(again["error"]["message"], "approval is not pending");

    for client in [&mut first, &mut second] {
        let (_, settled) = client
            .until(|frame| frame["method"] == "approval.resolved")
            .await;
        assert_eq!(
            settled["params"],
            json!({"gate_id": gate, "approved": true})
        );
        client.until(|frame| kind(frame, "turn.done")).await;
    }
    assert!(
        folder.join("made-by-the-test.txt").exists(),
        "the approved command did not run"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_outlives_the_backend_that_was_running_it() {
    let world = World::new();
    let folder = world.folder("w");
    let mut backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    client.send(&chat, "remember the word heron").await;
    client.until(|frame| kind(frame, "turn.done")).await;
    world.provider.asked();

    let second = Command::new(env!("CARGO_BIN_EXE_medha"))
        .arg("serve")
        .env("MEDHA_HOME", world.home())
        .output()
        .unwrap();
    assert!(
        !second.status.success(),
        "a second backend started beside the first"
    );
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));

    client.send(&chat, "HOLD while the backend dies").await;
    world.provider.asked();
    backend.child.kill().unwrap();
    backend.child.wait().unwrap();
    world.provider.release.send(()).unwrap();
    drop(backend);

    let backend = world.backend();
    let mut client = backend.connect().await;
    let resumed = client.create(&folder, Some(&chat)).await;
    assert_eq!(resumed["result"]["session"], chat.as_str(), "{resumed}");
    client
        .ask("session.attach", Some(&chat), json!({"after": 0}))
        .await;
    client.send(&chat, "what was the word").await;
    client.until(|frame| kind(frame, "turn.done")).await;
    let asked = world.provider.asked();
    assert!(
        asked.contains("remember the word heron") && asked.contains("what was the word"),
        "the resumed chat lost its history: {asked}"
    );
}
