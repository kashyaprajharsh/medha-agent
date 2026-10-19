use providers::{AuthKind, ProviderProfile};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn explicit_window_wins_and_server_metadata_is_used_for_an_unknown_model() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut profile = ProviderProfile::openai_chat(
        format!("http://{}/v1", listener.local_addr().unwrap()),
        "private-context-fixture",
        AuthKind::None,
    );
    profile.max_ctx = Some(8192);
    assert_eq!(
        runtime::model::context_window(&profile, "").await,
        (Some(8192), "config/env")
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let count = stream.read(&mut request).await.unwrap();
        assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /v1/models "));
        let body = r#"{"data":[{"id":"another-model","context_length":999999},{"id":"private-context-fixture","max_model_len":16384}]}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    profile.max_ctx = None;
    assert_eq!(
        runtime::model::context_window(&profile, "").await,
        (Some(16384), "discovered from /v1/models")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn unknown_deployments_do_not_receive_an_invented_default() {
    let profile = ProviderProfile::openai_chat("", "private-context-fixture", AuthKind::None);
    assert_eq!(runtime::model::context_window(&profile, "").await.0, None);
}
