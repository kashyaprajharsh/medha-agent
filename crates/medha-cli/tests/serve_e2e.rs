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

#[path = "common/editor_checks.rs"]
mod editor_checks;

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
                return Some((
                    headers.to_string(),
                    request[end + 4..end + 4 + length].to_vec(),
                ));
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

/// Largest request the SIZE_LIMIT_SERVER takes: room for a 4 MiB image, not more.
const SERVER_SIZE_LIMIT: usize = 6_000_000;

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
    let mut body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    // Test-only request evidence; never part of the model response or Medha's
    // event log. All credentials used by this server are dummy values.
    body["_test_authorization"] = json!(
        line.lines()
            .find_map(|header| header.strip_prefix("authorization:"))
            .map(str::trim)
    );
    let asked = last_user(&body);
    let ran = body["messages"]
        .as_array()
        .is_some_and(|all| all.iter().any(|message| message["role"] == "tool"));
    let _ = seen.send(body.clone());
    // A chat that names a text-only server gets one: it takes no images, and
    // says so the way a local server set to allow none does.
    let raw = body.to_string();
    let refuse = |stream: &mut TcpStream, refusal: &str| {
        let _ = write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{refusal}",
            refusal.len()
        );
    };
    if raw.contains("TEXT_ONLY_SERVER") && raw.contains("\"image_url\"") {
        return refuse(
            &mut stream,
            r#"{"error":{"message":"At most 0 image(s) may be provided in one prompt. (parameter=image)","type":"BadRequestError","param":"image","code":400}}"#,
        );
    }
    // One that reads images but refuses a large request, in words of its own.
    if raw.contains("SIZE_LIMIT_SERVER") && raw.len() > SERVER_SIZE_LIMIT {
        return refuse(
            &mut stream,
            r#"{"error":{"message":"request rejected by the gateway","code":400}}"#,
        );
    }
    // A message left unanswered arrives joined to the next; the newest part decides.
    if asked
        .rsplit("\n\n")
        .next()
        .unwrap_or(&asked)
        .contains("HOLD")
    {
        let _ = release.lock().unwrap().recv_timeout(WAIT);
    }
    if body["stream"] == false {
        return respond(&mut stream, "application/json", &json!({"choices":[{"message":{"role":"assistant","content":format!("echo: {asked}")},"finish_reason":"stop"}]}).to_string());
    }
    let read_completed = body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .is_some_and(|message| message["role"] == "tool");
    let events = if asked.starts_with("Call medha_setup_echo") {
        let call = json!({"index":0,"id":"setup-test","type":"function","function":{"name":"medha_setup_echo","arguments":"{\"value\":\"medha-check\"}"}});
        vec![
            json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[call]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]
    } else if let Some(elements) = asked
        .strip_prefix("DRAW ")
        .or_else(|| asked.strip_prefix("DRAWLIST "))
        .filter(|_| !read_completed)
    {
        // DRAWLIST sends the elements as a list, where the tool takes JSON text.
        let elements = if asked.starts_with("DRAWLIST ") {
            serde_json::from_str::<Value>(elements).unwrap()
        } else {
            json!(elements)
        };
        let name = body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .find(|name| name.ends_with("drawing__create_view"))
            .expect("the drawing tool was not offered to the model");
        let call = json!({"index":0,"id":"drawing-call","type":"function",
            "function":{"name":name,"arguments":json!({"elements":elements}).to_string()}});
        vec![
            json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[call]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]
    } else if let Some(path) = asked.strip_prefix("READ_PATH ").filter(|_| !read_completed) {
        let arguments = json!({"path":path});
        let call = json!({"index":0,"id":"read_path","type":"function",
            "function":{"name":"read","arguments":arguments.to_string()}});
        vec![
            json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[call]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]
    } else if let Some(rest) = asked.strip_prefix("WRITE ").filter(|_| !read_completed) {
        // WRITE <path> then a newline, then the file's contents.
        let (path, content) = rest.split_once('\n').unwrap_or((rest, ""));
        let arguments = json!({"path": path, "content": content});
        let call = json!({"index": 0, "id": "write_1", "type": "function",
            "function": {"name": "write", "arguments": arguments.to_string()}});
        vec![
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [call]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]
    } else if asked.contains("RUN") && !ran {
        let shell = body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .find(|name| name.contains("shell") && name.contains("exec"))
            .unwrap_or("shell_exec");
        // The first `quoted` part of the message is the command, if there is one.
        let command = asked
            .split('`')
            .nth(1)
            .unwrap_or("touch made-by-the-test.txt");
        let arguments = json!({"command": command, "network": false});
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
        .env("MEDHA_CATALOGUE_OFFLINE", "1")
        // These tests read the log, so what is written there is theirs to say.
        .env("RUST_LOG", "info")
        // Saved keys go to a file under the home, never to this machine's keychain.
        .env("MEDHA_CRED_STORE", "file");
    command
}

/// The real external ACP adapter, connected to this test's shared backend.
struct EditorProcess {
    child: Child,
    input: Option<std::process::ChildStdin>,
    frames: mpsc::Receiver<Value>,
    heard: Vec<Value>,
    asked: u64,
    session: String,
}
impl EditorProcess {
    fn start(folder: &Path, home: &Path, provider: &Provider, env: &[(&str, &str)]) -> Self {
        let mut editor = Self::launch(folder, home, provider, env);
        let hello = editor.rpc(
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
        );
        assert_eq!(hello["result"]["protocolVersion"], 1, "{hello}");
        let made = editor.rpc("session/new", json!({"cwd":folder,"mcpServers":[]}));
        editor.session = made["result"]["sessionId"]
            .as_str()
            .expect("ACP session id")
            .to_owned();
        editor
    }
    fn launch(folder: &Path, home: &Path, provider: &Provider, env: &[(&str, &str)]) -> Self {
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
        let stdout = child.stdout.take().unwrap();
        let (sender, frames) = mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                let frame: Value = serde_json::from_str(&line).expect("ACP JSON frame");
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input: Some(input),
            frames,
            heard: Vec::new(),
            asked: 0,
            session: String::new(),
        }
    }
    fn frame(&mut self) -> Value {
        let frame = self
            .frames
            .recv_timeout(WAIT)
            .unwrap_or_else(|e| panic!("ACP went quiet: {e}; {:?}", self.heard));
        self.heard.push(frame.clone());
        frame
    }
    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.asked += 1;
        writeln!(
            self.input.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","id":self.asked,"method":method,"params":params})
        )
        .unwrap();
        self.asked
    }
    fn result(&mut self, id: u64) -> Value {
        if let Some(frame) = self.heard.iter().find(|frame| frame["id"] == id) {
            return frame.clone();
        }
        loop {
            let frame = self.frame();
            if frame["id"] == id {
                return frame;
            }
        }
    }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.result(id)
    }
    fn disconnect(mut self) {
        drop(self.input.take());
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "ACP failed on stdin EOF: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "ACP did not exit on stdin EOF");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn ask(&mut self, method: &str, params: Value) -> Value {
        let (method, mut params) = match method {
            "session.settings" => ("_medha/session.settings", json!({})),
            "session.configure" => ("_medha/session.configure", json!({"change":params})),
            "message.send" => (
                "session/prompt",
                json!({"prompt":[{"type":"text","text":params["content"]}]}),
            ),
            other => (other, params),
        };
        params["sessionId"] = json!(self.session);
        self.rpc(method, params)
    }
}
impl Drop for EditorProcess {
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
                // A restarting backend may replace a stale token after we read it.
                && let Ok(client) = Client::meet(stream, &token).await
            {
                return client;
            }
            assert!(Instant::now() < deadline, "the backend never authenticated");
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

#[cfg(unix)]
struct TerminalClient {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<test_support::TerminalCapture>>,
}

#[cfg(unix)]
impl TerminalClient {
    fn start(world: &World, folder: &Path) -> Self {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_medha"));
        command.cwd(folder);
        command.env("TERM", "xterm-256color");
        command.env("MEDHA_HOME", world.home());
        command.env("MEDHA_BASE_URL", &world.provider.url);
        command.env("MEDHA_MODEL", "test-model");
        command.env("MEDHA_API_KEY", SECRET);
        command.env("MEDHA_PROTOCOL", "open-ai-chat");
        command.env("MEDHA_CRED_STORE", "file");
        command.env("MEDHA_CATALOGUE_OFFLINE", "1");
        command.env("MEDHA_MODE", "careful");
        command.env("RUST_LOG", "info");
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let writer = pair.master.take_writer().unwrap();
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
        Self {
            child,
            master: pair.master,
            writer,
            output,
        }
    }

