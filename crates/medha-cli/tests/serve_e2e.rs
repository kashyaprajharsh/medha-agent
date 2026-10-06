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

/// The binary under test, with the home and the model every process here shares.
fn medha(home: &Path, provider: &Provider) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_medha"));
    command
        .env("MEDHA_HOME", home)
        .env("MEDHA_BASE_URL", &provider.url)
        .env("MEDHA_MODEL", "test-model")
        .env("MEDHA_API_KEY", SECRET)
        .env("MEDHA_PROTOCOL", "open-ai-chat")
        .env("MEDHA_TOKEN_ACCOUNTING", "adaptive")
        // These tests read the log, so what is written there is theirs to say.
        .env("RUST_LOG", "info")
        // Saved keys go to a file under the home, never to this machine's keychain.
        .env("MEDHA_CRED_STORE", "file");
    command
}

/// `medha --acp`, one chat in a process of its own, as an editor runs it.
struct OwnProcess {
    child: Child,
    input: std::process::ChildStdin,
    output: std::io::BufReader<std::process::ChildStdout>,
    asked: u64,
}

impl OwnProcess {
    fn start(folder: &Path, home: &Path, provider: &Provider, env: &[(&str, &str)]) -> Self {
        let mut child = medha(home, provider)
            .arg("--acp")
            .envs(env.iter().copied())
            .current_dir(folder)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut chat = Self {
            child,
            input,
            output,
            asked: 0,
        };
        while chat.frame()["method"] != "ready" {}
        chat
    }

    fn frame(&mut self) -> Value {
        use std::io::BufRead;
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("a frame from the chat")
    }

    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.asked += 1;
        let id = self.asked;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.input, "{request}").unwrap();
        loop {
            let frame = self.frame();
            if frame["id"] == json!(id) {
                return frame;
            }
        }
    }
}

impl Drop for OwnProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What a chat answered, whichever way it was reached: its result, or its error.
fn outcome(reply: &Value) -> Value {
    match reply.get("error") {
        Some(error) => json!({"error": error["message"]}),
        None => reply["result"].clone(),
    }
}

struct Backend {
    child: Child,
    home: PathBuf,
}

