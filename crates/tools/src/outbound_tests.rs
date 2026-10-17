use super::*;

#[tokio::test]
async fn web_checks_block_before_network_and_approval_is_for_one_exact_send() {
    let guard = Arc::new(kernel::egress::Guard::default());
    let secret = "outbound-tool-test-credential-731968420";
    kernel::redact::remember(secret);
    let web = Web::new(Arc::new(Mutex::new(SearchSettings::default())));
    let attack = json!({"op":"fetch", "url":format!("https://unresolvable.invalid/?k={secret}")});
    let (blocked, notice) =
        kernel::egress::scope(guard.clone(), true, async { web.check_outbound(&attack) }).await;
    assert!(matches!(blocked, Err(ToolError::Security(_))));
    let review = notice.unwrap();
    assert!(!review.to_string().contains(secret));
    let (blocked, duplicate) =
        kernel::egress::scope(guard.clone(), true, web.execute(&attack)).await;
    assert!(matches!(blocked, Err(ToolError::Security(_))));
    assert!(duplicate.is_none());
    guard.approve(review["id"].as_str().unwrap()).unwrap();
    let payload = json!({"url":attack["url"]});
    let (result, _) = kernel::egress::scope(guard.clone(), true, async {
        web.check_outbound(&attack)?;
        web.check_outbound(&attack)?;
        kernel::egress::check("web.fetch", &payload).map_err(ToolError::Security)
    })
    .await;
    assert!(result.is_ok(), "preflight consumed the permission to send");
    let (result, _) =
        kernel::egress::scope(guard.clone(), true, async { web.check_outbound(&attack) }).await;
    assert!(matches!(result, Err(ToolError::Security(_))));
    for args in [
        json!({"op":"search","query":secret}),
        json!({"op":"crawl","url":"https://docs.invalid","instructions":secret}),
    ] {
        let (blocked, notice) =
            kernel::egress::scope(guard.clone(), true, async { web.check_outbound(&args) }).await;
        assert!(matches!(blocked, Err(ToolError::Security(_))));
        assert!(notice.is_some());
    }
}
