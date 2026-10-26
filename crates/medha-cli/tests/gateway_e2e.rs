//! The messaging gateway against a real backend and a scripted model: strangers never reach
//! the agent, approvals travel as buttons only a paired person can press, and a chat carries
//! on after the gateway restarts.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use gateway::pairing::Pairing;
use gateway::platform::{Button, Heard, Incoming, Platform};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc as channel};

const WAIT: Duration = Duration::from_secs(60);
const OWNER: &str = "1001";

/// Echoes the last user message; one containing RUN first asks to run `touch made-by-the-test.txt`.
fn answer(mut stream: TcpStream, seen: &mpsc::Sender<Value>) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut request = Vec::new();
    let body = loop {
        let mut chunk = [0u8; 8192];
        let Ok(read) = stream.read(&mut chunk) else {
            return;
        };
        if read == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..read]);
        let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
        let length: usize = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
            .unwrap_or(0);
        if request.len() >= end + 4 + length {
            if !headers.starts_with("post /v1/chat/completions") {
                return reply(
                    &mut stream,
                    "application/json",
                    r#"{"data":[{"id":"test-model","context_length":128000}]}"#,
                );
            }
            break serde_json::from_slice::<Value>(&request[end + 4..end + 4 + length])
                .unwrap_or(Value::Null);
        }
    };
    let _ = seen.send(body.clone());
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let asked = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .unwrap_or_default();
    let ran = messages.iter().any(|m| m["role"] == "tool");
    let delta = if asked.contains("RUN") && !ran {
        let shell = body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .find(|name| name.contains("shell") && name.contains("exec"))
            .unwrap_or("shell_exec");
        let arguments = json!({"command": "touch made-by-the-test.txt", "network": false});
        json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
            "function": {"name": shell, "arguments": arguments.to_string()}}]})
    } else {
        json!({"content": format!("echo: {}", asked.trim_matches('"'))})
    };
    let finish = if delta.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    let events = [
        json!({"choices": [{"index": 0, "delta": delta}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}),
    ];
    let body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    reply(&mut stream, "text/event-stream", &body);
}

