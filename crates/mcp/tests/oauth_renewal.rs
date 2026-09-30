//! A signed-in remote server must stay usable past its access token's lifetime:
//! renewed before use when the stored token is old, renewed on refusal when the
//! server rejects it, persisted so a restart does not replay a spent grant, and
//! reported as needing sign-in only when the provider actually refuses.

mod common;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use common::read_http_request;
use mcp::{
    Config, Error, McpManager, RemoteAuth, ServerConfig, ServerState, TokenStore, Transport,
};
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, net::TcpListener};

#[derive(Clone, Copy, PartialEq)]
enum TokenEndpoint {
    Grants,
    Refuses,
    Down,
}

/// One authorization server plus the resource it guards. Refreshes rotate both
/// tokens, as providers that follow OAuth 2.1 do.
struct Provider {
    access: Option<String>,
    refresh: String,
    endpoint: TokenEndpoint,
    refreshes: u32,
    /// Bearers the resource turned away, in order.
    refused: Vec<String>,
}

type Shared = Arc<Mutex<Provider>>;

async fn spawn_provider(access: Option<&str>, refresh: &str) -> (String, Shared) {
    let provider = Arc::new(Mutex::new(Provider {
        access: access.map(str::to_string),
        refresh: refresh.to_string(),
        endpoint: TokenEndpoint::Grants,
        refreshes: 0,
        refused: Vec::new(),
    }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = Arc::clone(&provider);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let provider = Arc::clone(&shared);
            tokio::spawn(async move {
                let Some(request) = read_http_request(&mut stream).await else {
                    return;
                };
                let (status, headers, body) = route(&provider, &request);
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\n{headers}Content-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), provider)
}

fn route(provider: &Shared, request: &str) -> (&'static str, String, String) {
    let line = request.lines().next().unwrap_or_default();
    let body = request.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    let mut state = provider.lock().unwrap();
    if line.starts_with("POST /token") {
        return match state.endpoint {
            TokenEndpoint::Down => ("503 Service Unavailable", String::new(), String::new()),
            TokenEndpoint::Refuses => refuse(),
            TokenEndpoint::Grants
                if !body.contains(&format!("refresh_token={}", state.refresh)) =>
            {
                refuse()
            }
            TokenEndpoint::Grants => {
                state.refreshes += 1;
                let n = state.refreshes;
                state.access = Some(format!("access-{n}"));
                state.refresh = format!("refresh-{n}");
                let grant = json!({
                    "access_token": format!("access-{n}"),
                    "token_type": "bearer",
                    "expires_in": 3600,
                    "refresh_token": format!("refresh-{n}"),
                });
                ("200 OK", String::new(), grant.to_string())
            }
        };
    }
    if !line.starts_with("POST /mcp") {
        // No discovery documents: rmcp falls back to `/token` on this origin.
        return ("404 Not Found", String::new(), String::new());
    }
    let presented = request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().trim_start_matches("Bearer ").to_string())
    });
    if presented.is_none() || presented != state.access {
        state.refused.extend(presented);
        return (
            "401 Unauthorized",
            "WWW-Authenticate: Bearer error=\"invalid_token\"\r\n".into(),
            String::new(),
        );
    }
    ("200 OK", String::new(), reply(body).to_string())
}

fn refuse() -> (&'static str, String, String) {
    let error = json!({ "error": "invalid_grant" });
    ("400 Bad Request", String::new(), error.to_string())
}