    fn type_text(&mut self, text: &str) {
        self.writer.write_all(text.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn asked(&mut self, provider: &Provider) -> String {
        let request = provider.seen.recv_timeout(WAIT).unwrap_or_else(|error| {
            panic!(
                "no terminal model request arrived: {error}; child: {:?}; {}",
                self.child.try_wait(),
                self.output.lock().unwrap().evidence()
            )
        });
        user_texts(&request)
    }

    fn until(&mut self, marker: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let output = self.output.lock().unwrap();
            if output.contains(marker) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "terminal never drew {marker:?}; {}",
                output.evidence()
            );
            drop(output);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn exit(&mut self) {
        self.type_text("\x04");
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "terminal failed: {status:?}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "terminal did not exit after Ctrl-D"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        self.until("\x1b[?1049l");
    }
}

#[cfg(unix)]
impl Drop for TerminalClient {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            // Only the exact process started by this fixture.
            if let Some(pid) = self.child.process_id() {
                // The unreaped fixture child owns this PID, so reuse is impossible.
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_terminal_and_another_client_share_one_chat_and_survive_viewer_exit() {
    let world = World::new();
    let folder = world.folder("terminal");
    let backend = world.backend();
    let mut viewer = backend.connect().await;
    let mut terminal = TerminalClient::start(&world, &folder);
    terminal.until("test-model");
    let listed = viewer.ask("session.list", None, json!({})).await;
    let live: Vec<_> = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|chat| chat["about"]["folder"] == json!(folder))
        .collect();
    assert_eq!(live.len(), 1, "{listed}");
    let session = live[0]["session"].as_str().unwrap().to_string();
    let attached = viewer
        .ask("session.attach", Some(&session), json!({"after":0}))
        .await;
    assert_eq!(attached["result"]["gap"], false);
    let settings = viewer
        .ask("session.settings", Some(&session), json!({}))
        .await;
    assert_eq!(settings["result"]["mode"], "careful");

    terminal.type_text("FROM_TUI\r");
    assert!(terminal.asked(&world.provider).contains("FROM_TUI"));
    viewer.until(|frame| kind(frame, "turn.done")).await;
    terminal.until("echo: FROM_TUI");
    viewer.send(&session, "FROM_OTHER_VIEWER").await;
    assert!(world.provider.asked().contains("FROM_OTHER_VIEWER"));
    viewer.until(|frame| kind(frame, "turn.done")).await;
    terminal.until("echo: FROM_OTHER_VIEWER");

    // Resize the actual PTY and use a control while attached to an active turn.
    terminal.output.lock().unwrap().resize(24, 80);
    terminal
        .master
        .resize(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    terminal.type_text("HOLD terminal cancellation\r");
    assert!(
        terminal
            .asked(&world.provider)
            .contains("HOLD terminal cancellation")
    );
    viewer.until(|frame| kind(frame, "turn.started")).await;
    // The other viewer can see the turn before the terminal has consumed it.
    terminal.until("thinking");
    terminal.type_text("\x1b");
    viewer.until(|frame| kind(frame, "turn.cancelled")).await;
    world.provider.release.send(()).unwrap();
    terminal.exit();

    // Closing a terminal detaches its viewer; the other viewer remains usable.
    let settings = viewer
        .ask("session.settings", Some(&session), json!({}))
        .await;
    assert!(settings.get("error").is_none(), "{settings}");
    let status = viewer.ask("backend.status", None, json!({})).await;
    assert_eq!(status["result"]["chats"], 1);
}

impl Client {
    async fn meet<S: AsyncRead + AsyncWrite + Send + 'static>(
        stream: S,
        token: &str,
    ) -> Result<Self, wire::Refusal> {
        let (read, write) = tokio::io::split(stream);
        let mut reader: BufReader<Box<dyn AsyncRead + Unpin + Send>> =
            BufReader::new(Box::new(read));
        let mut writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(write);
        let welcome = wire::greet(&mut reader, &mut writer, token, ROLES).await?;
        assert_eq!(welcome["result"]["protocol"], 1);
        Ok(Self {
            reader,
            writer,
            events: Default::default(),
            heard: String::new(),
            asked: 0,
        })
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
            .unwrap_or_else(|error| {
                panic!("the backend went quiet: {error}; heard: {}", self.heard)
            })
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

#[tokio::test]
async fn a_live_snapshot_covers_its_events_and_a_delayed_steer_never_becomes_a_new_turn() {
    let world = World::new();
    let backend = Backend::start(&world.home(), &world.provider, &[]);
    let mut client = backend.connect().await;
    let session = client.open(&world.folder("presentation")).await;
    let admitted = client
        .ask(
            "message.send",
            Some(&session),
            json!({"content": "HOLD snapshot", "intent": {"kind": "start"}}),
        )
        .await;
    assert_eq!(admitted["result"]["turn"], 1);
    assert!(world.provider.asked().contains("HOLD snapshot"));
    let first = client
        .ask("session.presentation", Some(&session), json!({}))
        .await;
    let snapshot: protocol::PresentationSnapshot =
        serde_json::from_value(first["result"].clone()).unwrap();
    assert!(snapshot.running);
    assert_eq!(snapshot.turn, 1);
    assert_eq!(snapshot.conversation, session);
    assert_eq!(snapshot.items.iter().filter(|item| matches!(item, protocol::PresentationItem::User { text, .. } if text == "HOLD snapshot")).count(), 1);
    let cursor = snapshot.cursor.unwrap();
    assert!(cursor.after > 0);

    let mut viewer = backend.connect().await;
    let attached = viewer
        .ask(
            "session.attach",
            Some(&session),
            json!({"stream": cursor.stream, "after": cursor.after}),
        )
        .await;
    assert_eq!(attached["result"]["gap"], false);
    assert_eq!(attached["result"]["replayed"], 0);
    let dialect = viewer
        .ask("initialize", Some(&session), json!({"protocolVersion": 1}))
        .await;
    assert!(dialect.get("error").is_some());
    let stale = viewer
        .ask(
            "message.send",
            Some(&session),
            json!({"content": "stale", "intent": {"kind": "steer", "turn": 0}}),
        )
        .await;
    assert!(stale.get("error").is_some());
    let queued = viewer
        .ask(
            "message.send",
            Some(&session),
            json!({"content": "keep this text", "intent": {"kind": "steer", "turn": 1}}),
        )
        .await;
    assert_eq!(queued["result"]["steered"], true);
    let waiting = viewer
        .ask("session.presentation", Some(&session), json!({}))
        .await;
    assert_eq!(
        waiting["result"]["pending_steers"],
        json!(["keep this text"])
    );
    viewer.ask("cancel", Some(&session), json!({})).await;
    client.until(|frame| kind(frame, "message.returned")).await;
    client.until(|frame| kind(frame, "turn.cancelled")).await;
    let late = viewer
        .ask(
            "message.send",
            Some(&session),
            json!({"content": "never auto restart", "intent": {"kind": "steer", "turn": 1}}),
        )
        .await;
    assert!(late.get("error").is_some());
    assert!(
        world
            .provider
            .seen
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    let settled = viewer
        .ask("session.presentation", Some(&session), json!({}))
        .await;
    assert_eq!(settled["result"]["running"], false);
    assert_eq!(settled["result"]["pending_steers"], json!([]));
    world.provider.release.send(()).unwrap();
}

/// A healthy reader must not lose its chat merely because requests arrive
/// faster than synchronous notifications can be written to the chat pipe.
#[tokio::test]
async fn settings_burst_is_backpressured_and_keeps_the_chat_alive() {
    let world = World::new();
    let backend = Backend::start(&world.home(), &world.provider, &[]);
    let mut client = backend.connect().await;
    let session = client.open(&world.folder("burst")).await;
    let first = client.asked + 1;
    let count = 3000;
    let writer = &mut client.writer;
    let reader = &mut client.reader;
    let sending = async {
        for id in first..first + count {
            let frame = json!({"jsonrpc": "2.0", "id": id, "session": session,
                "method": "session.settings", "params": {}});
            assert!(wire::write_frame(writer, &frame).await);
        }
    };
    let receiving = async {
        let mut replies = std::collections::HashSet::new();
        while replies.len() < count as usize {
            let frame = wire::read_frame(reader).await.unwrap();
            assert_ne!(
                frame["method"], "session.ended",
                "chat ended during burst: {frame}"
            );
            if let Some(id) = frame["id"]
                .as_u64()
                .filter(|id| (first..first + count).contains(id))
            {
                assert!(replies.insert(id), "duplicate reply for {id}");
                // Bounded admission may refuse a request, but cannot lose it.
                assert!(frame.get("result").is_some() || frame.get("error").is_some());
            }
        }
    };
    tokio::time::timeout(WAIT, async {
        tokio::join!(sending, receiving);
    })
    .await
    .expect("the burst lost a reply");
    client.asked += count;
    assert!(
        client
            .ask("session.settings", Some(&session), json!({}))
            .await
            .get("result")
            .is_some()
    );
}

#[tokio::test]
async fn idle_exit_waits_for_clients_and_recovers_a_stale_address() {
    let world = World::new();
    let mut backend = Backend::start(
        &world.home(),
        &world.provider,
        &[("MEDHA_SERVE_IDLE_SECONDS", "1")],
    );
    let mut client = backend.connect().await;
    let hello = client.ask("hello", None, json!({})).await;
    assert_eq!(hello["result"]["build"], wire::BUILD_ID);
    assert!(
        hello["result"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cap| cap == "lifecycle")
    );
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert!(
        backend.child.try_wait().unwrap().is_none(),
        "connected clients keep the backend alive"
    );
    drop(client);
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(status) = backend.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "idle backend did not exit");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!backend.file("address").exists());
    // A crash can leave both discovery files and a socket behind. Only the
    // singleton holder clears/replaces them before publishing new discovery.
    let next = Backend::start(&world.home(), &world.provider, &[]);
    let mut next = next;
    let _ = next.connect().await;
    next.child.kill().unwrap();
    next.child.wait().unwrap();
    assert!(next.file("address").exists());
    let replacement = Backend::start(&world.home(), &world.provider, &[]);
    let mut client = replacement.connect().await;
    assert!(
        client
            .ask("hello", None, json!({}))
            .await
            .get("result")
            .is_some()
    );
}

#[tokio::test]
async fn upgrade_refuses_active_work_and_an_idle_upgrade_exits() {
    let world = World::new();
    let mut backend = Backend::start(&world.home(), &world.provider, &[]);
    let mut client = backend.connect().await;
    let session = client.open(&world.folder("upgrade")).await;
    let reply = client
        .ask(
            "backend.prepare_upgrade",
            None,
            json!({"build": wire::BUILD_ID}),
        )
        .await;
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("active"),
        "{reply}"
    );
    assert!(
        client
            .ask("session.settings", Some(&session), json!({}))
            .await
            .get("result")
            .is_some()
    );
    let closing = client.ask("session.close", Some(&session), json!({})).await;
    let closing: protocol::Closing = serde_json::from_value(closing["result"].clone()).unwrap();
    assert!(closing.closing);
    let deadline = Instant::now() + Duration::from_secs(4);
    while !client.ask("session.list", None, json!({})).await["result"]["sessions"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let frame = json!({"id": 999, "method": "backend.prepare_upgrade", "params": {"build": wire::BUILD_ID}});
    assert!(wire::write_frame(&mut client.writer, &frame).await);
    let reply = tokio::time::timeout(Duration::from_secs(2), wire::read_frame(&mut client.reader))
        .await
        .unwrap();
    if let Some(reply) = reply {
        assert!(reply.get("error").is_none(), "{reply}");
    }
    let deadline = Instant::now() + Duration::from_secs(6);
    while backend.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "upgrade failed to exit");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn cli_and_backend_cannot_own_the_same_chat_and_a_crash_releases_ownership() {
    let world = World::new();
    let folder = world.folder("lease");
    let backend = Backend::start(&world.home(), &world.provider, &[]);
    let mut client = backend.connect().await;
    let session = client.open(&folder).await;
    client.send(&session, "seed the lease history").await;
    client.until(|frame| kind(frame, "turn.done")).await;
    let refused = medha(&world.home(), &world.provider)
        .args(["--resume", &session, "direct lease probe"])
        .current_dir(&folder)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("already open"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    client.ask("session.close", Some(&session), json!({})).await;
    let deadline = Instant::now() + Duration::from_secs(4);
    while !client.ask("session.list", None, json!({})).await["result"]["sessions"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // One-shot execution deliberately stays in-process and uses the same
    // durable lease. ACP now attaches to the backend instead of competing.
    struct DirectOwner(Child);
    impl Drop for DirectOwner {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut own = DirectOwner(
        medha(&world.home(), &world.provider)
            .args(["--resume", &session, "HOLD direct owner"])
            .current_dir(&folder)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    loop {
        if world.provider.asked().contains("HOLD direct owner") {
            break;
        }
    }
    let refused = client.create(&folder, Some(&session)).await;
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already open"),
        "{refused}"
    );
    own.0.kill().unwrap();
    own.0.wait().unwrap();
    let _ = world.provider.release.send(());
    let resumed = client.create(&folder, Some(&session)).await;
    assert_eq!(resumed["result"]["session"], session, "{resumed}");
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_shutdown_waits_for_blocked_owned_work_and_keeps_the_singleton() {
    use std::os::fd::AsRawFd;
    let world = World::new();
    let mut backend = Backend::start(
        &world.home(),
        &world.provider,
        &[("TOKIO_WORKER_THREADS", "1")],
    );
    let mut client = backend.connect().await;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(world.home().join("credentials.lock"))
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    std::fs::write(
        world.home().join("config.toml"),
        "[mcp.blocked]\nurl=\"http://127.0.0.1:9/mcp\"\ntrust=\"trusted\"\nauth=\"bearer\"\n",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        client
            .ask("hello", None, json!({}))
            .await
            .get("result")
            .is_some()
    );
    unsafe {
        libc::kill(backend.child.id() as libc::pid_t, libc::SIGTERM);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        backend.child.try_wait().unwrap().is_none(),
        "the blocked watcher must remain accounted for"
    );
    let replacement = medha(&world.home(), &world.provider)
        .arg("serve")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !replacement.status.success(),
        "a replacement acquired ownership while the old work was still alive"
    );
    assert!(String::from_utf8_lossy(&replacement.stderr).contains("already running"));
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(status) = backend.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "MCP shutdown left the process alive past the hard drain bound"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    drop(lock);
}

struct World {
    root: tempfile::TempDir,
    provider: Provider,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gemini_reasoning_choices_and_model_switches_are_shared_between_viewers() {
    let world = World::new();
    let folder = world.folder("gemini-settings");
    // The catalogue belongs to this backend's MEDHA_HOME, even when the OS
    // home is different (Windows does not resolve it from HOME/USERPROFILE).
    let os_home = world.root.path().join("os-home");
    std::fs::create_dir_all(&os_home).unwrap();
    std::fs::create_dir_all(world.home()).unwrap();
    let fetched = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let snapshot = json!({"schema_version":4,"fetched_at_unix":fetched,"entries":{
        "google/gemini-3.8-flash":{"reasoning_efforts":["low","medium","high"]},
        "google/gemini-fixture-only":{"reasoning_efforts":["low","high"]}
    }});
    std::fs::write(
        world.home().join("models_dev_cache.json"),
        snapshot.to_string(),
    )
    .unwrap();
    let os_home = os_home.to_str().unwrap();
    let backend = world.backend_in(&[
        ("HOME", os_home),
        ("USERPROFILE", os_home),
        ("MEDHA_BASE_URL", ""),
        ("MEDHA_API_KEY", ""),
        ("MEDHA_MODEL", ""),
        ("MEDHA_PROTOCOL", ""),
    ]);
    let mut first = backend.connect().await;
    for (name, model, default) in [
        ("flash6", "gemini-3.6-flash", true),
        ("flash8", "gemini-3.8-flash", false),
        ("catalogue", "gemini-fixture-only", false),
    ] {
        first
            .about(
                &folder,
                json!({"method":"settings.model.save","params":{
                    "name":name,"default":default,"key":SECRET,
                    "profile":{"protocol":"gemini-interactions","provider_id":"google","base_url":world.provider.url,
                        "model":model,"auth":"x-goog-api-key","max_ctx":128000,"reasoning":"effort"}
                }}),
            )
            .await
            .expect("save isolated Gemini profile");
    }
    let chat = first.open(&folder).await;
    let initial = first.ask("session.settings", Some(&chat), json!({})).await;
    assert_eq!(initial["result"]["model"], "gemini-3.6-flash", "{initial}");
    assert_eq!(
        initial["result"]["efforts"],
        json!(["minimal", "low", "medium", "high"])
    );
    let minimal = first
        .ask(
            "session.configure",
            Some(&chat),
            json!({"effort":"minimal"}),
        )
        .await;
    assert_eq!(minimal["result"]["effort"], "minimal", "{minimal}");

    let mut second = backend.connect().await;
    let attached = second
        .ask("session.attach", Some(&chat), json!({"after":0}))
        .await;
    assert_eq!(attached["result"]["gap"], false);
    let switched = first
        .ask(
            "session.configure",
            Some(&chat),
            json!({"profile":"flash8"}),
        )
        .await;
    assert_eq!(
        switched["result"]["model"], "gemini-3.8-flash",
        "{switched}"
    );
    assert_eq!(switched["result"]["effort"], "auto");
    assert_eq!(switched["result"]["reasoning"], "auto");
    assert_eq!(
        switched["result"]["efforts"],
        json!(["low", "medium", "high"])
    );
    let (_, notification) = second
        .until(|frame| {
            frame["method"] == "settings" && frame["params"]["model"] == "gemini-3.8-flash"
        })
        .await;
    assert_eq!(notification["params"], switched["result"]);
    let refused = second
        .ask(
            "session.configure",
            Some(&chat),
            json!({"effort":"minimal"}),
        )
        .await;
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not supported"),
        "{refused}"
    );
    let high = second
        .ask("session.configure", Some(&chat), json!({"effort":"high"}))
        .await;
    assert_eq!(high["result"]["effort"], "high", "{high}");
    let observed = first.ask("session.settings", Some(&chat), json!({})).await;
    assert_eq!(observed["result"], high["result"]);

    // A model absent from bundled corrections uses the exact catalogue effort
    // list on switching, and again when a new chat starts with that default.
    let custom = first
        .ask(
            "session.configure",
            Some(&chat),
            json!({"profile":"catalogue"}),
        )
        .await;
    assert_eq!(
        custom["result"]["efforts"],
        json!(["low", "high"]),
        "{custom}"
    );
    first
        .about(
            &folder,
            json!({"method":"settings.model.default","params":{"name":"catalogue"}}),
        )
        .await
        .unwrap();
    let new_chat = first.open(&folder).await;
    let new_settings = first
        .ask("session.settings", Some(&new_chat), json!({}))
        .await;
    assert_eq!(
        new_settings["result"]["model"], "gemini-fixture-only",
        "{new_settings}"
    );
    assert_eq!(new_settings["result"]["efforts"], json!(["low", "high"]));
    assert!(
        world.provider.seen.try_recv().is_err(),
        "settings made an inference call"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_application_resources_preserve_credentials_and_reject_stale_grants() {
    use protocol::{ChangeResource, ReadResource, ResourceResult};
    let world = World::new();
    let folder = world.folder("typed-features");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let change = ChangeResource::SaveModel(protocol::ModelDraft {
        provider_id: None,
        auth: None,
        headers: Default::default(),
        name: Some("saved".into()),
        protocol: protocol::ModelProtocol::OpenAiChat,
        base_url: world.provider.url.clone(),
        model: "test-model".into(),
        context_limit: Some(128000),
        key: Some(protocol::Secret("sk-zz-saved".into())),
    });
    let reply = client
        .about(
            &folder,
            json!({"method":"application.resources.change", "params":change}),
        )
        .await
        .unwrap();
    let saved: ResourceResult = serde_json::from_value(reply).unwrap();
    assert!(
        matches!(saved, ResourceResult::ModelSaved { catalogue, .. } if catalogue.profiles.iter().any(|profile| profile.name == "saved" && profile.key_present && profile.requires_key))
    );

    let mut startup = protocol::StartupOptions {
        model: Some("saved".into()),
        model_env: protocol::ModelOverrides {
            api_key: Some("sk-zz-client".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let refused = client
        .ask(
            "session.create",
            None,
            json!({"folder":folder,"startup":startup}),
        )
        .await;
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("MEDHA_BASE_URL")
    );
    assert!(!refused.to_string().contains("sk-zz-client"));
    startup.model_env.base_url = Some(world.provider.url.clone());
    let made = client
        .ask(
            "session.create",
            None,
            json!({"folder":folder,"startup":startup}),
        )
        .await;
    let chat = made["result"]["session"]
        .as_str()
        .expect("chat starts")
        .to_owned();
    client
        .ask("session.attach", Some(&chat), json!({"after":0}))
        .await;
    let bootstrap = client
        .ask(
            "session.inspect",
            Some(&chat),
            json!(protocol::SessionRead::Bootstrap),
        )
        .await;
    let decoded: protocol::SessionResult =
        serde_json::from_value(bootstrap["result"].clone()).unwrap();
    assert!(
        matches!(decoded, protocol::SessionResult::Bootstrap(protocol::Bootstrap { tools, .. }) if tools.iter().any(|tool| tool.name == "shell.exec" && !tool.icon.is_empty()))
    );

    // Switching a saved profile keeps this caller's override, even though the
    // daemon was started with a different dummy API key.
    let switched = client
        .ask(
            "session.configure",
            Some(&chat),
            json!(protocol::Configure::Profile("saved".into())),
        )
        .await;
    assert!(switched.get("result").is_some(), "{switched}");
    client.send(&chat, "caller key").await;
    let request = world.provider.seen.recv_timeout(WAIT).unwrap();
    assert_eq!(request["_test_authorization"], "bearer sk-zz-client");
    client.until(|frame| kind(frame, "turn.done")).await;

    let activated = client
        .ask(
            "session.profile.saved",
            Some(&chat),
            json!(protocol::ActivateSavedProfile {
                profile: "saved".into()
            }),
        )
        .await;
    assert!(activated.get("result").is_some(), "{activated}");
    client.send(&chat, "saved key").await;
    let request = world.provider.seen.recv_timeout(WAIT).unwrap();
    assert_eq!(request["_test_authorization"], "bearer sk-zz-saved");
    client.until(|frame| kind(frame, "turn.done")).await;

    // A local source is resolved in the caller's folder, not the daemon CWD.
    let package = folder.join("package");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("plugin.toml"), "schema_version = 1\nid = \"dev.medha.typed\"\nname = \"Typed\"\nversion = \"0.1.0\"\nmedha = \">=0.1.0, <0.2.0\"\n[[components]]\nkind = \"action\"\nid = \"review\"\ntitle = \"Review\"\ndescription = \"Review\"\nprompt = \"Review $ARGUMENTS\"\n").unwrap();
    let installed = client.about(&folder, json!({"method":"application.resources.change", "params":ChangeResource::InstallPlugin { source: "./package".into() }})).await.unwrap();
    assert!(
        matches!(serde_json::from_value::<ResourceResult>(installed).unwrap(), ResourceResult::PluginInstalled { id } if id == "dev.medha.typed")
    );
    let listed = client
        .ask(
            "session.resources",
            Some(&chat),
            json!(protocol::ChatResource(ReadResource::Plugins)),
        )
        .await;
    let ResourceResult::Plugins(list) = serde_json::from_value(listed["result"].clone()).unwrap()
    else {
        panic!("{listed}")
    };
    let plugin = list
        .plugins
        .iter()
        .find(|plugin| plugin.id == "dev.medha.typed")
        .unwrap();
    let enable = |hash: String| ChangeResource::EnablePlugin {
        id: plugin.id.clone(),
        scope: plugin.scope,
        hash,
        grant: plugin.grant.clone().unwrap(),
    };
    assert!(
        client
            .about(
                &folder,
                json!({"method":"application.resources.change", "params":enable("stale".into())})
            )
            .await
            .is_err()
    );
    client
        .about(
            &folder,
            json!({"method":"application.resources.change", "params":enable(plugin.hash.clone())}),
        )
        .await
        .unwrap();
    client
        .ask("extensions.reload", Some(&chat), json!({}))
        .await;
    let expanded = client
        .ask(
            "application.command.expand",
            Some(&chat),
            json!(protocol::ExpandCommand {
                typed: "/review changes".into()
            }),
        )
        .await;
    let expanded: protocol::ExpandedCommand =
        serde_json::from_value(expanded["result"].clone()).unwrap();
    assert_eq!(expanded.prompt, "Review changes");
    client.about(&folder, json!({"method":"application.resources.change", "params":ChangeResource::DisablePlugin { id: plugin.id.clone(), scope: plugin.scope }})).await.unwrap();
    client
        .ask("extensions.reload", Some(&chat), json!({}))
        .await;
    let refused = client
        .ask(
            "application.command.expand",
            Some(&chat),
            json!({"typed":"/review changes"}),
        )
        .await;
    assert!(refused.get("error").is_some());
    for key in [SECRET, "sk-zz-client", "sk-zz-saved"] {
        assert!(!client.heard.contains(key), "credential reached a viewer");
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_provider_setup_is_offline_capable_pins_credentials_and_tests_before_saving() {
    let world = World::new();
    let folder = world.folder("providers");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let catalogue = client
        .about(
            &folder,
            json!({"method":"application.providers","params":{}}),
        )
        .await
        .unwrap();
    let catalogue: protocol::ProviderCatalogue = serde_json::from_value(catalogue).unwrap();
    assert!(catalogue.providers.len() >= 220);
    assert_eq!(catalogue.revision.len(), 64);
    assert!(
        catalogue
            .providers
            .iter()
            .any(|p| p.id == "anthropic" && !p.available && p.unavailable_reason.is_some())
    );
    let config_before =
        std::fs::read_to_string(world.home().join("config.toml")).unwrap_or_default();
    let invalid = client.about(&folder, json!({"method":"application.providers.resolve","params":{"provider_id":"cloudflare-workers-ai","parameters":{"CLOUDFLARE_ACCOUNT_ID":"../other"}}})).await;
    assert!(invalid.is_err());
    let deployment: protocol::ModelDeployment = serde_json::from_value(client.about(&folder, json!({"method":"application.providers.resolve","params":{"provider_id":"cloudflare-workers-ai","parameters":{"CLOUDFLARE_ACCOUNT_ID":"my-account"}}})).await.unwrap()).unwrap();
    assert_eq!(
        deployment.base_url,
        "https://api.cloudflare.com/client/v4/accounts/my-account/ai/v1"
    );
    assert_eq!(
        std::fs::read_to_string(world.home().join("config.toml")).unwrap_or_default(),
        config_before,
        "reading/resolving metadata must not rewrite saved endpoints"
    );
    let mut draft = protocol::ModelDraft {
        provider_id: Some("fixture".into()),
        auth: Some(protocol::ModelAuth::Bearer),
        headers: Default::default(),
        name: Some("new-provider".into()),
        protocol: protocol::ModelProtocol::OpenAiChat,
        base_url: world.provider.url.clone(),
        model: "fixture-model".into(),
        context_limit: Some(8000),
        key: None,
    };
    let missing_key = client
        .about(
            &folder,
            json!({"method":"application.models.test","params":{"draft":draft}}),
        )
        .await;
    assert!(
        missing_key.unwrap_err().contains("API key is required"),
        "an ambient daemon key cannot authorize setup"
    );
    assert!(world.provider.seen.try_recv().is_err());
    draft.key = Some(protocol::Secret("dummy-setup-only".into()));
    let tested: protocol::ModelTestResult = serde_json::from_value(
        client
            .about(
                &folder,
                json!({"method":"application.models.test","params":{"draft":draft}}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(tested.text.status, protocol::ModelTestStatus::Passed);
    assert_eq!(tested.streaming.status, protocol::ModelTestStatus::Passed);
    assert_eq!(
        tested.tools.status,
        protocol::ModelTestStatus::Passed,
        "{}",
        tested.tools.message
    );
    for _ in 0..4 {
        let request = world.provider.seen.recv_timeout(WAIT).unwrap();
        assert_eq!(request["_test_authorization"], "bearer dummy-setup-only");
    }
    let credentials_before =
        std::fs::read_to_string(world.home().join("credentials.toml")).unwrap_or_default();
    assert!(
        !credentials_before.contains("dummy-setup-only"),
        "Test alone does not save a key"
    );
    client.about(&folder, json!({"method":"application.resources.change","params":protocol::ChangeResource::SaveModel(draft.clone())})).await.unwrap();
    draft.key = None;
    let discovered: Vec<protocol::DiscoveredModel> = serde_json::from_value(client.about(&folder, json!({"method":"application.models.discover","params":{"protocol":"open-ai-chat","base_url":draft.base_url,"auth":"bearer","key":""}})).await.unwrap()).unwrap();
    assert_eq!(
        discovered[0].id, "test-model",
        "the exact saved deployment key is reusable"
    );
    draft.base_url.push_str("/another-account");
    assert!(
        client
            .about(
                &folder,
                json!({"method":"application.models.test","params":{"draft":draft}})
            )
            .await
            .unwrap_err()
            .contains("API key is required")
    );
    let refreshed = client
        .about(
            &folder,
            json!({"method":"application.providers.refresh","params":{}}),
        )
        .await
        .unwrap_err();
    assert!(refreshed.contains("OFFLINE"));
    let after = client
        .about(
            &folder,
            json!({"method":"application.providers","params":{}}),
        )
        .await
        .unwrap();
    assert_eq!(after["revision"], catalogue.revision);
    assert!(
        !client.heard.contains("dummy-setup-only"),
        "replies contain no saved secret"
    );
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
        json!({"accepted": true, "steered": true, "turn": 1})
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

/// A file a command made is an output of the reply that names it; a file the
/// reply only mentions, made before the run, is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reply_shows_the_files_its_run_made_and_only_those() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("named");
    std::fs::write(folder.join("old.png"), b"made earlier").unwrap();
    std::fs::File::options()
        .write(true)
        .open(folder.join("old.png"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let chat = client.open(&folder).await;

    client
        .send(
            &chat,
            "RUN `touch chart.png` then show `chart.png` beside `old.png`",
        )
        .await;
    let (_, approval) = client.until(|frame| frame["method"] == "approval").await;
    let allowed = client
        .ask(
            "approval.respond",
            Some(&chat),
            json!({"gate_id": approval["params"]["gate_id"], "approve": true}),
        )
        .await;
    assert_eq!(allowed["result"]["accepted"], true, "{allowed}");
    client.until(|frame| kind(frame, "turn.done")).await;

    let list = json!({"method":"application.artifacts","params":{"session_id":chat}});
    let listed = client.about(&folder, list).await.unwrap();
    let files: Vec<_> = listed["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["path"].is_string())
        .collect();
    assert_eq!(files.len(), 1, "{listed}");
    let made = folder.canonicalize().unwrap().join("chart.png");
    assert_eq!(files[0]["path"], json!(made.to_string_lossy()));
    assert_eq!(files[0]["kind"], "image");
    assert!(files[0].get("snapshot").is_none(), "no copy is kept");
}

/// A written page is saved once, in its write's own result: reading it back
/// gives exactly what was written, its stylesheet is listed by path for the
/// window to read, and nothing is copied into the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_written_page_is_read_back_from_its_write_and_never_copied() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("written");
    std::fs::write(folder.join("site.css"), "h1{color:blue}").unwrap();
    let chat = client.open(&folder).await;
    let page = "<html><head><link rel=stylesheet href=site.css></head><h1>one</h1></html>";
    client
        .send(&chat, &format!("WRITE index.html\n{page}"))
        .await;
    let (_, approval) = client.until(|frame| frame["method"] == "approval").await;
    let allowed = client
        .ask(
            "approval.respond",
            Some(&chat),
            json!({"gate_id": approval["params"]["gate_id"], "approve": true}),
        )
        .await;
    assert_eq!(allowed["result"]["accepted"], true, "{allowed}");
    client.until(|frame| kind(frame, "turn.done")).await;
    // The model reads that the page was written, not the page again.
    let _call = world.provider.seen.recv_timeout(WAIT).unwrap();
    let after = world.provider.seen.recv_timeout(WAIT).unwrap();
    let result = after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .unwrap_or_default();
    assert_eq!(
        result,
        format!("Wrote index.html (1 lines, {} bytes).", page.len())
    );

    let list = json!({"method":"application.artifacts","params":{"session_id":chat}});
    let listed = client.about(&folder, list).await.unwrap();
    let saved = &listed["revisions"][0];
    assert_eq!(saved["kind"], "page", "{listed}");
    let assets = &saved["snapshot"]["assets"];
    assert_eq!(assets[0]["path"], "site.css", "{saved}");
    assert!(
        assets[0].get("hash").is_none(),
        "the stylesheet is not copied"
    );
    let revision = saved["revision_id"].as_str().unwrap();
    let read = json!({"method":"application.artifacts.read","params":{"session_id":chat,"revision_id":revision,"asset":null}});
    let bytes = client.about(&folder, read).await.unwrap();
    use base64::Engine;
    let text = base64::engine::general_purpose::STANDARD
        .decode(bytes["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(text, page.as_bytes());

    let stored: Vec<_> = walk(&world.home())
        .into_iter()
        .filter(|path| {
            path.components()
                .any(|part| part.as_os_str() == "artifacts")
                && path.is_file()
        })
        .collect();
    assert!(stored.is_empty(), "nothing was copied: {stored:?}");
}

/// A text-only server refuses an image: the turn and the chat still go on, a
/// second image read is declined up front, and the lesson is kept for later chats.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_text_only_server_never_stops_a_chat_that_meets_an_image() {
    const PNG: [u8; 70] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x38,
        0x71, 0xe2, 0xc4, 0x7f, 0x00, 0x08, 0x0c, 0x03, 0x58, 0x1f, 0xff, 0xb6, 0xf3, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("textonly");
    std::fs::write(folder.join("dot.png"), PNG).unwrap();
    let chat = client.open(&folder).await;
    let turn = async |client: &mut Client, text: &str| {
        client.send(&chat, text).await;
        let (_, ended) = client
            .until(|frame| kind(frame, "turn.done") || kind(frame, "turn.error"))
            .await;
        assert!(kind(&ended, "turn.done"), "{text}: {ended}");
    };
    let carried_image = |world: &World| {
        let mut any = false;
        while let Ok(request) = world.provider.seen.try_recv() {
            any |= request.to_string().contains("\"image_url\"");
        }
        any
    };

    turn(&mut client, "TEXT_ONLY_SERVER hello").await;
    assert!(!carried_image(&world));
    // The model reads the image; the server refuses it; the turn finishes anyway.
    turn(&mut client, "READ_PATH dot.png").await;
    assert!(carried_image(&world), "the image was tried once");
    // The image is still in the chat's history, and the chat still goes on.
    turn(&mut client, "ok").await;
    assert!(!carried_image(&world), "it is not sent again");
    // Asked again, the read is declined before an image is made.
    client.send(&chat, "READ_PATH dot.png").await;
    let (_, declined) = client.until(|frame| kind(frame, "tool.observation")).await;
    assert_eq!(declined["params"]["ok"], false, "{declined}");
    assert!(
        declined["params"]["payload"]["error"]
            .as_str()
            .is_some_and(|reason| reason.contains("this model cannot read images")),
        "{declined}"
    );
    client.until(|frame| kind(frame, "turn.done")).await;
    assert!(!carried_image(&world));

    let learned = std::fs::read_to_string(world.home().join("learned_models.json")).unwrap();
    assert!(
        learned.contains("test-model") && learned.contains("false"),
        "{learned}"
    );
}

/// A server that reads images refuses one for its size, in words no list has:
/// the image goes smaller, the chat carries on, and the model is known to see.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_image_too_large_for_its_server_is_sent_smaller() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("large");
    // Noise does not compress, so this PNG is as large as its pixels.
    let mut seed = 7u32;
    let pixels: Vec<u8> = (0..1200 * 1200 * 4)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        })
        .collect();
    let picture = media::from_rgba(1200, 1200, &pixels).unwrap();
    assert!(picture.len() > 4 * 1024 * 1024);
    std::fs::write(folder.join("noise.png"), picture).unwrap();
    let chat = client.open(&folder).await;
    let turn = async |client: &mut Client, text: &str| {
        client.send(&chat, text).await;
        let (_, ended) = client
            .until(|frame| kind(frame, "turn.done") || kind(frame, "turn.error"))
            .await;
        assert!(kind(&ended, "turn.done"), "{text}: {ended}");
    };
    // Whether each request that carried an image was one the server takes.
    let image_requests = |world: &World| {
        let mut taken = Vec::new();
        while let Ok(request) = world.provider.seen.try_recv() {
            let raw = request.to_string();
            if raw.contains("\"image_url\"") {
                taken.push(raw.len() <= SERVER_SIZE_LIMIT);
            }
        }
        taken
    };

    turn(&mut client, "SIZE_LIMIT_SERVER hello").await;
    turn(&mut client, "READ_PATH noise.png").await;
    assert_eq!(
        image_requests(&world),
        [false, true],
        "refused whole, then taken smaller"
    );
    // The next turn starts smaller: nothing is refused a second time.
    turn(&mut client, "ok").await;
    assert_eq!(image_requests(&world), [true]);

    let learned = std::fs::read_to_string(world.home().join("learned_models.json")).unwrap();
    assert!(
        learned.contains("test-model") && learned.contains("true"),
        "a model that took an image is remembered as one that sees: {learned}"
    );
}

/// A message typed in Plan mode reaches the model with the plan instruction,
/// and a person reads only what they typed: in the chat, its title and rewind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plan_instruction_reaches_the_model_and_nowhere_a_person_reads() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("planned");
    let chat = client.open(&folder).await;
    let plan = json!({"mode": "plan"});
    client.ask("session.configure", Some(&chat), plan).await;
    client.send(&chat, "fix the login bug").await;
    client.until(|frame| kind(frame, "turn.done")).await;

    let asked = world.provider.seen.recv_timeout(WAIT).unwrap();
    let told = user_texts(&asked);
    assert!(
        told.contains("Plan mode is active") && told.contains("fix the login bug"),
        "{told}"
    );

    let rewind = client
        .ask("session.rewind.points", Some(&chat), json!({}))
        .await;
    assert_eq!(
        rewind["result"]["points"][0]["text"], "fix the login bug",
        "{rewind}"
    );
    let listed = json!({"method": "sessions.list"});
    let listed = client.about(&folder, listed).await.unwrap().to_string();
    assert!(listed.contains("fix the login bug"), "{listed}");
    assert!(!listed.contains("Plan mode is active"), "{listed}");
    // The test server echoes what it was sent, so only the person's line is checked.
    let events = json!({"method": "sessions.events", "session_id": chat});
    let events = client.about(&folder, events).await.unwrap();
    let typed: Vec<_> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "user")
        .map(|event| event["text"].as_str().unwrap())
        .collect();
    assert_eq!(typed, ["fix the login bug"]);
}

/// The provider's key never travels back to the provider inside a tool result:
/// a command that prints it shows the model a marker, while the record of the
/// command keeps what it printed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_that_prints_the_provider_key_never_shows_it_to_the_model() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("secret");
    let chat = client.open(&folder).await;
    client
        .send(&chat, &format!("RUN `printf 'key=%s' {SECRET}`"))
        .await;
    let (_, approval) = client.until(|frame| frame["method"] == "approval").await;
    client
        .ask(
            "approval.respond",
            Some(&chat),
            json!({"gate_id": approval["params"]["gate_id"], "approve": true}),
        )
        .await;
    let (_, observed) = client.until(|frame| kind(frame, "tool.observation")).await;
    client.until(|frame| kind(frame, "turn.done")).await;
    let _call = world.provider.seen.recv_timeout(WAIT).unwrap();
    let after = world.provider.seen.recv_timeout(WAIT).unwrap();
    let result = after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .unwrap_or_default();
    // The key is in this test's own message and command; what is checked is
    // that the command's output does not carry it back a second way.
    assert!(result.contains("key=[secret removed]"), "{result}");
    assert!(!result.contains(SECRET), "{result}");
    assert!(
        observed["params"]["payload"]["stdout"]
            .as_str()
            .is_some_and(|out| out.contains(SECRET)),
        "the person's own record shows what the command printed: {observed}"
    );
}

/// A window stays current from what each turn adds: every turn sends only its
/// own new outputs, and those are exactly what listing the chat again returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_turn_sends_only_the_outputs_it_added() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("following");
    let chat = client.open(&folder).await;
    let mut pushed = Vec::new();
    for name in ["first.html", "second.html"] {
        client
            .send(
                &chat,
                &format!("WRITE {name}\n<html><h1>{name}</h1></html>"),
            )
            .await;
        let (_, approval) = client.until(|frame| frame["method"] == "approval").await;
        client
            .ask(
                "approval.respond",
                Some(&chat),
                json!({"gate_id": approval["params"]["gate_id"], "approve": true}),
            )
            .await;
        let (_, sent) = client.until(|frame| kind(frame, "outputs")).await;
        let revisions = sent["params"]["revisions"].as_array().unwrap().clone();
        assert_eq!(revisions.len(), 1, "{sent}");
        assert_eq!(revisions[0]["name"], name);
        pushed.extend(revisions);
        client.until(|frame| kind(frame, "turn.done")).await;
    }
    let list = json!({"method":"application.artifacts","params":{"session_id":chat}});
    let listed = client.about(&folder, list).await.unwrap();
    assert_eq!(listed["revisions"], json!(pushed));
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        }
        found.push(path);
    }
    found
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

/// With no short folder of its own, a backend says so. It does not turn to a
/// folder every user shares, where whoever came first could keep it from starting.
#[cfg(target_os = "linux")]
#[test]
fn a_backend_with_nowhere_of_its_own_for_a_socket_says_so_and_uses_no_shared_folder() {
    use std::os::unix::fs::PermissionsExt;
    let world = World::new();
    let home = world.root.path().join("d".repeat(120)).join("home");
    let temporary = world.root.path().join("t".repeat(120));
    let shared = world.folder("s");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&temporary).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();

    // No folder for the session's sockets at all, and then one that anyone may write in.
    for session in [None, Some(&shared)] {
        let mut serve = medha(&home, &world.provider);
        serve.arg("serve").env("TMPDIR", &temporary);
        match session {
            Some(shared) => serve.env("XDG_RUNTIME_DIR", shared),
            None => serve.env_remove("XDG_RUNTIME_DIR"),
        };
        let mut child = serve
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + WAIT;
        while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let started = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        let said = child.wait_with_output().unwrap();
        let said = String::from_utf8_lossy(&said.stderr);
        assert!(!started, "a backend started with nowhere of its own");
        assert!(said.contains("too long for a local socket"), "{said}");
        assert!(!home.join("serve/address").exists());
    }
}

/// A socket's path may be only about a hundred bytes; a home may be far deeper.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_whose_home_is_too_deep_for_a_socket_is_still_reached() {
    let world = World::new();
    let home = world.root.path().join("d".repeat(120)).join("home");
    std::fs::create_dir_all(&home).unwrap();
    let temporary = world.root.path().join("t".repeat(120));
    std::fs::create_dir_all(&temporary).unwrap();
    let temporary = temporary.display().to_string();
    // Linux keeps a session's sockets in a folder of the user's own. macOS has
    // none, and is asked for the user's temporary folder whatever TMPDIR says.
    // As the system makes that folder: nobody else may write in it.
    let session = world.folder("run");
    std::fs::set_permissions(
        &session,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let session = session.display().to_string();
    let own = if cfg!(target_os = "macos") {
        ""
    } else {
        &session
    };
    let env = [("TMPDIR", temporary.as_str()), ("XDG_RUNTIME_DIR", own)];
    let backend = Backend::start(&home, &world.provider, &env);
    let mut client = backend.connect().await;
    let hello = client.ask("hello", None, json!({})).await;
    assert_eq!(hello["result"]["protocol"], 1);
    let address = std::fs::read_to_string(backend.file("address")).unwrap();
    assert!(
        !address.starts_with("/tmp/medha-") && !address.starts_with("/private/tmp/medha-"),
        "the socket is in a folder every user shares: {address}"
    );
    let chat = client.open(&world.folder("w")).await;
    let settings = client.ask("session.settings", Some(&chat), json!({})).await;
    assert!(settings.get("result").is_some(), "{settings}");
    let status = client.ask("backend.status", None, json!({})).await;
    assert_eq!(status["result"]["chats"], 1);
    assert_eq!(status["result"]["resources"]["folders"]["open"], 1);
    assert_eq!(
        status["result"]["resources"]["folder_requests"]["active_limit"],
        8
    );
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

/// A backend told to stop may have work it cannot stop: a save waiting on a
/// lock another program holds. Until that work is gone no other backend may
/// take its place, and it must not be done after one has.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_told_to_stop_gives_way_to_another_only_once_its_own_work_is_over() {
    let world = World::new();
    let folder = world.folder("w");
    let mut backend = world.backend();
    let mut client = backend.connect().await;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(world.home().join("credentials.lock"))
        .unwrap();
    held.lock().unwrap();
    let save = json!({"id": 9401, "folder": folder, "method": "settings.keys.set",
        "params": {"group": "search", "id": "tavily", "key": "sk-zz-never-saved"}});
    assert!(wire::write_frame(&mut client.writer, &save).await);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let told = Command::new("kill")
        .args(["-TERM", &backend.child.id().to_string()])
        .status()
        .unwrap();
    assert!(told.success());
    let deadline = Instant::now() + WAIT;
    while backend.file("address").exists() {
        assert!(Instant::now() < deadline, "the backend ignored the signal");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // It is still here, waiting on its save, and so nothing takes its place.
    assert!(backend.child.try_wait().unwrap().is_none());
    let second = medha(&world.home(), &world.provider)
        .arg("serve")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&second.stderr);
    assert!(
        !second.status.success() && said.contains("already running"),
        "another backend started beside one still at work: {said}"
    );

    // It does not wait for ever: it goes, and the save it could not finish goes with it.
    while backend.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the backend never stopped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    held.unlock().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let saved = std::fs::read_to_string(world.home().join("credentials.toml")).unwrap_or_default();
    assert!(
        !saved.contains("sk-zz-never-saved"),
        "a backend that had stopped went on to save a key"
    );
    let next = world.backend();
    let hello = next.connect().await.ask("hello", None, json!({})).await;
    assert_eq!(hello["result"]["protocol"], 1);
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
async fn editor_and_application_commands_share_settings_and_policy() {
    let world = World::new();
    let folder = world.folder("w");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    let mut editor = EditorProcess::start(&folder, &world.home(), &world.provider, &[]);
    for (method, params) in [
        ("session.settings", json!({})),
        ("session.configure", json!({"mode":"plan"})),
        ("session.settings", json!({})),
    ] {
        let application = outcome(&client.ask(method, Some(&chat), params.clone()).await);
        let external = outcome(&editor.ask(method, params));
        assert_eq!(application, external, "{method}");
    }
    assert!(
        editor
            .ask("session.configure", json!({"mode":"bad"}))
            .get("error")
            .is_some()
    );
    assert_eq!(
        editor.ask("session.settings", json!({}))["result"]["mode"],
        "plan"
    );
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
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
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
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
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
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
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
    let mut alone = EditorProcess::start(&folder, &world.home(), &world.provider, &env);

    let in_backend = outcome(&client.ask("session.settings", Some(&chat), json!({})).await);
    assert_eq!(in_backend["mode"], "plan", "{in_backend}");
    let typed: protocol::Settings = serde_json::from_value(in_backend.clone()).unwrap();
    assert_eq!(typed.mode, protocol::Mode::Plan);
    assert_eq!(typed.protocol.as_deref(), Some("open-ai-chat"));
    assert!(typed.image_input.is_some());
    assert!(typed.image_support.is_some());
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

/// Harmless implementation of the drawing app's documented checkpoint protocol.
/// Both the model and server are loopback fixtures; nothing is sent to a provider.
struct DrawingServer {
    url: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DrawingServer {
    fn start() -> Self {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut checkpoints: HashMap<String, Value> = HashMap::new();
            let mut serial = 0;
            while !stopped.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                // On macOS an accepted socket inherits the listener's
                // non-blocking mode; a request still in flight would then read
                // as nothing and be dropped, failing the call under load.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let Some((_, body)) = read_request(&mut stream) else {
                    continue;
                };
                let asked: Value = serde_json::from_slice(&body).unwrap_or_default();
                let result = match asked["method"].as_str() {
                    Some("initialize") => {
                        json!({"protocolVersion":"2025-06-18", "capabilities":{"tools":{},"resources":{}}, "serverInfo":{"name":"drawing-fixture","version":"1"}})
                    }
                    Some("tools/list") => json!({"tools":[
                        {"name":"create_view","description":"Show a drawing", "inputSchema":{"type":"object","properties":{"elements":{"type":"string"}},"required":["elements"]}, "_meta":{"ui":{"resourceUri":"ui://excalidraw/mcp-app.html"}}},
                        {"name":"read_checkpoint","description":"Read a drawing checkpoint", "inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}, "_meta":{"ui":{"visibility":["app"]}}},
                        {"name":"save_checkpoint","description":"Save a working checkpoint", "inputSchema":{"type":"object","properties":{"id":{"type":"string"},"data":{"type":"string"}},"required":["id","data"]}, "_meta":{"ui":{"visibility":["app"]}}}
                    ]}),
                    Some("resources/read") => {
                        json!({"contents":[{"uri":"ui://excalidraw/mcp-app.html","mimeType":"text/html","text":"<!doctype html><p>Drawing fixture</p>"}]})
                    }
                    Some("tools/call") => {
                        let args = &asked["params"]["arguments"];
                        match asked["params"]["name"].as_str() {
                            Some("create_view") => {
                                let elements: Vec<Value> =
                                    serde_json::from_str(args["elements"].as_str().unwrap())
                                        .unwrap();
                                let elements = if let Some(restore) = elements
                                    .iter()
                                    .find(|element| element["type"] == "restoreCheckpoint")
                                {
                                    checkpoints[restore["id"].as_str().unwrap()]["elements"].clone()
                                } else {
                                    json!(elements)
                                };
                                serial += 1;
                                let id = format!("scene-{serial}");
                                checkpoints.insert(id.clone(), json!({"elements":elements}));
                                json!({"content":[{"type":"text","text":"Drawing ready"}],"structuredContent":{"checkpointId":id}})
                            }
                            Some("read_checkpoint") => {
                                let state = &checkpoints[args["id"].as_str().unwrap()];
                                json!({"content":[{"type":"text","text":state.to_string()}]})
                            }
                            Some("save_checkpoint") => {
                                let id = args["id"].as_str().unwrap();
                                assert!(checkpoints.contains_key(id));
                                let state: Value =
                                    serde_json::from_str(args["data"].as_str().unwrap()).unwrap();
                                checkpoints.insert(id.into(), state);
                                json!({"content":[{"type":"text","text":"Saved working copy"}]})
                            }
                            _ => {
                                json!({"isError":true,"content":[{"type":"text","text":"Unknown fixture tool"}]})
                            }
                        }
                    }
                    _ => json!({}),
                };
                respond(
                    &mut stream,
                    "application/json",
                    &json!({"jsonrpc":"2.0","id":asked["id"],"result":result}).to_string(),
                );
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for DrawingServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drawing_is_saved_as_its_call_recorded_it_and_its_screen_calls_back_only_as_its_connection()
 {
    use base64::Engine;
    let world = World::new();
    let drawing = DrawingServer::start();
    std::fs::create_dir_all(world.home()).unwrap();
    std::fs::write(
        world.home().join("config.toml"),
        format!(
            "[mcp.drawing]\nurl = \"{}\"\ntrust = \"trusted\"\n",
            drawing.url
        ),
    )
    .unwrap();
    let backend = world.backend();
    let folder = world.folder("artifacts");
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    client
        .until(|frame| {
            frame["method"] == "mcp.status"
                && frame["params"]["servers"]
                    .as_array()
                    .is_some_and(|servers| {
                        servers.iter().any(|server| {
                            server["server"] == "drawing" && server["state"] == "ready"
                        })
                    })
        })
        .await;
    let accepted = client
        .send(&chat, "DRAW [{\"id\":\"shape\",\"x\":1}]")
        .await;
    assert!(accepted.get("error").is_none(), "{accepted}");
    let (_, approval) = client.until(|frame| frame["method"] == "approval").await;
    let allowed = client
        .ask(
            "approval.respond",
            Some(&chat),
            json!({"gate_id":approval["params"]["gate_id"],"approve":true}),
        )
        .await;
    assert_eq!(allowed["result"]["accepted"], true, "{allowed}");
    client.until(|frame| kind(frame, "turn.done")).await;
    let list = json!({"method":"application.artifacts","params":{"session_id":chat}});
    let listed = client.about(&folder, list.clone()).await.unwrap();
    let first = &listed["revisions"][0];
    assert_eq!(
        first["screen"]["input"]["elements"], "[{\"id\":\"shape\",\"x\":1}]",
        "{first}"
    );
    assert!(
        first["screen"].get("checkpoints").is_none(),
        "nothing is fetched from the server after the call: {first}"
    );
    let revision = first["revision_id"].as_str().unwrap().to_owned();
    let identity = first["screen"]["identity"].as_str().unwrap().to_owned();

    // The screen may call its own server back, and only as the connection it was drawn by.
    let state = json!({"elements":[{"id":"shape","x":2}]}).to_string();
    let call = |identity: &str| json!({"server":"drawing","tool":"save_checkpoint","args":{"id":"scene-1","data":state},"identity":identity});
    let refused = client
        .ask("mcp.screen.call", Some(&chat), call("different server"))
        .await;
    assert!(refused.get("error").is_some(), "{refused}");
    let accepted = client
        .ask("mcp.screen.call", Some(&chat), call(&identity))
        .await;
    assert!(accepted.get("error").is_none(), "{accepted}");

    // A saved output is read back exactly as the call recorded it.
    let bytes = client.about(&folder, json!({"method":"application.artifacts.read","params":{"session_id":chat,"revision_id":revision,"asset":null}})).await.unwrap();
    let recorded: Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(bytes["data"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(recorded, first["screen"]);

    // A later drawing is its own output: no server declared a shared document.
    client
        .send(
            &chat,
            "DRAW [{\"type\":\"restoreCheckpoint\",\"id\":\"scene-1\"}]",
        )
        .await;
    let (_, approval) = client.until(|frame| frame["method"] == "approval").await;
    let allowed = client
        .ask(
            "approval.respond",
            Some(&chat),
            json!({"gate_id":approval["params"]["gate_id"],"approve":true}),
        )
        .await;
    assert_eq!(allowed["result"]["accepted"], true, "{allowed}");
    client.until(|frame| kind(frame, "turn.done")).await;
    let listed = client.about(&folder, list).await.unwrap();
    let records = listed["revisions"].as_array().unwrap();
    assert_eq!(records.len(), 2, "{listed}");
    assert_ne!(records[1]["artifact_id"], records[0]["artifact_id"]);
    assert!(records[1]["predecessor"].is_null());
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

    // The host is already ready when a later chat starts. It must see that
    // state even if no server changes after its subscription.
    let mut later = backend.connect().await;
    later.open(&world.folder("later-viewer")).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while hosted(&later.next().await) != Some(true) {}
    })
    .await
    .expect("a chat missed the already-ready MCP snapshot");

    std::fs::remove_file(&config).unwrap();
    while hosted(&client.next().await) != Some(false) {}
    while hosted(&later.next().await) != Some(false) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolved_terminal_startup_wins_over_the_daemons_unrelated_environment() {
    let world = World::new();
    let backend = world.backend_in(&[
        ("MEDHA_MODE", "invalid-daemon-mode"),
        ("MEDHA_TOOLS", "invalid-daemon-tools"),
    ]);
    let mut client = backend.connect().await;
    let startup = protocol::StartupOptions {
        mode: Some(protocol::Mode::Plan),
        tools_preset: Some("minimal".into()),
        model_env: protocol::ModelOverrides {
            base_url: Some(world.provider.url.clone()),
            model: Some("test-model".into()),
            api_key: Some("caller-secret".into()),
            protocol: Some("open-ai-chat".into()),
            token_accounting: Some("adaptive".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let made = client
        .ask(
            "session.create",
            None,
            json!({"folder": world.folder("caller"), "startup": startup}),
        )
        .await;
    let chat = made["result"]["session"]
        .as_str()
        .unwrap_or_else(|| panic!("{made}"))
        .to_owned();
    client
        .ask("session.attach", Some(&chat), json!({"after": 0}))
        .await;
    let settings = client.ask("session.settings", Some(&chat), json!({})).await;
    assert_eq!(settings["result"]["mode"], "plan");
    assert!(!settings.to_string().contains("caller-secret"));
    client.send(&chat, "what may you use").await;
    assert!(!offered(&world.provider.seen.recv_timeout(WAIT).unwrap()).is_empty());
    client.until(|frame| kind(frame, "turn.done")).await;
    let rejected = client
        .ask(
            "session.create",
            None,
            json!({"folder": world.folder("conflict"), "startup": {}, "mode": "plan"}),
        )
        .await;
    assert!(
        rejected["error"]["message"]
            .as_str()
            .unwrap()
            .contains("either")
    );
}

/// The terminal names the chat to resume twice: beside its startup options,
/// where the backend keeps two clients from resuming one chat at once, and
/// inside them, where the chat itself reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_is_resumed_with_startup_options_as_the_terminal_asks_for_it() {
    let world = World::new();
    let folder = world.folder("w");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    client.send(&chat, "remember the word heron").await;
    client.until(|frame| kind(frame, "turn.done")).await;
    world.provider.asked();
    client.ask("session.close", Some(&chat), json!({})).await;
    client
        .until(|frame| frame["method"] == "session.ended")
        .await;

    let startup = protocol::StartupOptions {
        resume: protocol::Resume::Id(chat.clone()),
        model_env: protocol::ModelOverrides {
            base_url: Some(world.provider.url.clone()),
            model: Some("test-model".into()),
            api_key: Some(SECRET.into()),
            protocol: Some("open-ai-chat".into()),
            token_accounting: Some("adaptive".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let another = json!({"folder": folder, "startup": startup,
        "resume": "01ARZ3NDEKTSV4RRFFQ69G5FAV"});
    let refused = client.ask("session.create", None, another).await;
    let why = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(why.contains("not the one"), "{refused}");

    let asked = json!({"folder": folder, "startup": startup, "resume": chat});
    let resumed = client.ask("session.create", None, asked).await;
    assert_eq!(resumed["result"]["session"], chat.as_str(), "{resumed}");
    client
        .ask("session.attach", Some(&chat), json!({"after": 0}))
        .await;
    client.send(&chat, "what was the word").await;
    client.until(|frame| kind(frame, "turn.done")).await;
    let seen = world.provider.asked();
    assert!(
        seen.contains("remember the word heron") && seen.contains("what was the word"),
        "the resumed chat lost its history: {seen}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_force_stop_settles_all_viewers_before_another_turn_runs() {
    let world = World::new();
    let backend = world.backend();
    let (mut first, mut second) = (backend.connect().await, backend.connect().await);
    let chat = first.open(&world.folder("force-stop")).await;
    second.ask("session.attach", Some(&chat), json!({})).await;
    first.send(&chat, "HOLD the answer").await;
    world.provider.asked();
    let stopped = second.ask("turn.abort", Some(&chat), json!({})).await;
    assert_eq!(stopped["result"]["accepted"], true, "{stopped}");
    for viewer in [&mut first, &mut second] {
        viewer
            .until(|frame| kind(frame, "turn.abort_settled"))
            .await;
    }
    // Release only this test's held provider request; no old owner may resume
    // its output into the next turn.
    world.provider.release.send(()).unwrap();
    first.send(&chat, "a fresh turn").await;
    assert!(world.provider.asked().contains("a fresh turn"));
    let mut text = String::new();
    loop {
        let (_, frame) = first
            .until(|frame| kind(frame, "model.text") || kind(frame, "turn.done"))
            .await;
        if kind(&frame, "turn.done") {
            break;
        }
        text.push_str(frame["params"]["delta"].as_str().unwrap());
    }
    // The stopped message was never answered, so it arrives joined to this one.
    assert_eq!(text, "echo: HOLD the answer\n\na fresh turn");
}

/// The window removes a server for the folder, the shared host lets go of it at
/// once, and only then is each open chat told to look again. By then the server
/// is nobody's to remove, which is what was asked for and no failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_told_to_look_again_after_its_server_was_removed_does_not_fail() {
    let world = World::new();
    let folder = world.folder("w");
    let (url, _) = remote_mcp_server();
    std::fs::create_dir_all(world.home()).unwrap();
    let saved = format!("[mcp.hosted]\nurl = \"{url}\"\ntrust = \"trusted\"\n");
    std::fs::write(world.home().join("config.toml"), saved).unwrap();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    let hosted = |frame: &Value| {
        let said = &frame["params"]["frame"];
        said["method"] == "mcp.status"
            && said["params"]["servers"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|server| server["server"] == "hosted" && server["state"] == "ready")
    };
    while !hosted(&client.next().await) {}

    let remove = json!({"method": "settings.mcp.remove", "params": {"id": "hosted"}});
    client
        .about(&folder, remove)
        .await
        .expect("the server is removed");
    // More than once: a first failure used to be met again on every later change.
    for _ in 0..2 {
        let looked = client
            .ask("extensions.reload", Some(&chat), json!({}))
            .await;
        assert!(looked["result"]["warnings"].is_array(), "{looked}");
    }
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

/// A new version of an output is linked by what the person's message names,
/// sent as data. The model reads which output in words and never sees an ID,
/// and nothing it writes back can name one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_about_an_output_links_its_reply_and_shows_the_model_no_id() {
    let world = World::new();
    let backend = world.backend();
    let folder = world.folder("revisions");
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    let list = json!({"method":"application.artifacts","params":{"session_id":chat}});
    let drawn = |body: &str| format!("drawn\n```mermaid\nflowchart LR\n{body}\n```");
    macro_rules! turn {
        ($params:expr) => {{
            let accepted = client.ask("message.send", Some(&chat), $params).await;
            assert!(accepted.get("error").is_none(), "{accepted}");
            let (_, shown) = client.until(|frame| kind(frame, "message.accepted")).await;
            let asked = world.provider.asked();
            client.until(|frame| kind(frame, "turn.done")).await;
            let listed = client.about(&folder, list.clone()).await.unwrap();
            // What every surface shows for this message is the person's own
            // words, live and again from saved history.
            assert_eq!(shown["params"]["content"], $params["content"], "{shown}");
            let saved = client
                .ask("session.presentation", Some(&chat), json!({}))
                .await;
            let saved: protocol::PresentationSnapshot =
                serde_json::from_value(saved["result"].clone()).unwrap();
            let last = saved.items.iter().rev().find_map(|item| match item {
                protocol::PresentationItem::User { text, about, .. } => {
                    Some((text.clone(), about.clone()))
                }
                _ => None,
            });
            assert_eq!(
                last,
                Some((
                    $params["content"].as_str().unwrap().to_owned(),
                    shown["params"]["about"].as_str().map(str::to_owned)
                )),
            );
            (
                asked,
                listed["revisions"].as_array().unwrap().clone(),
                shown["params"]["about"].clone(),
            )
        }};
    }

    let (_, records, label) = turn!(json!({"content": drawn("A-->B")}));
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(label.is_null(), "{label}");
    let first = records[0].clone();
    let about = json!({"artifact_id": first["artifact_id"], "revision_id": first["revision_id"]});

    let (asked, records, label) = turn!(json!({"content": drawn("A-->C"), "about": about}));
    assert_eq!(label, "Diagram");
    assert!(
        asked.contains("About the diagram you made earlier:"),
        "{asked}"
    );
    assert!(
        !asked.contains("medha-output") && !asked.contains("artifact"),
        "{asked}"
    );
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1]["artifact_id"], first["artifact_id"]);
    assert_eq!(records[1]["predecessor"], first["revision_id"]);

    // An ordinary follow-up, and a reference to an output that does not exist.
    let (_, records, _) = turn!(json!({"content": drawn("X-->Y")}));
    assert_ne!(records[2]["artifact_id"], first["artifact_id"]);
    assert!(records[2]["predecessor"].is_null(), "{records:?}");
    let forged =
        json!({"artifact_id": "inline:someone-elses:diagram", "revision_id": first["revision_id"]});
    let (asked, records, label) = turn!(json!({"content": drawn("P-->Q"), "about": forged}));
    assert!(label.is_null(), "{label}");
    let latest = asked.rsplit(" | ").next().unwrap();
    assert!(!latest.contains("About the diagram"), "{latest}");
    assert_ne!(records[3]["artifact_id"], first["artifact_id"]);
    assert!(records[3]["predecessor"].is_null(), "{records:?}");

    let malformed = client
        .ask(
            "message.send",
            Some(&chat),
            json!({"content": "x", "about": "the first one"}),
        )
        .await;
    assert!(malformed.get("error").is_some(), "{malformed}");

    // A fresh backend rebuilds the chat from its log alone: the same words,
    // the same label, and the same link.
    drop(client);
    let mut backend = backend;
    backend.child.kill().unwrap();
    backend.child.wait().unwrap();
    drop(backend);
    let backend = world.backend();
    let mut client = backend.connect().await;
    let resumed = client.create(&folder, Some(&chat)).await;
    assert_eq!(resumed["result"]["session"], chat.as_str(), "{resumed}");
    client
        .ask("session.attach", Some(&chat), json!({"after": 0}))
        .await;
    let saved = client
        .ask("session.presentation", Some(&chat), json!({}))
        .await;
    let saved: protocol::PresentationSnapshot =
        serde_json::from_value(saved["result"].clone()).unwrap();
    let said: Vec<(String, Option<String>)> = saved
        .items
        .iter()
        .filter_map(|item| match item {
            protocol::PresentationItem::User { text, about, .. } => {
                Some((text.clone(), about.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        said,
        vec![
            (drawn("A-->B"), None),
            (drawn("A-->C"), Some("Diagram".to_owned())),
            (drawn("X-->Y"), None),
            (drawn("P-->Q"), None),
        ]
    );
    let listed = client.about(&folder, list).await.unwrap();
    assert_eq!(listed["revisions"][1]["predecessor"], first["revision_id"]);
}

/// A model that sends a drawing's elements as a list, where the server's
/// schema takes JSON text, still draws: the call is repaired before it is sent,
/// and the server receives exactly what it asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_list_sent_where_a_server_takes_text_is_repaired_before_the_call() {
    let world = World::new();
    let drawing = DrawingServer::start();
    std::fs::create_dir_all(world.home()).unwrap();
    std::fs::write(
        world.home().join("config.toml"),
        format!(
            "[mcp.drawing]\nurl = \"{}\"\ntrust = \"trusted\"\n",
            drawing.url
        ),
    )
    .unwrap();
    let backend = world.backend();
    let folder = world.folder("repaired");
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    client
        .until(|frame| {
            frame["method"] == "mcp.status"
                && frame["params"]["servers"]
                    .as_array()
                    .is_some_and(|servers| {
                        servers.iter().any(|server| {
                            server["server"] == "drawing" && server["state"] == "ready"
                        })
                    })
        })
        .await;
    let screens = client.ask("mcp.screens", Some(&chat), json!({})).await;
    let offered = &screens["result"]["tools"][0];
    assert_eq!(offered["text_fields"], json!(["elements"]), "{screens}");

    let accepted = client
        .send(&chat, "DRAWLIST [{\"id\":\"shape\",\"x\":7}]")
        .await;
    assert!(accepted.get("error").is_none(), "{accepted}");
    let (_, approval) = client.until(|frame| frame["method"] == "approval").await;
    let allowed = client
        .ask(
            "approval.respond",
            Some(&chat),
            json!({"gate_id":approval["params"]["gate_id"],"approve":true}),
        )
        .await;
    assert_eq!(allowed["result"]["accepted"], true, "{allowed}");
    client.until(|frame| kind(frame, "turn.done")).await;
    let list = json!({"method":"application.artifacts","params":{"session_id":chat}});
    let saved = client.about(&folder, list).await.unwrap();
    let drawn = &saved["revisions"][0]["screen"];
    let sent: Value = serde_json::from_str(
        drawn["input"]["elements"]
            .as_str()
            .unwrap_or_else(|| panic!("the call records the text the server was sent: {drawn}")),
    )
    .unwrap();
    assert_eq!(sent[0]["x"], 7, "{drawn}");
    assert_eq!(
        drawn["result"]["structuredContent"]["checkpointId"], "scene-1",
        "the server parsed the text it was sent: {drawn}"
    );
    // The request after the call carries its result back to the model.
    let _call = world.provider.seen.recv_timeout(WAIT).unwrap();
    let after = world.provider.seen.recv_timeout(WAIT).unwrap();
    let result = after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .map(|message| message["content"].to_string())
        .unwrap_or_default();
    assert!(
        result.contains("was sent as an array and passed as a string"),
        "the model is told what was changed: {result}"
    );
}
