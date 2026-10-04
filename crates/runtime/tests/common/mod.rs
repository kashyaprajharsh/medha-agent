//! What the many-chats tests share: a model that echoes, and chats built as a backend would.

#![allow(dead_code)]

use kernel::{Message, Role};
use runtime::session::{Start, Started};
use runtime::{Notices, SessionOptions, Surface, Workspace};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Quiet;

impl Notices for Quiet {
    fn say(&self, _: &str) {}
}

/// Replies `echo: <last user message>` so a chat that got another chat's turn is caught.
pub async fn endpoint() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
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

pub fn open(folder: &Path) -> Workspace {
    std::fs::create_dir_all(folder).unwrap();
    let workspace = Workspace::open(folder.to_path_buf(), &Quiet).unwrap();
    std::fs::create_dir_all(workspace.state.join("logs")).unwrap();
    workspace
}

pub async fn chat(workspace: &Workspace, endpoint: &str, model: &str) -> Started {
    chat_with(workspace, endpoint, model, Arc::new(kernel::AutoDeny)).await
}

pub async fn chat_with(
    workspace: &Workspace,
    endpoint: &str,
    model: &str,
    gate: Arc<dyn kernel::HumanGate>,
) -> Started {
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
            gate,
            asker: Arc::new(kernel::NoAsker),
            agents: None,
        },
    )
    .await
    .unwrap()
}

pub async fn say(chat: &Started, text: &str) -> String {
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
