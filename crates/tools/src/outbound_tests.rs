use super::*;

#[tokio::test]
async fn unavailable_selected_provider_never_falls_through_to_keyless_search() {
    let search = WebSearch {
        search: Arc::new(Mutex::new(SearchSettings {
            provider: SearchProvider::Tavily,
            captured: true,
            ..SearchSettings::default()
        })),
    };
    let error = search
        .execute(&json!({"query":"do not transmit this query"}))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "tavily: no API key configured");
    assert_eq!(
        search
            .check_outbound(&json!({"query":"test"}))
            .unwrap_err()
            .to_string(),
        error.to_string()
    );
}

#[tokio::test]
async fn a_key_alone_does_not_enable_crawling() {
    let crawl = WebCrawl {
        search: Arc::new(Mutex::new(SearchSettings {
            tavily_key: Some("configured-search-secret-890123456".into()),
            captured: true,
            ..SearchSettings::default()
        })),
    };
    let error = crawl
        .execute(&json!({"url":"https://example.org"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("crawl is off"));
    assert!(!error.to_string().contains("configured-search-secret"));
}

#[tokio::test]
#[ignore = "explicit live keyless-search check; no paid provider"]
async fn live_keyless_search_with_the_final_application_identity() {
    assert_eq!(BROWSER_UA, "Medha/0.1");
    let response = http_client()
        .unwrap()
        .post("https://html.duckduckgo.com/html/")
        .form(&[("q", "Rust programming language")])
        .send()
        .await
        .unwrap();
    let status = response.status();
    let html = response.text().await.unwrap();
    let count = parse_ddg(&html, 10).len();
    println!(
        "UA={BROWSER_UA}; HTTP={status}; results={count}; anomaly={}",
        html.contains("anomaly")
    );
    assert!(status.is_success());
    assert_eq!(count, 10);
}

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
