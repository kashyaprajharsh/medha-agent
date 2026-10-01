#![cfg(unix)]

mod common;

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use common::read_http_request;
use mcp::{
    Config, McpManager, RemoteAuth, ServerConfig, ServerState, TokenStore, Transport,
    hub::{Endpoint, Resolve},
};
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, net::TcpListener};

/// A hosted MCP server that counts how many connections were opened to it.
async fn counting_server(handshakes: Arc<AtomicUsize>) -> String {
    hosted_server(handshakes, None, Duration::ZERO).await
}

/// `key` is the bearer token it insists on; `delay` slows every tool call.
async fn hosted_server(
    handshakes: Arc<AtomicUsize>,
    key: Option<&'static str>,
    delay: Duration,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let handshakes = Arc::clone(&handshakes);
            tokio::spawn(async move {
                let Some(request) = read_http_request(&mut stream).await else {
                    return;
                };
                if let Some(key) = key
                    && !request.lines().any(|line| {
                        line.eq_ignore_ascii_case(&format!("authorization: Bearer {key}"))
                    })
                {
                    let _ = stream
                        .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
                let body = request.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                let message: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                let result = match message["method"].as_str() {
                    Some("initialize") => {
                        handshakes.fetch_add(1, Ordering::SeqCst);
                        json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                               "serverInfo": {"name": "hosted", "version": "0"}})
                    }
                    Some("tools/list") => json!({"tools": [{"name": "ping",
                        "description": "Ping the hosted server", "inputSchema": {"type": "object"}}]}),
                    Some("tools/call") => {
                        tokio::time::sleep(delay).await;
                        json!({"content": [{"type": "text", "text": "pong"}], "isError": false})
                    }
                    _ => json!({}),
                };
                let payload =
                    json!({"jsonrpc": "2.0", "id": message["id"], "result": result}).to_string();
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
                            payload.len()
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://127.0.0.1:{port}/mcp")
}

#[derive(Debug, Default)]
struct NoTokens;

impl TokenStore for NoTokens {
    fn load(&self, _server: &str, _url: &str) -> Option<String> {
        None
    }
    fn save(&self, _server: &str, _url: &str, _blob: &str) {}
    fn clear(&self, _server: &str, _url: &str) {}
}

fn hosted(url: &str) -> ServerConfig {
    ServerConfig {
        id: "hosted".into(),
        transport: Transport::Remote {
            url: url.into(),
            auth: RemoteAuth::None,
        },
        ..Default::default()
    }
}

fn config(servers: Vec<ServerConfig>, cache: Option<PathBuf>) -> Config {
    Config {
        enabled: true,
        servers,
        startup_timeout: Duration::from_secs(10),
        request_timeout: Duration::from_secs(10),
        health_interval: Duration::from_millis(200),
        tokens: Some(Arc::new(NoTokens)),
        cache,
        ..Config::default()
    }
}

type UserConfig = Arc<std::sync::Mutex<Vec<ServerConfig>>>;

/// Starts a host serving `url` as the only server in "the user's config".
async fn host(url: &str, dir: &std::path::Path) -> (McpManager, Endpoint) {
    let (manager, endpoint, _) = host_with(vec![hosted(url)], dir).await;
    (manager, endpoint)
}

async fn host_with(
    servers: Vec<ServerConfig>,
    dir: &std::path::Path,
) -> (McpManager, Endpoint, UserConfig) {
    let manager = McpManager::new(dir.to_path_buf(), config(servers.clone(), None));
    manager.connect_startup().await;
    let endpoint = Endpoint {
        address: dir.join("host.sock").display().to_string(),
        token: "a1b2c3d4".into(),
    };
    let user: UserConfig = Arc::new(std::sync::Mutex::new(servers));
    let file = Arc::clone(&user);
    let resolve: Resolve = Arc::new(move |id: &str| {
        let servers = file.lock().unwrap();
        servers.iter().find(|server| server.id == id).cloned()
    });
    let (serving, address, token) = (
        manager.clone(),
        endpoint.address.clone(),
        endpoint.token.clone(),
    );
    tokio::spawn(async move { mcp::hub::run_host(serving, &address, token.into(), resolve).await });
    for _ in 0..50 {
        if std::path::Path::new(&endpoint.address).exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (manager, endpoint, user)
}

async fn chat(dir: &std::path::Path, endpoint: &Endpoint) -> McpManager {
    let manager = McpManager::new(dir.to_path_buf(), config(Vec::new(), None));
    manager.attach_hub(endpoint.clone()).await.unwrap();
    manager
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_chats_share_one_connection_to_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let handshakes = Arc::new(AtomicUsize::new(0));
    let url = counting_server(Arc::clone(&handshakes)).await;
    let (_host, endpoint) = host(&url, dir.path()).await;

    let first = chat(dir.path(), &endpoint).await;
    let second = chat(dir.path(), &endpoint).await;
    for chat in [&first, &second] {
        assert_eq!(chat.status().await[0].state, ServerState::Ready);
        assert!(
            chat.tool_specs()
                .iter()
                .any(|spec| spec.name == "mcp__hosted__ping")
        );
        let out = chat.call("mcp__hosted__ping", &json!({})).await.unwrap();
        assert_eq!(out.text, "pong");
    }
    // Asking the host for a server it already runs must not reconnect it.
    let status = second.add_server(hosted(&url)).await.unwrap();
    assert_eq!(status.state, ServerState::Ready);
    assert_eq!(handshakes.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_token_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let url = counting_server(Arc::new(AtomicUsize::new(0))).await;
    let (_host, mut endpoint) = host(&url, dir.path()).await;
    endpoint.token = "a1b2c3d5".into();
    let chat = McpManager::new(dir.path().to_path_buf(), config(Vec::new(), None));
    assert!(chat.attach_hub(endpoint).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_can_name_a_server_but_never_define_one_for_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let url = counting_server(Arc::new(AtomicUsize::new(0))).await;
    let (host, endpoint) = host(&url, dir.path()).await;
    let chat = chat(dir.path(), &endpoint).await;

    let foreign_url = counting_server(Arc::new(AtomicUsize::new(0))).await;
    let foreign = ServerConfig {
        id: "foreign".into(),
        ..hosted(&foreign_url)
    };
    chat.add_server(foreign).await.unwrap();
    assert!(
        host.status()
            .await
            .iter()
            .all(|status| status.server != "foreign")
    );
    assert!(
        chat.status()
            .await
            .iter()
            .any(|status| status.server == "foreign")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_change_reaches_every_attached_chat() {
    let dir = tempfile::tempdir().unwrap();
    let url = counting_server(Arc::new(AtomicUsize::new(0))).await;
    let (host, endpoint) = host(&url, dir.path()).await;
    let chat = chat(dir.path(), &endpoint).await;
    let mut changes = chat.subscribe();
    changes.borrow_and_update();

    host.set_disabled("hosted", true).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            changes.changed().await.unwrap();
            if chat.status().await[0].state == ServerState::Disabled {
                return;
            }
        }
    })
    .await
    .expect("the chat heard about the change without asking");
    assert!(chat.tool_specs().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn known_tools_are_listed_before_the_server_answers() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let url = counting_server(Arc::new(AtomicUsize::new(0))).await;
    let first = McpManager::new(
        dir.path().into(),
        config(vec![hosted(&url)], Some(cache.clone())),
    );
    first.connect_startup().await;
    first.shutdown().await;

    let next = McpManager::new(
        dir.path().into(),
        config(vec![hosted(&url)], Some(cache.clone())),
    );
    assert!(
        next.tool_specs()
            .iter()
            .any(|spec| spec.name == "mcp__hosted__ping")
    );

    let moved = hosted("https://elsewhere.example/mcp");
    let changed = McpManager::new(dir.path().into(), config(vec![moved], Some(cache)));
    assert!(
        changed.tool_specs().is_empty(),
        "a changed server starts without stale tools"
    );
}

fn keyed(url: &str, key: &str) -> ServerConfig {
    ServerConfig {
        id: "keyed".into(),
        transport: Transport::Remote {
            url: url.into(),
            auth: RemoteAuth::Bearer(key.into()),
        },
        ..Default::default()
    }
}

async fn reaches(manager: &McpManager, server: &str, want: ServerState) -> Option<ServerState> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state = manager
            .status()
            .await
            .into_iter()
            .find(|status| status.server == server)
            .map(|status| status.state);
        if state == Some(want) || tokio::time::Instant::now() > deadline {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_saved_while_the_host_runs_is_the_one_it_connects_with() {
    let dir = tempfile::tempdir().unwrap();
    let url = hosted_server(Arc::new(AtomicUsize::new(0)), Some("fresh"), Duration::ZERO).await;
    let (host, endpoint, user) = host_with(vec![keyed(&url, "")], dir.path()).await;
    assert_eq!(
        reaches(&host, "keyed", ServerState::NeedsToken).await,
        Some(ServerState::NeedsToken)
    );
    let chat = chat(dir.path(), &endpoint).await;

    *user.lock().unwrap() = vec![keyed(&url, "fresh")];
    let status = chat.add_server(keyed(&url, "fresh")).await.unwrap();
    assert_eq!(status.state, ServerState::Ready);
    assert_eq!(
        chat.call("mcp__keyed__ping", &json!({}))
            .await
            .unwrap()
            .text,
        "pong"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_added_after_the_host_started_reaches_new_chats_without_being_approved() {
    let dir = tempfile::tempdir().unwrap();
    let handshakes = Arc::new(AtomicUsize::new(0));
    let url = counting_server(Arc::clone(&handshakes)).await;
    let (host, endpoint, user) = host_with(Vec::new(), dir.path()).await;
    let later = ServerConfig {
        id: "later".into(),
        ..hosted(&url)
    };
    let off = ServerConfig {
        id: "off".into(),
        disabled: true,
        ..hosted(&url)
    };
    let gated = ServerConfig {
        id: "gated".into(),
        requires_approval: true,
        ..hosted(&url)
    };
    user.lock()
        .unwrap()
        .extend([later.clone(), off.clone(), gated.clone()]);

    let chat = chat(dir.path(), &endpoint).await;
    chat.share(vec![later, off, gated]).await;
    assert_eq!(
        reaches(&chat, "later", ServerState::Ready).await,
        Some(ServerState::Ready)
    );
    assert!(
        chat.tool_specs()
            .iter()
            .any(|spec| spec.name == "mcp__later__ping")
    );
    assert_eq!(
        reaches(&host, "off", ServerState::Disabled).await,
        Some(ServerState::Disabled)
    );
    assert_eq!(
        reaches(&host, "gated", ServerState::NeedsApproval).await,
        Some(ServerState::NeedsApproval)
    );
    assert_eq!(
        handshakes.load(Ordering::SeqCst),
        1,
        "starting a chat never turns a server on or approves one"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_the_host_declines_stays_gated_in_the_chat() {
    let dir = tempfile::tempdir().unwrap();
    let handshakes = Arc::new(AtomicUsize::new(0));
    let url = counting_server(Arc::clone(&handshakes)).await;
    let (_host, endpoint, _) = host_with(Vec::new(), dir.path()).await;
    let gated = ServerConfig {
        requires_approval: true,
        ..hosted(&url)
    };

    let chat = chat(dir.path(), &endpoint).await;
    chat.share(vec![gated]).await;
    assert_eq!(
        reaches(&chat, "hosted", ServerState::NeedsApproval).await,
        Some(ServerState::NeedsApproval)
    );
    assert_eq!(handshakes.load(Ordering::SeqCst), 0, "connected unapproved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_a_server_off_in_one_chat_lets_another_chats_call_finish() {
    let dir = tempfile::tempdir().unwrap();
    let url = hosted_server(
        Arc::new(AtomicUsize::new(0)),
        None,
        Duration::from_millis(800),
    )
    .await;
    let (_host, endpoint) = host(&url, dir.path()).await;
    let first = chat(dir.path(), &endpoint).await;
    let second = chat(dir.path(), &endpoint).await;

    let call = tokio::spawn({
        let second = second.clone();
        async move { second.call("mcp__hosted__ping", &json!({})).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    first.set_disabled("hosted", true).await.unwrap();
    assert_eq!(call.await.unwrap().unwrap().text, "pong");
    assert_eq!(
        reaches(&second, "hosted", ServerState::Disabled).await,
        Some(ServerState::Disabled)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_impostor_at_the_address_never_learns_the_token() {
    use tokio::io::AsyncReadExt;
    let dir = tempfile::tempdir().unwrap();
    let address = dir.path().join("host.sock");
    let listener = tokio::net::UnixListener::bind(&address).unwrap();
    let heard = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut seen = Vec::new();
        let mut buffer = [0u8; 4096];
        let read = stream.read(&mut buffer).await.unwrap();
        seen.extend_from_slice(&buffer[..read]);
        let made_up =
            json!({"id": 0, "result": {"proof": "00".repeat(32), "nonce": "11".repeat(32)}});
        let _ = stream.write_all(format!("{made_up}\n").as_bytes()).await;
        while let Ok(Ok(read)) =
            tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buffer)).await
        {
            if read == 0 {
                break;
            }
            seen.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8_lossy(&seen).into_owned()
    });
    let token = "5ecre7-t0ken-a1b2c3";
    let chat = McpManager::new(dir.path().to_path_buf(), config(Vec::new(), None));
    let endpoint = Endpoint {
        address: address.display().to_string(),
        token: token.into(),
    };
    assert!(chat.attach_hub(endpoint).await.is_err());
    let heard = heard.await.unwrap();
    assert!(heard.contains("hello"), "{heard}");
    assert!(
        !heard.contains(token),
        "the token crossed the channel: {heard}"
    );
}
