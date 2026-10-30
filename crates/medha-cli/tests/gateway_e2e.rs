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

/// What the gateway sent to the platform: a new message with its id, or a change to one.
#[derive(Debug)]
struct Sent {
    id: String,
    text: String,
    buttons: Vec<Button>,
    edit: bool,
}

struct FakePlatform {
    heard: Mutex<channel::Receiver<Heard>>,
    sent: channel::UnboundedSender<Sent>,
    ids: std::sync::atomic::AtomicU64,
}

impl FakePlatform {
    fn record(&self, text: &str, buttons: &[Button]) -> String {
        let id = self
            .ids
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string();
        let _ = self.sent.send(Sent {
            id: id.clone(),
            text: text.into(),
            buttons: buttons.to_vec(),
            edit: false,
        });
        id
    }
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
        self.record(text, &[]);
        Ok(())
    }

    async fn ask(
        &self,
        _chat: &str,
        text: &str,
        buttons: &[Button],
    ) -> anyhow::Result<Option<String>> {
        Ok(Some(self.record(text, buttons)))
    }

    async fn send_live(&self, _chat: &str, text: &str) -> anyhow::Result<Option<String>> {
        Ok(Some(self.record(text, &[])))
    }

    async fn edit(&self, _chat: &str, message: &str, text: &str) -> anyhow::Result<()> {
        let _ = self.sent.send(Sent {
            id: message.into(),
            text: text.into(),
            buttons: Vec::new(),
            edit: true,
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

    /// A running gateway on a fresh fake platform, with chats in the test's folder.
    fn gateway(&self) -> Gateway {
        self.gateway_with(vec![self.folder.clone()])
    }

    /// A running gateway whose chats may work in `folders`, the first being the default.
    fn gateway_with(&self, folders: Vec<PathBuf>) -> Gateway {
        self.gateway_set(gateway::Settings::Fixed {
            folders,
            steps: false,
        })
    }

    fn gateway_set(&self, settings: gateway::Settings) -> Gateway {
        let (tell, heard) = channel::channel(16);
        let (sent_tx, sent) = channel::unbounded_channel();
        let platform = Arc::new(FakePlatform {
            heard: Mutex::new(heard),
            sent: sent_tx,
            ids: std::sync::atomic::AtomicU64::new(1),
        });
        let router = gateway::Router::in_home(
            platform,
            Arc::clone(&self.client),
            self.folder.clone(),
            settings,
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
fn new_offers_only_the_allowed_projects_and_a_removed_one_is_refused() {
    let world = World::start();
    runtime().block_on(projects(&world));
}

/// The folder each live chat works in, as the backend reports it.
async fn folders_in_use(world: &World) -> Vec<PathBuf> {
    let listed = world
        .client
        .connection()
        .unwrap()
        .call_async(protocol::Scope::Service, &protocol::ListSessions {})
        .await
        .unwrap();
    listed
        .sessions
        .into_iter()
        .filter_map(|live| Some(live.about?.folder))
        .collect()
}

async fn projects(world: &World) {
    world.pair(OWNER);
    let site = world.root.path().join("site");
    std::fs::create_dir_all(&site).unwrap();
    let site = site.canonicalize().unwrap();
    let mut gateway = world.gateway_with(vec![world.folder.clone(), site.clone()]);

    gateway.write(OWNER, "/new").await;
    let asked = gateway.next().await;
    let labels: Vec<_> = asked.buttons.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(labels, ["work", "site"], "{asked:?}");
    // A button names a place in the list, never a path.
    assert!(
        asked.buttons.iter().all(|b| !b.choice.contains('/')),
        "{asked:?}"
    );
    // A path typed on the phone is not taken.
    gateway.write(OWNER, "/new /etc").await;
    assert!(gateway.next().await.text.contains("no project called /etc"));

    gateway.press(OWNER, OWNER, &asked.buttons[1].choice).await;
    let chosen = gateway.next().await;
    assert!(chosen.edit && chosen.id == asked.id, "{chosen:?}");
    assert_eq!(chosen.text, format!("{}\n\nYou chose site.", asked.text));
    let moved = gateway.next().await.text;
    assert!(moved.contains(&site.display().to_string()), "{moved}");
    gateway.write(OWNER, "hello from site").await;
    assert_eq!(gateway.next().await.text, "echo: hello from site");
    world.seen.recv_timeout(WAIT).unwrap();
    assert!(folders_in_use(world).await.contains(&site));
    drop(gateway);

    // Taken off the list on the computer: the chat is told, and works in the default again.
    let mut restarted = world.gateway_with(vec![world.folder.clone()]);
    restarted.write(OWNER, "hello again").await;
    let told = restarted.next().await.text;
    assert!(told.contains("no longer open to messages"), "{told}");
    assert_eq!(restarted.next().await.text, "echo: hello again");
    world.seen.recv_timeout(WAIT).unwrap();
    let in_use = folders_in_use(world).await;
    assert!(in_use.contains(&world.folder), "{in_use:?}");
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
    let mut seen = Vec::new();
    loop {
        let sent = gateway.next().await;
        if sent.text == "echo: RUN the command" {
            break;
        }
        seen.push(sent);
    }
    assert!(
        world.folder.join("made-by-the-test.txt").exists(),
        "the approved command did not run"
    );
    // The request stays where it was asked, its buttons gone and the choice under it.
    let settled = seen
        .iter()
        .find(|sent| sent.edit && sent.id == asked.id)
        .expect("the approval was not settled in place");
    assert!(
        settled.text.starts_with("Medha asks to:"),
        "{}",
        settled.text
    );
    assert!(
        settled.text.ends_with("\n\n✓ Yes, allow once"),
        "{}",
        settled.text
    );
    assert!(
        !seen.iter().any(|sent| sent.text.contains("Ran `touch")),
        "steps are off unless chosen: {seen:?}"
    );
}

#[test]
fn chosen_steps_show_as_one_message_marked_as_they_end() {
    let world = World::start();
    runtime().block_on(steps_shown(&world));
}

async fn steps_shown(world: &World) {
    world.pair(OWNER);
    let mut gateway = world.gateway_set(gateway::Settings::Fixed {
        folders: vec![world.folder.clone()],
        steps: true,
    });
    gateway.write(OWNER, "RUN the command").await;
    let step = "Ran `touch made-by-the-test.txt`";
    let running = gateway.next().await;
    assert_eq!(
        running.text,
        format!("◦ {step}"),
        "the step comes before its approval"
    );
    let asked = gateway.until(|sent| !sent.buttons.is_empty()).await;
    let allow = asked.buttons[0].choice.clone();
    gateway.press(OWNER, OWNER, &allow).await;
    let mut seen = Vec::new();
    loop {
        let sent = gateway.next().await;
        if sent.text == "echo: RUN the command" {
            break;
        }
        seen.push(sent);
    }
    let marked = seen
        .iter()
        .find(|sent| sent.edit && sent.id == running.id)
        .unwrap_or_else(|| panic!("the step was not marked in its message: {seen:?}"));
    assert_eq!(marked.text, format!("✓ {step}"));
    assert!(
        seen.iter().filter(|sent| !sent.edit).count() == 0,
        "one message for the steps, not one per change: {seen:?}"
    );
}
