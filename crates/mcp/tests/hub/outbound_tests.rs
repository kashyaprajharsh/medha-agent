use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exposed_chat_checks_shared_remote_arguments_before_forwarding() {
    let dir = tempfile::tempdir().unwrap();
    let url = counting_server(Arc::new(AtomicUsize::new(0))).await;
    let (_host, endpoint) = host(&url, dir.path()).await;
    let chat = chat(dir.path(), &endpoint).await;
    let guard = Arc::new(kernel::egress::Guard::default());
    let secret = "shared-mcp-outbound-credential-835216794";
    kernel::redact::remember(secret);
    let args = serde_json::json!({"secret":secret});
    assert!(chat.remote_call("mcp__hosted__ping").await.unwrap());
    let (result, notice) =
        kernel::egress::scope(guard, true, chat.call("mcp__hosted__ping", &args)).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Outbound payload blocked")
    );
    let review = notice.unwrap();
    assert!(
        review["destination"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
    assert!(!review.to_string().contains(secret));
}
