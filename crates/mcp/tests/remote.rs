//! Remote (Streamable HTTP) transport coverage against a hermetic local MCP
//! server: no-auth and bearer connects, header enforcement, plaintext refusal,
//! and the needs-sign-in state an OAuth server without credentials lands in.

use std::{sync::Arc, time::Duration};

use mcp::{
    Config, Error, McpManager, RemoteAuth, ServerConfig, ServerState, TokenStore, Transport,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// A minimal Streamable HTTP MCP endpoint: one JSON-RPC POST in, one JSON
/// response out. Enough to exercise the transport, not to reimplement a server.
async fn spawn_server(required_bearer: Option<&'static str>) -> String {
    spawn_with_challenge(required_bearer, None).await
}

/// `challenge` is the `WWW-Authenticate` value returned with a 401 when the
/// request carries no credentials — the signal Medha probes for.
async fn spawn_with_challenge(
    required_bearer: Option<&'static str>,
    challenge: Option<&'static str>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let Some(request) = read_http_request(&mut stream).await else {
                    return;
                };
                let authorized = match required_bearer {
                    Some(token) => request.lines().any(|line| {
                        line.to_ascii_lowercase().starts_with("authorization:")
                            && line.contains(token)
                    }),
                    None => true,
                };
                if !authorized {
                    let header = challenge
                        .map(|value| format!("WWW-Authenticate: {value}\r\n"))
                        .unwrap_or_default();
                    let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 401 Unauthorized\r\n{header}Content-Length: 0\r\n\r\n"
                            )
                            .as_bytes(),
                        )
                        .await;
                    return;
                }
                let body = request.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
                let response = reply(body);
                let payload = serde_json::to_string(&response).unwrap_or_default();
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\n\r\n{payload}",
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

/// Read one complete HTTP request: headers plus any `Content-Length` body. A
/// single `read` can return a partial request when TCP splits the segment under
/// load, dropping the auth header or JSON body — the source of the intermittent
/// handshake failures this mock otherwise produced.
async fn read_http_request(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return Some(String::from_utf8_lossy(&buf).into_owned());
        }
        buf.extend_from_slice(&chunk[..read]);
        let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let content_length = String::from_utf8_lossy(&buf[..head_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if buf.len() >= head_end + 4 + content_length {
            return Some(String::from_utf8_lossy(&buf).into_owned());
        }
    }
}

fn reply(body: &str) -> Value {
    let message: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let result = match message.get("method").and_then(Value::as_str) {
        Some("initialize") => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "hosted", "version": "0.0.1" }
        }),
        Some("tools/list") => json!({ "tools": [{
            "name": "ping",
            "description": "Ping the hosted server",
            "inputSchema": { "type": "object" }
        }]}),
        Some("tools/call") => json!({
            "content": [{ "type": "text", "text": "pong" }],
            "isError": false
        }),
        _ => json!({}),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

#[derive(Debug, Default)]
struct MemoryTokens;

impl TokenStore for MemoryTokens {
    fn load(&self, _server: &str, _url: &str) -> Option<String> {
        None
    }
    fn save(&self, _server: &str, _url: &str, _blob: &str) {}
    fn clear(&self, _server: &str, _url: &str) {}
}