impl Backend {
    /// A name given no value is taken out of the backend's environment.
    fn start(home: &Path, provider: &Provider, env: &[(&str, &str)]) -> Self {
        let mut command = medha(home, provider);
        for (name, value) in env {
            match value.is_empty() {
                true => command.env_remove(name),
                false => command.env(name, value),
            };
        }
        let child = command
            .arg("serve")
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

    /// The next frame in the order it arrived: one set aside while an answer
    /// was awaited comes before anything still on the wire.
    async fn next(&mut self) -> Value {
        match self.events.pop_front() {
            Some(frame) => frame,
            None => self.frame().await,
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
        let frame = self.next().await;
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

    /// The same request the desktop service takes, asked of the backend about `folder`.
    async fn about(&mut self, folder: &Path, mut request: Value) -> Result<Value, String> {
        self.asked += 1;
        let id = self.asked;
        request["id"] = json!(id);
        request["folder"] = json!(folder);
        assert!(wire::write_frame(&mut self.writer, &request).await);
        loop {
            let frame = self.frame().await;
            if frame["id"] != json!(id) {
                self.events.push_back(frame);
                continue;
            }
            return match frame["error"]["message"].as_str() {
                Some(error) => Err(error.to_string()),
                None => Ok(frame["result"].clone()),
            };
        }
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
        self.backend_in(&[])
    }

    /// A backend whose own environment asks something of every chat it starts.
    fn backend_in(&self, env: &[(&str, &str)]) -> Backend {
        std::fs::create_dir_all(self.home()).unwrap();
        Backend::start(&self.home(), &self.provider, env)
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
        let frame = client.next().await;
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

    let log: String = std::fs::read_dir(world.home().join("logs"))
        .unwrap()
        .map(|file| std::fs::read_to_string(file.unwrap().path()).unwrap())
        .collect();
    assert!(log.contains("medha session start"), "the log was not found");
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

/// An app started from the desktop hands its children a low limit on open files.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_started_with_few_open_files_allowed_still_holds_many_chats() {
    let world = World::new();
    std::fs::create_dir_all(world.home()).unwrap();
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", r#"ulimit -S -n 64; exec "$0" serve"#])
        .arg(env!("CARGO_BIN_EXE_medha"));
    for (key, value) in medha(&world.home(), &world.provider).get_envs() {
        command.env(key, value.unwrap());
    }
    let backend = Backend {
        child: command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        home: world.home(),
    };
    let mut client = backend.connect().await;
    for n in 0..12 {
        let made = client.create(&world.folder(&format!("w{n}")), None).await;
        assert!(made["result"]["session"].is_string(), "chat {n}: {made}");
    }
}

/// A socket's path may be only about a hundred bytes; a home may be far deeper.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_whose_home_is_too_deep_for_a_socket_is_still_reached() {
    let world = World::new();
    let home = world.root.path().join("d".repeat(120)).join("home");
    std::fs::create_dir_all(&home).unwrap();
    let backend = Backend::start(&home, &world.provider, &[]);
    let mut client = backend.connect().await;
    let hello = client.ask("hello", None, json!({})).await;
    assert_eq!(hello["result"]["protocol"], 1);
    let chat = client.open(&world.folder("w")).await;
    let settings = client.ask("session.settings", Some(&chat), json!({})).await;
    assert!(settings.get("result").is_some(), "{settings}");
}

/// What a client reads to find a backend must not outlive the backend it names.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_told_to_stop_leaves_nothing_that_names_it() {
    let world = World::new();
    let mut backend = world.backend();
    drop(backend.connect().await);
    let deadline = Instant::now() + WAIT;
    while !backend.file("mcp.sock").exists() {
        assert!(Instant::now() < deadline, "the MCP host never listened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let told = Command::new("kill")
        .args(["-TERM", &backend.child.id().to_string()])
        .status()
        .unwrap();
    assert!(told.success());
    let stopped = loop {
        if let Some(status) = backend.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the backend ignored the signal");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(
        stopped.success(),
        "the backend was killed, not stopped: {stopped}"
    );

    let mut left: Vec<String> = std::fs::read_dir(world.home().join("serve"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["lock"], "a stopped backend left its address behind");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_backend_answers_what_the_desktop_asks_about_a_folder() {
    let world = World::new();
    let folder = world.folder("w");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    client.send(&chat, "one turn of history").await;
    client.until(|frame| kind(frame, "turn.done")).await;

    let profile = json!({"protocol": "open-ai-chat", "base_url": "http://127.0.0.1:9/v1",
        "model": "saved-here", "auth": "none", "max_ctx": 32768});
    let saved = json!({"method": "settings.model.save",
        "params": {"name": "saved-here", "profile": profile, "default": true}});
    client
        .about(&folder, saved)
        .await
        .expect("the backend saves a setting");
    let noted = json!({"method": "instructions.save",
        "params": {"kind": "agents", "before": "", "content": "Written through the backend."}});
    client
        .about(&folder, noted)
        .await
        .expect("the backend saves instructions");
    let written = std::fs::read_to_string(folder.join("AGENTS.md")).unwrap();
    assert_eq!(written, "Written through the backend.");

    let answered = [
        json!({"method": "sessions.list"}),
        json!({"method": "sessions.events", "session_id": chat}),
        json!({"method": "sessions.changes", "session_id": chat}),
        json!({"method": "usage.summary", "params": {"days": 7}}),
        json!({"method": "library.sessions"}),
        json!({"method": "settings.defaults"}),
        json!({"method": "settings.list"}),
        json!({"method": "settings.keys"}),
        json!({"method": "settings.tools"}),
        json!({"method": "instructions.list"}),
        json!({"method": "extensions.list"}),
        json!({"method": "extensions.hooks.list"}),
        json!({"method": "extensions.marketplace.list"}),
        json!({"method": "extensions.connectors"}),
    ];
    for request in answered {
        let answer = client.about(&folder, request.clone()).await;
        assert!(answer.is_ok(), "{request}: {answer:?}");
    }
    let refused = [
        json!({"method": "sessions.events"}),
        json!({"method": "extensions.nonsense"}),
        json!({"method": "nonsense"}),
    ];
    for request in refused {
        let answer = client.about(&folder, request.clone()).await;
        assert!(answer.is_err(), "{request}: {answer:?}");
    }
    let listed = client
        .about(&folder, json!({"method": "sessions.list"}))
        .await
        .unwrap();
    assert!(listed.to_string().contains(&chat), "the chat is not listed");
    let page = json!({"method": "sessions.events", "session_id": chat, "limit": 2});
    let page = client.about(&folder, page).await.unwrap();
    let next = json!({"method": "sessions.events", "session_id": chat,
        "cursor": page["next_cursor"]});
    let next = client.about(&folder, next).await.unwrap();
    assert_ne!(next, page, "the second page repeated the first");
    let defaults = client
        .about(&folder, json!({"method": "settings.defaults"}))
        .await;
    assert_eq!(defaults.unwrap()["profile"], "saved-here");
    assert!(!client.heard.contains(SECRET), "a key reached a client");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_in_the_backend_answers_its_own_requests_as_one_in_its_own_process_does() {
    let world = World::new();
    let folder = world.folder("w");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    let mut alone = OwnProcess::start(&folder, &world.home(), &world.provider, &[]);

    let requests = [
        ("hello", json!({})),
        ("session.settings", json!({})),
        ("session.configure", json!({"mode": "plan"})),
        ("session.settings", json!({})),
        ("session.configure", json!({"mode": "no-such-mode"})),
        ("session.rewind.points", json!({})),
        ("patch.list", json!({})),
        (
            "agent.control",
            json!({"agent": "nobody", "action": "stop"}),
        ),
        ("memory.list", json!({})),
        ("memory.provenance", json!({"id": "nothing"})),
        ("tasks.list", json!({})),
        ("extensions.catalog", json!({})),
        ("extensions.reload", json!({})),
        ("mcp.screens", json!({})),
        ("mcp.disconnect", json!({"server": "none"})),
        ("mcp.connect", json!({"server": "none"})),
        (
            "question.respond",
            json!({"question_id": 1, "dismiss": true}),
        ),
        ("approval.respond", json!({"gate_id": 9, "approve": true})),
        ("interrupt", json!({})),
        ("no.such.method", json!({})),
    ];
    for (method, params) in requests {
        let in_backend = outcome(&client.ask(method, Some(&chat), params.clone()).await);
        let in_process = outcome(&alone.ask(method, params));
        assert_eq!(in_backend, in_process, "{method}");
    }
}

/// A chat reads its saved key as it starts, and that waits on the same lock.
/// Chats starting together must not take the threads every client is served on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chats_waiting_on_a_held_lock_as_they_start_hold_up_nobody_else() {
    let world = World::new();
    let folder = world.folder("w");
    let serving = [("TOKIO_WORKER_THREADS", "2"), ("MEDHA_API_KEY", "")];
    let backend = world.backend_in(&serving);
    let mut client = backend.connect().await;
    let profile = json!({"protocol": "open-ai-chat", "base_url": world.provider.url,
        "model": "test-model", "auth": "bearer", "max_ctx": 32768});
    let save = json!({"method": "settings.model.save",
        "params": {"name": "saved", "profile": profile, "default": true, "key": SECRET}});
    client
        .about(&folder, save)
        .await
        .expect("a model is saved with its key");
    // The saved key is what a chat here runs on.
    let chat = client.open(&folder).await;
    client.send(&chat, "does the saved key work").await;
    client.until(|frame| kind(frame, "turn.done")).await;
    world.provider.asked();
    let mut other = backend.connect().await;

    let held = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(world.home().join("credentials.lock"))
        .unwrap();
    held.lock().unwrap();
    let mut starting = Vec::new();
    for id in 9201..9204 {
        let create = json!({"id": id, "method": "session.create",
            "params": {"folder": world.folder(&format!("w{id}"))}});
        assert!(wire::write_frame(&mut client.writer, &create).await);
        starting.push(id);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let hello = other.ask("hello", None, json!({})).await;
    assert_eq!(hello["result"]["protocol"], 1);
    let listed = other.ask("session.list", None, json!({})).await;
    assert_eq!(
        listed["result"]["sessions"].as_array().map(Vec::len),
        Some(1),
        "a chat started without reading its key: {listed}"
    );
    let tasks = client.ask("tasks.list", Some(&chat), json!({})).await;
    assert!(tasks.get("result").is_some(), "{tasks}");

    held.unlock().unwrap();
    while !starting.is_empty() {
        let frame = client.next().await;
        if let Some(id) = frame["id"].as_u64() {
            assert!(frame["result"]["session"].is_string(), "{frame}");
            starting.retain(|waited| *waited != id);
        }
    }
}

/// Saving a key waits on a lock another program may hold. While it waits, the
/// backend still serves its other clients, its chats and other folder requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_waiting_on_a_held_lock_hold_up_nobody_else() {
    let world = World::new();
    let folder = world.folder("w");
    // Two threads serve everyone, so more than two waiters would once have taken them all.
    let backend = world.backend_in(&[("TOKIO_WORKER_THREADS", "2")]);
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    let mut other = backend.connect().await;

    let held = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(world.home().join("credentials.lock"))
        .unwrap();
    held.lock().unwrap();
    let mut waiting = Vec::new();
    for id in 9001..9005 {
        let save = json!({"id": id, "folder": folder, "method": "settings.keys.set",
            "params": {"group": "search", "id": "tavily", "key": "sk-zz-waits"}});
        assert!(wire::write_frame(&mut client.writer, &save).await);
        waiting.push(id);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let hello = other.ask("hello", None, json!({})).await;
    assert_eq!(hello["result"]["protocol"], 1);
    let settings = client.ask("session.settings", Some(&chat), json!({})).await;
    assert!(settings.get("result").is_some(), "{settings}");
    let listed = other
        .about(&folder, json!({"method": "sessions.list"}))
        .await;
    assert!(listed.is_ok(), "{listed:?}");

    held.unlock().unwrap();
    while !waiting.is_empty() {
        let frame = client.next().await;
        if let Some(id) = frame["id"].as_u64() {
            assert!(frame.get("result").is_some(), "{frame}");
            waiting.retain(|waited| *waited != id);
        }
    }
}

/// The shared MCP host reads keys as it follows the configuration, and a
/// running chat reads one when its model is changed. Each waits on that lock
/// on a thread that others are served on; none of them may keep it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waits_on_a_held_lock_by_the_mcp_host_and_by_running_chats_hold_up_nobody_else() {
    let world = World::new();
    let folder = world.folder("w");
    // One thread serves every client and the MCP host as well.
    let serving = [("TOKIO_WORKER_THREADS", "1"), ("MEDHA_API_KEY", "")];
    let backend = world.backend_in(&serving);
    let mut client = backend.connect().await;
    let profile = json!({"protocol": "open-ai-chat", "base_url": world.provider.url,
        "model": "test-model", "auth": "bearer", "max_ctx": 32768});
    let save = json!({"method": "settings.model.save",
        "params": {"name": "saved", "profile": profile, "default": true, "key": SECRET}});
    client
        .about(&folder, save)
        .await
        .expect("a model is saved with its key");
    // As many chats as there are threads for chats to run on, and one beside them.
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    let mut changing = Vec::new();
    for _ in 0..threads.max(4) {
        changing.push(client.open(&folder).await);
    }
    let beside = client.open(&folder).await;
    let mut other = backend.connect().await;

    let held = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(world.home().join("credentials.lock"))
        .unwrap();
    held.lock().unwrap();
    let config = world.home().join("config.toml");
    let mut servers = std::fs::read_to_string(&config).unwrap();
    for n in 0..2 {
        let url = format!("http://127.0.0.1:9/mcp{n}");
        servers += &format!("\n[mcp.held{n}]\nurl = \"{url}\"\ntrust = \"trusted\"\n");
    }
    servers +=
        "\n[mcp.keyed]\nurl = \"http://127.0.0.1:9/k\"\ntrust = \"trusted\"\nauth = \"bearer\"\n";
    std::fs::write(&config, servers).unwrap();
    let mut waiting = Vec::new();
    for (n, chat) in changing.iter().enumerate() {
        let id = 9300 + n as u64;
        let change = json!({"jsonrpc": "2.0", "id": id, "method": "session.configure",
            "session": chat, "params": {"profile": "saved"}});
        assert!(wire::write_frame(&mut client.writer, &change).await);
        waiting.push(id);
    }
    // Longer than the host goes between looks at the configuration.
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let hello = other.ask("hello", None, json!({})).await;
    assert_eq!(hello["result"]["protocol"], 1);
    let tasks = client.ask("tasks.list", Some(&beside), json!({})).await;
    assert!(tasks.get("result").is_some(), "{tasks}");
    let early: Vec<&Value> = client
        .events
        .iter()
        .filter(|frame| !frame["id"].is_null())
        .collect();
    assert!(
        early.is_empty(),
        "a change of model did not wait for the key: {early:?}"
    );

    held.unlock().unwrap();
    while !waiting.is_empty() {
        let frame = client.next().await;
        if let Some(id) = frame["id"].as_u64() {
            assert!(frame.get("result").is_some(), "{frame}");
            waiting.retain(|waited| *waited != id);
        }
    }
}

/// The names of the tools one model request offered, in order.
fn offered(request: &Value) -> Vec<String> {
    let mut names: Vec<String> = request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_in_the_backend_obeys_the_environment_as_one_in_its_own_process_does() {
    let env = [
        ("MEDHA_MODE", "plan"),
        ("MEDHA_TOOLS", "minimal"),
        ("MEDHA_MAX_PARALLEL_TOOLS", "1"),
    ];
    let world = World::new();
    let folder = world.folder("w");
    let backend = world.backend_in(&env);
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    let mut alone = OwnProcess::start(&folder, &world.home(), &world.provider, &env);

    let in_backend = outcome(&client.ask("session.settings", Some(&chat), json!({})).await);
    assert_eq!(in_backend["mode"], "plan", "{in_backend}");
    assert_eq!(
        in_backend,
        outcome(&alone.ask("session.settings", json!({})))
    );

    client.send(&chat, "what may you use").await;
    let from_backend = offered(&world.provider.seen.recv_timeout(WAIT).unwrap());
    client.until(|frame| kind(frame, "turn.done")).await;
    alone.ask("message.send", json!({"content": "what may you use"}));
    let from_process = offered(&world.provider.seen.recv_timeout(WAIT).unwrap());
    assert!(!from_backend.is_empty(), "the model was offered no tools");
    assert_eq!(from_backend, from_process, "the tools offered differ");

    // What a client chooses for its chat wins over the environment, as a flag does.
    let chosen = json!({"folder": world.folder("chosen"), "mode": "careful"});
    let made = client.ask("session.create", None, chosen).await;
    let own = made["result"]["session"].as_str().unwrap().to_string();
    client
        .ask("session.attach", Some(&own), json!({"after": 0}))
        .await;
    let settings = outcome(&client.ask("session.settings", Some(&own), json!({})).await);
    assert_eq!(settings["mode"], "careful", "{settings}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_setting_that_makes_no_sense_stops_only_what_would_have_used_it() {
    let env = [("MEDHA_MODE", "no-such-mode")];
    let world = World::new();
    let folder = world.folder("w");
    let backend = world.backend_in(&env);
    let mut client = backend.connect().await;

    let left_to_it = client.create(&folder, None).await;
    let refused = left_to_it["error"]["message"].as_str().unwrap_or_default();
    assert!(refused.contains("no-such-mode"), "{left_to_it}");

    let chosen = json!({"folder": folder, "mode": "plan"});
    let made = client.ask("session.create", None, chosen).await;
    let chat = made["result"]["session"]
        .as_str()
        .unwrap_or_else(|| panic!("a chat that chose its own mode was refused: {made}"))
        .to_string();
    client
        .ask("session.attach", Some(&chat), json!({"after": 0}))
        .await;
    let settings = outcome(&client.ask("session.settings", Some(&chat), json!({})).await);
    assert_eq!(settings["mode"], "plan", "{settings}");

    // Listing the chats of a folder starts none, so it reads nothing a chat would.
    let listing = medha(&world.home(), &world.provider)
        .arg("--sessions")
        .envs(env)
        .current_dir(&folder)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&listing.stderr);
    assert!(listing.status.success(), "{said}");
}

/// A remote MCP server with one tool, counting how many times a client connected to it.
fn remote_mcp_server() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let connected = Arc::new(AtomicUsize::new(0));
    let counted = connected.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
            let Some((_, body)) = read_request(&mut stream) else {
                continue;
            };
            let asked: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let result = match asked["method"].as_str() {
                Some("initialize") => {
                    counted.fetch_add(1, Ordering::SeqCst);
                    json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                        "serverInfo": {"name": "hosted", "version": "0.0.1"}})
                }
                Some("tools/list") => json!({"tools": [{"name": "ping",
                    "description": "Ping the hosted server", "inputSchema": {"type": "object"}}]}),
                _ => json!({}),
            };
            let reply = json!({"jsonrpc": "2.0", "id": asked["id"], "result": result});
            respond(&mut stream, "application/json", &reply.to_string());
        }
    });
    (url, connected)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chats_in_the_backend_share_one_connection_to_a_remote_mcp_server() {
    let world = World::new();
    let (url, connected) = remote_mcp_server();
    std::fs::create_dir_all(world.home()).unwrap();
    let config = format!("[mcp.hosted]\nurl = \"{url}\"\ntrust = \"trusted\"\n");
    std::fs::write(world.home().join("config.toml"), config).unwrap();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chats = [
        client.open(&world.folder("one")).await,
        client.open(&world.folder("two")).await,
    ];

    // Each chat is told the server is ready and offers its tool.
    let mut ready = std::collections::HashSet::new();
    while ready.len() < chats.len() {
        let frame = client.next().await;
        let said = &frame["params"]["frame"];
        let hosted = said["params"]["servers"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|server| server["server"] == "hosted" && server["state"] == "ready");
        if said["method"] == "mcp.status" && hosted {
            ready.insert(frame["params"]["session"].as_str().unwrap().to_string());
        }
    }
    let connections = connected.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        connections, 1,
        "each chat connected to the server for itself"
    );
}

/// A config file that is deleted names no servers; that is followed like any other change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_config_takes_its_servers_away_from_the_shared_host() {
    let world = World::new();
    let (url, _) = remote_mcp_server();
    std::fs::create_dir_all(world.home()).unwrap();
    let config = world.home().join("config.toml");
    std::fs::write(
        &config,
        format!("[mcp.hosted]\nurl = \"{url}\"\ntrust = \"trusted\"\n"),
    )
    .unwrap();
    let backend = world.backend();
    let mut client = backend.connect().await;
    client.open(&world.folder("w")).await;
    let hosted = |frame: &Value| {
        let said = &frame["params"]["frame"];
        (said["method"] == "mcp.status").then(|| {
            said["params"]["servers"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|server| server["server"] == "hosted" && server["state"] == "ready")
        })
    };
    while hosted(&client.next().await) != Some(true) {}