fn reply(body: &str) -> Value {
    let message: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let Some(id) = message.get("id").cloned() else {
        return json!({});
    };
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

/// Stands in for the keychain both Medha front ends share.
#[derive(Debug, Default)]
struct Keychain(Mutex<Option<String>>);

impl TokenStore for Keychain {
    fn load(&self, _server: &str, _url: &str) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
    fn save(&self, _server: &str, _url: &str, blob: &str) {
        *self.0.lock().unwrap() = Some(blob.to_string());
    }
    fn clear(&self, _server: &str, _url: &str) {
        *self.0.lock().unwrap() = None;
    }
}

impl Keychain {
    fn holding(access: &str, refresh: &str, received_at: Option<u64>) -> Arc<Self> {
        let mut blob = json!({
            "client_id": "medha-test",
            "token": {
                "access_token": access,
                "token_type": "bearer",
                "expires_in": 3600,
                "refresh_token": refresh,
            },
        });
        if let Some(at) = received_at {
            blob["received_at"] = json!(at);
        }
        Arc::new(Self(Mutex::new(Some(blob.to_string()))))
    }

    fn stored(&self) -> Value {
        serde_json::from_str(self.0.lock().unwrap().as_deref().unwrap()).unwrap()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn manager(url: &str, keychain: &Arc<Keychain>) -> McpManager {
    McpManager::new(
        std::env::temp_dir(),
        Config {
            enabled: true,
            servers: vec![ServerConfig {
                id: "hosted".into(),
                transport: Transport::Remote {
                    url: url.into(),
                    auth: RemoteAuth::OAuth,
                },
                ..Default::default()
            }],
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(10),
            // Keep the supervisor's probe out of the way of each scenario.
            health_interval: Duration::from_secs(3600),
            tokens: Some(Arc::clone(keychain) as Arc<dyn TokenStore>),
            ..Config::default()
        },
    )
}

async fn state(manager: &McpManager) -> ServerState {
    manager.status().await[0].state
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_token_is_renewed_before_use_and_survives_a_restart() {
    let (url, provider) = spawn_provider(None, "refresh-0").await;
    let keychain = Keychain::holding("access-0", "refresh-0", Some(now() - 7200));

    let first = manager(&url, &keychain);
    first.connect_startup().await;
    assert_eq!(state(&first).await, ServerState::Ready);
    assert_eq!(
        first
            .call("mcp__hosted__ping", &json!({}))
            .await
            .unwrap()
            .text,
        "pong"
    );
    first.shutdown().await;

    // Its age was known, so it was renewed up front: the server never saw it.
    assert_eq!(provider.lock().unwrap().refused, Vec::<String>::new());
    let stored = keychain.stored();
    assert_eq!(stored["token"]["refresh_token"], "refresh-1");
    assert!(stored["received_at"].as_u64().unwrap() >= now() - 60);

    // A restart — or the other front end — resumes from the rotated grant.
    // Replaying the spent `refresh-0` would be refused and force a sign-in.
    let second = manager(&url, &keychain);
    second.connect_startup().await;
    assert_eq!(
        second
            .call("mcp__hosted__ping", &json!({}))
            .await
            .unwrap()
            .text,
        "pong"
    );
    assert_eq!(provider.lock().unwrap().refreshes, 1);
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_refused_mid_session_is_renewed_and_the_call_succeeds() {
    // Written before issue times were stored, so its expiry is unknown.
    let (url, provider) = spawn_provider(Some("access-0"), "refresh-0").await;
    let keychain = Keychain::holding("access-0", "refresh-0", None);
    let manager = manager(&url, &keychain);
    manager.connect_startup().await;
    assert_eq!(state(&manager).await, ServerState::Ready);

    provider.lock().unwrap().access = None;
    let out = manager.call("mcp__hosted__ping", &json!({})).await.unwrap();

    assert_eq!(out.text, "pong");
    assert_eq!(state(&manager).await, ServerState::Ready);
    assert_eq!(keychain.stored()["token"]["access_token"], "access-1");
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_grant_asks_for_sign_in_instead_of_staying_connected() {
    let (url, provider) = spawn_provider(Some("access-0"), "refresh-0").await;
    let keychain = Keychain::holding("access-0", "refresh-0", None);
    let manager = manager(&url, &keychain);
    manager.connect_startup().await;
    assert_eq!(state(&manager).await, ServerState::Ready);

    {
        let mut provider = provider.lock().unwrap();
        provider.access = None;
        provider.endpoint = TokenEndpoint::Refuses;
    }
    let outcome = manager.call("mcp__hosted__ping", &json!({})).await;

    assert!(matches!(outcome, Err(Error::NeedsAuth(_))), "{outcome:?}");
    assert_eq!(state(&manager).await, ServerState::NeedsAuth);
    assert!(manager.needs_sign_in("hosted").await);
    assert!(
        manager.tool_specs().is_empty(),
        "dead tools must leave the model's context"
    );
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_token_endpoint_is_not_mistaken_for_a_lost_sign_in() {
    let (url, provider) = spawn_provider(Some("access-0"), "refresh-0").await;
    let keychain = Keychain::holding("access-0", "refresh-0", None);
    let manager = manager(&url, &keychain);
    manager.connect_startup().await;

    {
        let mut provider = provider.lock().unwrap();
        provider.access = None;
        provider.endpoint = TokenEndpoint::Down;
    }
    let outcome = manager.call("mcp__hosted__ping", &json!({})).await;
    assert!(matches!(outcome, Err(Error::Protocol(_))), "{outcome:?}");
    assert_eq!(state(&manager).await, ServerState::Ready);

    // Once the provider is back, the same grant still works.
    provider.lock().unwrap().endpoint = TokenEndpoint::Grants;
    let out = manager.call("mcp__hosted__ping", &json!({})).await.unwrap();
    assert_eq!(out.text, "pong");
    manager.shutdown().await;
}