fn remote(id: &str, url: String, auth: RemoteAuth) -> Config {
    Config {
        enabled: true,
        servers: vec![ServerConfig {
            id: id.into(),
            transport: Transport::Remote { url, auth },
            ..Default::default()
        }],
        startup_timeout: Duration::from_secs(10),
        request_timeout: Duration::from_secs(10),
        health_interval: Duration::from_millis(200),
        tokens: Some(Arc::new(MemoryTokens)),
        ..Config::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connects_to_a_hosted_server_over_http() {
    let url = spawn_server(None).await;
    let manager = McpManager::new(
        std::env::temp_dir(),
        remote("hosted", url, RemoteAuth::None),
    );
    manager.connect_startup().await;

    let status = &manager.status().await[0];
    assert_eq!(
        status.state,
        ServerState::Ready,
        "detail: {:?}",
        status.detail
    );
    assert_eq!(status.tools, 1);

    let out = manager.call("mcp__hosted__ping", &json!({})).await.unwrap();
    assert_eq!(out.text, "pong");
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bearer_token_is_sent_and_required() {
    let url = spawn_server(Some("s3cret")).await;

    let good = McpManager::new(
        std::env::temp_dir(),
        remote("hosted", url.clone(), RemoteAuth::Bearer("s3cret".into())),
    );
    good.connect_startup().await;
    assert_eq!(good.status().await[0].state, ServerState::Ready);
    good.shutdown().await;

    // Wrong token: the server rejects, so the host must not report ready.
    let bad = McpManager::new(
        std::env::temp_dir(),
        remote("hosted", url, RemoteAuth::Bearer("wrong".into())),
    );
    bad.connect_startup().await;
    assert_ne!(bad.status().await[0].state, ServerState::Ready);
    bad.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_without_credentials_asks_for_sign_in() {
    let url = spawn_server(None).await;
    let manager = McpManager::new(
        std::env::temp_dir(),
        remote("hosted", url, RemoteAuth::OAuth),
    );
    manager.connect_startup().await;

    assert_eq!(manager.status().await[0].state, ServerState::NeedsAuth);
    assert!(manager.needs_sign_in("hosted").await);
    // A model-invoked start must not open a browser — it reports the state instead.
    assert!(matches!(
        manager.approve_and_connect("hosted", None).await,
        Err(Error::NeedsAuth(_))
    ));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plaintext_remote_servers_are_refused() {
    let manager = McpManager::new(
        std::env::temp_dir(),
        remote(
            "hosted",
            "http://mcp.example.com/mcp".into(),
            RemoteAuth::None,
        ),
    );
    manager.connect_startup().await;

    let status = &manager.status().await[0];
    // A bad URL is a configuration fault: terminal, never retried.
    assert_eq!(status.state, ServerState::Failed);
    assert!(status.detail.as_deref().unwrap_or("").contains("https"));
    assert!(mcp::validate_remote_url("http://mcp.example.com/mcp").is_err());
    assert!(mcp::validate_remote_url("https://mcp.linear.app/mcp").is_ok());
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_server_needs_no_configuration() {
    let url = spawn_server(None).await;
    let manager = McpManager::new(
        std::env::temp_dir(),
        remote("hosted", url, RemoteAuth::Auto),
    );
    manager.connect_startup().await;

    // Auto is the default: pasting a URL is enough for an unauthenticated server.
    let status = &manager.status().await[0];
    assert_eq!(
        status.state,
        ServerState::Ready,
        "detail: {:?}",
        status.detail
    );
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oauth_challenge_routes_to_sign_in() {
    // A 401 advertising protected-resource metadata means discovery works, so
    // the browser flow can run without asking the user anything.
    let url = spawn_with_challenge(
        Some("never-sent"),
        Some(r#"Bearer resource_metadata="https://example.test/.well-known/oauth-protected-resource""#),
    )
    .await;
    let manager = McpManager::new(
        std::env::temp_dir(),
        remote("hosted", url, RemoteAuth::Auto),
    );
    manager.connect_startup().await;

    assert_eq!(manager.status().await[0].state, ServerState::NeedsAuth);
    assert!(manager.needs_sign_in("hosted").await);
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_challenge_asks_for_a_token() {
    // 401 with no discovery metadata: Medha cannot obtain this on its own, so
    // it must ask rather than guess.
    let url = spawn_with_challenge(Some("never-sent"), Some("Bearer")).await;
    let manager = McpManager::new(
        std::env::temp_dir(),
        remote("hosted", url, RemoteAuth::Auto),
    );
    manager.connect_startup().await;

    assert_eq!(manager.status().await[0].state, ServerState::NeedsToken);
    assert!(!manager.needs_sign_in("hosted").await);
    manager.shutdown().await;
}

#[test]
fn a_pasted_url_is_enough_to_name_a_server() {
    assert_eq!(
        mcp::id_from_url("https://mcp.linear.app/mcp").as_deref(),
        Some("linear")
    );
    assert_eq!(
        mcp::id_from_url("https://mcp.alphavantage.co/mcp").as_deref(),
        Some("alphavantage")
    );
    assert_eq!(
        mcp::id_from_url("https://api.github.com/mcp").as_deref(),
        Some("github")
    );
    assert!(mcp::is_url("https://x.dev"));
    assert!(!mcp::is_url("npx"));
    assert_eq!(mcp::id_from_url("not a url"), None);
}