    std::fs::remove_file(&config).unwrap();
    while hosted(&client.next().await) != Some(false) {}
}

/// Saving a key leaves the config file as it was: only being told makes the host use the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_saved_through_the_backend_reaches_the_shared_mcp_host_at_once() {
    use std::sync::atomic::Ordering::SeqCst;
    let world = World::new();
    let (url, connected) = remote_mcp_server();
    std::fs::create_dir_all(world.home()).unwrap();
    let config = format!("[mcp.hosted]\nurl = \"{url}\"\ntrust = \"trusted\"\nauth = \"bearer\"\n");
    std::fs::write(world.home().join("config.toml"), config).unwrap();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("w");

    for (round, key) in ["sk-zz-first-mcp-key", "sk-zz-second-mcp-key"]
        .into_iter()
        .enumerate()
    {
        let before = connected.load(SeqCst);
        let save = json!({"method": "settings.keys.set",
            "params": {"group": "mcp", "id": "hosted", "key": key}});
        client.about(&folder, save).await.expect("a key is saved");
        let deadline = Instant::now() + Duration::from_secs(20);
        while connected.load(SeqCst) == before {
            assert!(
                Instant::now() < deadline,
                "key {round} was saved and the host went on with the old one"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_backend_saves_keys_reaches_out_and_installs_without_showing_a_key() {
    let world = World::new();
    let folder = world.folder("w");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let endpoint = world.provider.url.clone();

    // A model saved with its key through the backend: the key is kept and never shown.
    let profile = json!({"protocol": "open-ai-chat", "base_url": endpoint, "model": "keyed",
        "auth": "bearer", "max_ctx": 32768});
    let save = json!({"method": "settings.model.save",
        "params": {"name": "keyed", "profile": profile, "key": "sk-zz-model-key"}});
    client
        .about(&folder, save)
        .await
        .expect("a model is saved with its key");
    let keys = json!({"method": "settings.keys"});
    let held = client.about(&folder, keys.clone()).await;
    assert!(held.unwrap().to_string().contains(r#""present":true"#));

    // Replaced, then removed.
    let key = json!({"group": "model", "id": endpoint, "key": "sk-zz-second-key"});
    client
        .about(
            &folder,
            json!({"method": "settings.keys.set", "params": key}),
        )
        .await
        .expect("a key is replaced");
    let gone =
        json!({"method": "settings.keys.remove", "params": {"group": "model", "id": endpoint}});
    client
        .about(&folder, gone)
        .await
        .expect("the backend removes a key");
    let held = client.about(&folder, keys).await;
    assert!(!held.unwrap().to_string().contains(r#""present":true"#));

    // Discovery reaches the model's endpoint.
    let discover = json!({"method": "settings.model.discover", "params": {"profile": {
        "protocol": "open-ai-chat", "base_url": endpoint, "model": "any", "auth": "none"}}});
    let found = client.about(&folder, discover).await;
    assert_eq!(found.unwrap()["models"][0]["id"], "test-model");

    // An extension installed from a folder, then removed.
    let package = world.root.path().join("package");
    std::fs::create_dir_all(&package).unwrap();
    let manifest = "schema_version = 1\nid = \"dev.medha.review\"\nname = \"Review helpers\"\n\
        version = \"0.1.0\"\nmedha = \">=0.1.0, <0.2.0\"\n\n[[components]]\nkind = \"action\"\n\
        id = \"review\"\ntitle = \"Review\"\ndescription = \"Review the selected change\"\n\
        prompt = \"Review the selected change.\"\n";
    std::fs::write(package.join("plugin.toml"), manifest).unwrap();
    let install = json!({"method": "extensions.install", "params": {"source": package}});
    let installed = client
        .about(&folder, install)
        .await
        .expect("an extension is installed");
    assert_eq!(installed["id"], "dev.medha.review");
    let listed = client
        .about(&folder, json!({"method": "extensions.list"}))
        .await;
    assert!(listed.unwrap().to_string().contains("dev.medha.review"));
    let checked = client
        .about(&folder, json!({"method": "extensions.doctor"}))
        .await;
    assert!(checked.is_ok(), "{checked:?}");
    let remove = json!({"method": "extensions.remove", "params": {"id": "dev.medha.review"}});
    client
        .about(&folder, remove)
        .await
        .expect("an extension is removed");
    let listed = client
        .about(&folder, json!({"method": "extensions.list"}))
        .await;
    assert!(!listed.unwrap().to_string().contains("dev.medha.review"));

    // What cannot be done is refused, not accepted quietly.
    let refused = [
        json!({"method": "extensions.marketplace.add", "params": {"source": "/not/a/repository"}}),
        json!({"method": "extensions.enable", "params": {"id": "nobody"}}),
        json!({"method": "extensions.rollback", "params": {"id": "nobody"}}),
        json!({"method": "extensions.remove", "params": {"id": "nobody"}}),
        json!({"method": "settings.tools.save", "params": {"preset": "no-such-preset"}}),
        json!({"method": "settings.mcp.save", "params": {"args": []}}),
        json!({"method": "settings.mcp.remove", "params": {"id": "nobody"}}),
        json!({"method": "settings.model.remove", "params": {"name": "nobody"}}),
        json!({"method": "settings.keys.set", "params": {"group": "model", "id": "nobody", "key": "k"}}),
        json!({"method": "extensions.hooks.remove", "params": {"event": "nothing", "file": "none"}}),
    ];
    for request in refused {
        let from_backend = client.about(&folder, request.clone()).await;
        assert!(from_backend.is_err(), "{request} was accepted");
    }
    for key in [SECRET, "sk-zz-model-key", "sk-zz-second-key"] {
        assert!(!client.heard.contains(key), "{key} reached a client");
    }
}
