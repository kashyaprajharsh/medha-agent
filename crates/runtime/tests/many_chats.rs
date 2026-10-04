//! One process, two folders, two chats in each: what a single backend has to be able to do.

use kernel::{Message, Role};
use runtime::session::{Start, Started};
use runtime::{Notices, SessionOptions, Surface, Workspace};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Quiet;

impl Notices for Quiet {
    fn say(&self, _: &str) {}
}

/// Replies `echo: <last user message>` so a chat that got another chat's turn is caught.
async fn endpoint() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}/v1", listener.local_addr().unwrap());
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                let head = loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..head]).to_lowercase();
                let len: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |n| n.trim().parse().unwrap());
                while bytes.len() < head + len {
                    let n = socket.read(&mut chunk).await.unwrap();
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let request: serde_json::Value =
                    serde_json::from_slice(&bytes[head..head + len]).unwrap_or_default();
                let last = request["messages"]
                    .as_array()
                    .and_then(|all| all.iter().rev().find(|m| m["role"] == "user"))
                    .map(|m| m["content"].as_str().unwrap_or_default().to_string())
                    .unwrap_or_default();
                let model = request["model"].as_str().unwrap_or_default().to_string();
                seen.lock().unwrap().push((model, last.clone()));
                let body = serde_json::json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": format!("echo: {last}")},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    (address, asked)
}

fn open(folder: &Path) -> Workspace {
    std::fs::create_dir_all(folder).unwrap();
    let workspace = Workspace::open(folder.to_path_buf(), &Quiet).unwrap();
    std::fs::create_dir_all(workspace.state.join("logs")).unwrap();
    workspace
}

async fn chat(workspace: &Workspace, endpoint: &str, model: &str) -> Started {
    let mut lock = runtime::workspace::load_lock(&workspace.given, &Quiet).unwrap();
    lock.reasoning.stream = Some(false);
    let options = SessionOptions {
        model_env: runtime::config::ModelEnv {
            base_url: Some(endpoint.into()),
            model: Some(model.into()),
            api_key: Some("k".into()),
            max_ctx: Some("32000".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let model = runtime::model::resolve(&lock, &options, &Quiet)
        .await
        .unwrap();
    runtime::session::start(
        Start {
            lock: &lock,
            options: &options,
            workspace,
            model,
            autonomy: kernel::AutonomyLevel::Careful,
            verify_command: None,
            verify_required: false,
            verify_timeout: std::time::Duration::from_secs(60),
            notices: &Quiet,
        },
        |_, _| Surface {
            gate: Arc::new(kernel::AutoDeny),
            asker: Arc::new(kernel::NoAsker),
            agents: None,
        },
    )
    .await
    .unwrap()
}

async fn say(chat: &Started, text: &str) -> String {
    let messages = vec![
        Message::system(chat.system.clone()),
        Message::new(Role::User, text),
    ];
    let (history, _) = chat
        .kernel
        .run_session(
            &chat.session,
            messages,
            chat.base_budget.clone().with_fresh_pool(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    history.last().unwrap().content.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_folders_with_two_chats_each_run_in_one_process_without_crossing() {
    let root = tempfile::tempdir().unwrap();
    let (address, asked) = endpoint().await;
    // SAFETY: this file holds one test, so nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("MEDHA_HOME", root.path().join("home")) };
    let home_a = open(&root.path().join("a"));
    let home_b = open(&root.path().join("b"));

    let a1 = chat(&home_a, &address, "model-a1").await;
    let a2 = chat(&home_a, &address, "model-a2").await;
    let b1 = chat(&home_b, &address, "model-b1").await;
    let b2 = chat(&home_b, &address, "model-b2").await;
    assert!(
        Arc::ptr_eq(&a1.log, &a2.log) && Arc::ptr_eq(&b1.log, &b2.log),
        "chats in one folder share its one log connection"
    );
    assert!(!Arc::ptr_eq(&a1.log, &b1.log));

    let (ra1, ra2, rb1, rb2) = tokio::join!(
        say(&a1, "from a1"),
        say(&a2, "from a2"),
        say(&b1, "from b1"),
        say(&b2, "from b2"),
    );
    assert_eq!(
        [ra1, ra2, rb1, rb2],
        [
            "echo: from a1",
            "echo: from a2",
            "echo: from b1",
            "echo: from b2"
        ]
    );
    let mut asked = asked.lock().unwrap().clone();
    asked.sort();
    let pair = |model: &str, text: &str| (model.to_string(), text.to_string());
    assert_eq!(
        asked,
        [
            pair("model-a1", "from a1"),
            pair("model-a2", "from a2"),
            pair("model-b1", "from b1"),
            pair("model-b2", "from b2"),
        ],
        "each chat asked its own model, once"
    );

    assert_ne!(home_a.state, home_b.state);
    let ids = |chat: &Started| -> Vec<_> {
        let mut ids: Vec<_> = chat
            .log
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|session| session.id)
            .collect();
        ids.sort();
        ids
    };
    let mut in_a = vec![a1.session.id, a2.session.id];
    let mut in_b = vec![b1.session.id, b2.session.id];
    in_a.sort();
    in_b.sort();
    assert_eq!(ids(&a1), in_a, "folder a's log holds only its own chats");
    assert_eq!(ids(&b1), in_b, "folder b's log holds only its own chats");
    assert_ne!(a1.workspace.root(), b1.workspace.root());
}
