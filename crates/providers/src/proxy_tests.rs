use crate::{
    AuthKind, ProviderClient, ProviderProfile, TokenCounter,
    test_http::{Response, Server},
};
use futures::StreamExt;
use kernel::{CompiledContext, Message, Provider};

#[tokio::test]
async fn explicit_proxy_carries_generation_counting_and_discovery_for_one_profile() {
    let proxy = Server::scripted(vec![
        Response::json(r#"{"count":42}"#),
        Response::json(r#"{"choices":[{"message":{"role":"assistant","content":"finished"},"finish_reason":"stop"}]}"#),
        Response::json(r#"{"data":[{"id":"model"}]}"#),
    ]).await;
    let mut profile =
        ProviderProfile::openai_chat("http://deployment.invalid/v1", "model", AuthKind::Bearer);
    profile.proxy = Some(proxy.url.clone());
    profile.token_counter = TokenCounter::Vllm;
    let provider =
        ProviderClient::from_profile(profile.clone(), "explicit-proxy-test-key").unwrap();
    provider.set_streaming(false);
    let context = CompiledContext {
        model: String::new(),
        messages: vec![Message::user("hello")],
        ordered: None,
        tools: vec![],
    };
    let request = provider.prepare_request(&context).unwrap();
    assert_eq!(
        provider
            .count_input_tokens(&request)
            .await
            .unwrap()
            .unwrap()
            .tokens,
        42
    );
    let mut stream = provider.stream_prepared(&request).await.unwrap();
    while let Some(block) = stream.next().await {
        block.unwrap();
    }
    assert_eq!(
        crate::discovery::list_models_for_profile(&profile, "explicit-proxy-test-key")
            .await
            .unwrap()[0]
            .id,
        "model"
    );
    let seen = proxy.requests.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(
        seen[0]
            .0
            .starts_with("POST http://deployment.invalid/tokenize ")
    );
    assert!(
        seen[1]
            .0
            .starts_with("POST http://deployment.invalid/v1/chat/completions ")
    );
    assert!(
        seen[2]
            .0
            .starts_with("GET http://deployment.invalid/v1/models ")
    );
    for (headers, _) in seen.iter() {
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer explicit-proxy-test-key")
        );
    }
}
