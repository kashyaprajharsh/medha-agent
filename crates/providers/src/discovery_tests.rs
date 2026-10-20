use super::*;
use crate::AuthKind;
use crate::test_http::{Response, Server};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn discovery_rejects_redirects_without_forwarding_any_authentication() {
    let destination = Server::scripted(vec![Response::json(r#"{"data":[]}"#)]).await;
    for auth in [AuthKind::Bearer, AuthKind::XApiKey, AuthKind::XGoogApiKey] {
        let source = Server::scripted(vec![Response {
            status: "302 Found",
            headers: format!("Location: {}\r\n", destination.url),
            ..Response::json("")
        }])
        .await;
        let mut profile = ProviderProfile::openai_chat(&source.url, "fixture", auth);
        profile
            .headers
            .insert("x-account-secret".into(), "dummy-custom-secret".into());
        assert!(
            list_models_for_profile(&profile, "dummy-redirect-key")
                .await
                .is_err()
        );
        assert!(destination.requests.lock().unwrap().is_empty());
        let source_requests = source.requests.lock().unwrap();
        assert!(source_requests[0].0.contains("dummy-redirect-key"));
        assert!(source_requests[0].0.contains("dummy-custom-secret"));
    }
}

#[tokio::test]
async fn discovery_rejects_oversized_or_invalid_lists_and_preserves_manual_setup() {
    let fixtures = [
        "x".repeat(DISCOVERY_MAX_BYTES + 1),
        json!({"data":(0..=DISCOVERY_MAX_MODELS).map(|n| json!({"id":format!("m-{n}")})).collect::<Vec<_>>()} ).to_string(),
        json!({"data":[{"id":"unsafe\u{1b}[2J"}]}).to_string(),
        json!({"data":[{"id":"x".repeat(513)}]}).to_string(),
    ];
    for fixture in fixtures {
        let server = Server::scripted(vec![Response::json(fixture)]).await;
        let profile = ProviderProfile::openai_chat(&server.url, "fixture", AuthKind::None);
        assert!(list_models_for_profile(&profile, "").await.is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
    let server = Server::scripted(vec![Response::json(
        r#"{"data":[{"id":"valid","context_length":0},{"id":"valid"}]}"#,
    )])
    .await;
    let profile = ProviderProfile::openai_chat(&server.url, "fixture", AuthKind::None);
    let models = list_models_for_profile(&profile, "").await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].context_length, None);
}

#[tokio::test]
async fn gemini_discovery_is_bounded_even_when_every_page_token_is_different() {
    let pages = (0..DISCOVERY_MAX_PAGES)
        .map(|n| {
            Response::json(json!({"models":[],"nextPageToken":format!("token-{n}")}).to_string())
        })
        .collect();
    let server = Server::scripted(pages).await;
    let mut profile = ProviderProfile::openai_chat(&server.url, "fixture", AuthKind::XGoogApiKey);
    profile.protocol = Protocol::GeminiInteractions;
    let error = list_models_for_profile(&profile, "dummy-google-key")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("16 pages"));
    assert_eq!(server.requests.lock().unwrap().len(), DISCOVERY_MAX_PAGES);
}

#[tokio::test]
async fn native_model_discovery_uses_v1_google_auth_and_normalizes_ids() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "client closed before headers");
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let headers = String::from_utf8_lossy(&bytes);
        assert!(
            headers.starts_with("GET /v1/models?pageSize=1000 HTTP/1.1"),
            "{headers}"
        );
        assert!(
            headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case("x-goog-api-key: discovery-secret")),
            "{headers}"
        );
        assert!(
                headers
                    .lines()
                    .any(|line| line
                        .eq_ignore_ascii_case(&format!("user-agent: {}", http::USER_AGENT))),
                "{headers}"
            );
        let body = serde_json::json!({
            "models": [
                {
                    "name": "models/gemini-3.5-flash",
                    "baseModelId": "gemini-3.5-flash",
                    "inputTokenLimit": 1_048_576,
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/text-embedding-004",
                    "baseModelId": "text-embedding-004",
                    "inputTokenLimit": 2_048,
                    "supportedGenerationMethods": ["embedContent"]
                }
            ]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let mut profile = ProviderProfile::openai_chat(
        format!("http://{address}/v1"),
        "model-discovery",
        AuthKind::XGoogApiKey,
    );
    profile.protocol = Protocol::GeminiInteractions;
    let models = list_models_for_profile(&profile, "discovery-secret")
        .await
        .unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "gemini-3.5-flash");
    assert_eq!(models[0].context_length, Some(1_048_576));
    server.await.unwrap();
}

#[tokio::test]
async fn openai_chat_discovery_handles_groq_dual_context_keys() {
    // Groq reports BOTH `context_length` and `context_window` on each model.
    // Aliasing them to one field made serde raise "duplicate field" and the
    // whole discovery failed with an opaque decode error. It must now parse,
    // preferring `context_length`.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "client closed before headers");
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        assert!(String::from_utf8_lossy(&bytes).lines().any(|line| {
            line.eq_ignore_ascii_case(&format!("user-agent: {}", http::USER_AGENT))
        }));
        let body = serde_json::json!({
            "object": "list",
            "data": [
                {
                    "id": "openai/gpt-oss-120b",
                    "object": "model",
                    "owned_by": "OpenAI",
                    "active": true,
                    "context_window": 131_072,
                    "context_length": 131_072,
                    "max_completion_tokens": 8_192
                }
            ]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let profile = ProviderProfile::openai_chat(
        format!("http://{address}/openai/v1"),
        "model-discovery",
        AuthKind::Bearer,
    );
    let models = list_models_for_profile(&profile, "gsk_test").await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "openai/gpt-oss-120b");
    assert_eq!(models[0].context_length, Some(131_072));
    server.await.unwrap();
}
