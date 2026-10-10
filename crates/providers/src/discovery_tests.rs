use super::*;
use crate::test_http::{Response, Server};
use serde_json::json;

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