fn reply(stream: &mut TcpStream, kind: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// What the gateway sent to the platform.
#[derive(Debug)]
struct Sent {
    text: String,
    buttons: Vec<Button>,
}

struct FakePlatform {
    heard: Mutex<channel::Receiver<Heard>>,
    sent: channel::UnboundedSender<Sent>,
}

#[async_trait::async_trait]
impl Platform for FakePlatform {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn limit(&self) -> usize {
        4096
    }

    async fn hear(&self) -> anyhow::Result<Vec<Heard>> {
        Ok(self.heard.lock().await.recv().await.into_iter().collect())
    }

    async fn send(&self, _chat: &str, text: &str) -> anyhow::Result<()> {
        let _ = self.sent.send(Sent {
            text: text.into(),
            buttons: Vec::new(),
        });
        Ok(())
    }

    async fn ask(&self, _chat: &str, text: &str, buttons: &[Button]) -> anyhow::Result<()> {
        let _ = self.sent.send(Sent {
            text: text.into(),
            buttons: buttons.to_vec(),
        });
        Ok(())
    }
}

/// The backend process; it is stopped with the test.
struct Serving(std::process::Child);

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct World {
    root: tempfile::TempDir,
    _serving: Serving,
    client: Arc<medha_client::Backend>,
    folder: PathBuf,
    seen: mpsc::Receiver<Value>,
}

impl World {
    fn start() -> Self {
        let root = tempfile::Builder::new().prefix("mg").tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (seen_tx, seen) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let seen = seen_tx.clone();
                std::thread::spawn(move || answer(stream, &seen));
            }
        });
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let serving = Serving(
            std::process::Command::new(env!("CARGO_BIN_EXE_medha"))
                .arg("serve")
                .env("MEDHA_HOME", &home)
                .env("MEDHA_CRED_STORE", "file")
                .env("MEDHA_CATALOGUE_OFFLINE", "1")
                .env("MEDHA_BASE_URL", url)
                .env("MEDHA_MODEL", "test-model")
                .env("MEDHA_API_KEY", "test-key")
                .env("MEDHA_PROTOCOL", "open-ai-chat")
                .env("MEDHA_TOKEN_ACCOUNTING", "adaptive")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        // Both files are written only once the backend listens.
        let read = |name: &str| {
            std::fs::read_to_string(home.join("serve").join(name))
                .ok()
                .filter(|text| !text.is_empty())
        };
        let deadline = std::time::Instant::now() + WAIT;
        let (address, token) = loop {
            if let (Some(address), Some(token)) = (read("address"), read("token")) {
                break (address, token);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the backend never listened"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        let client = medha_client::Backend::joined_to(address, &token);
        let folder = root.path().join("work");
        std::fs::create_dir_all(&folder).unwrap();
        let folder = folder.canonicalize().unwrap();
        Self {
            root,
            _serving: serving,
            client,
            folder,
            seen,
        }
    }

    fn gateway_home(&self) -> PathBuf {
        self.root.path().join("gateway")
    }

    fn pairing(&self) -> Pairing {
        Pairing::at(self.gateway_home().join("pairing.json"))
    }

    /// A running gateway on a fresh fake platform.
    fn gateway(&self) -> Gateway {
        let (tell, heard) = channel::channel(16);
        let (sent_tx, sent) = channel::unbounded_channel();
        let platform = Arc::new(FakePlatform {
            heard: Mutex::new(heard),
            sent: sent_tx,
        });
        let router = gateway::Router::in_home(
            platform,
            Arc::clone(&self.client),
            self.folder.clone(),
            &self.gateway_home(),
        );
        Gateway {
            tell,
            sent,
            task: tokio::spawn(router.run()),
        }
    }

    fn pair(&self, user: &str) {
        let code = self
            .pairing()
            .request("fake", user, "Owner")
            .unwrap()
            .unwrap();
        self.pairing().approve("fake", &code).unwrap().unwrap();
    }
}

struct Gateway {
    tell: channel::Sender<Heard>,
    sent: channel::UnboundedReceiver<Sent>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Gateway {
    async fn write(&self, user: &str, text: &str) {
        let message = Incoming {
            chat: format!("chat-{user}"),
            user: user.into(),
            name: "Someone".into(),
            text: text.into(),
            ..Incoming::default()
        };
        self.tell.send(Heard::Message(message)).await.unwrap();
    }

    async fn press(&self, chat_of: &str, user: &str, choice: &str) {
        let pressed = Heard::Pressed {
            chat: format!("chat-{chat_of}"),
            user: user.into(),
            choice: choice.into(),
        };
        self.tell.send(pressed).await.unwrap();
    }

    async fn next(&mut self) -> Sent {
        tokio::time::timeout(WAIT, self.sent.recv())
            .await
            .expect("the gateway said nothing")
            .unwrap()
    }

    async fn until(&mut self, wanted: impl Fn(&Sent) -> bool) -> Sent {
        loop {
            let sent = self.next().await;
            if wanted(&sent) {
                return sent;
            }
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn user_texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].to_string())
        .collect()
}

/// The client joins the backend from outside any async runtime, so each test makes its own after.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_stranger_gets_only_a_code_and_a_paired_chat_carries_on_after_a_restart() {
    let world = World::start();
    runtime().block_on(stranger_then_restart(&world));
}

async fn stranger_then_restart(world: &World) {
    let mut gateway = world.gateway();
    gateway.write("77", "delete everything").await;
    let code = gateway.next().await.text;
    assert!(code.contains("medha pairing approve fake "), "{code}");
    assert!(
        world.seen.recv_timeout(Duration::from_secs(2)).is_err(),
        "a stranger reached the model"
    );

    world.pair(OWNER);
    gateway.write(OWNER, "remember the word heron").await;
    assert_eq!(gateway.next().await.text, "echo: remember the word heron");
    world.seen.recv_timeout(WAIT).unwrap();
    drop(gateway);

    let mut restarted = world.gateway();
    restarted.write(OWNER, "which word?").await;
    assert_eq!(restarted.next().await.text, "echo: which word?");
    let asked = user_texts(&world.seen.recv_timeout(WAIT).unwrap());
    assert!(
        asked.iter().any(|text| text.contains("heron")),
        "the conversation did not carry on: {asked:?}"
    );
}

#[test]
fn an_approval_arrives_as_buttons_only_a_paired_person_can_press() {
    let world = World::start();
    runtime().block_on(approval_by_button(&world));
}

async fn approval_by_button(world: &World) {
    world.pair(OWNER);
    let mut gateway = world.gateway();
    gateway.write(OWNER, "RUN the command").await;
    let asked = gateway.until(|sent| !sent.buttons.is_empty()).await;
    assert!(asked.text.starts_with("Medha asks to:"), "{}", asked.text);
    let allow = asked
        .buttons
        .iter()
        .find(|button| button.label == "Yes, allow once")
        .expect("no allow button");
    assert!(
        asked
            .buttons
            .iter()
            .any(|button| button.label == "No, deny")
    );

    gateway.press(OWNER, "4242", &allow.choice).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !world.folder.join("made-by-the-test.txt").exists(),
        "a stranger's press ran the command"
    );

    gateway.press(OWNER, OWNER, &allow.choice).await;
    gateway
        .until(|sent| sent.text == "echo: RUN the command")
        .await;
    assert!(
        world.folder.join("made-by-the-test.txt").exists(),
        "the approved command did not run"
    );
}
