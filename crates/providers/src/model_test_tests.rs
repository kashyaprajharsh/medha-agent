use super::*;
use crate::{
    AuthKind,
    test_http::{Response, Server},
};
use serde_json::{Value, json};

fn text_json() -> Response {
    Response::json(json!({"choices":[{"message":{"role":"assistant","content":"MEDHA_OK"},"finish_reason":"stop"}]}).to_string())
}
fn text_stream(text: &str, complete: bool) -> Response {
    let delta = json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text}}]});
    let mut body = format!("data: {delta}\n\n");
    if complete {
        body.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");
    }
    Response::sse(body)
}
fn tool_stream(name: &str) -> Response {
    let delta = json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"fixture-call","type":"function","function":{"name":name,"arguments":"{\"value\":\"medha-check\"}"}}]}}]});
    Response::sse(format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    ))
}

#[tokio::test]
async fn setup_checks_real_chat_adapter_and_synthetic_result_without_executing_a_tool() {
    let server = Server::scripted(vec![
        text_json(),
        text_stream("MEDHA_OK", true),
        tool_stream(TOOL),
        text_stream("MEDHA_OK", true),
    ])
    .await;
    let mut profile = ProviderProfile::openai_chat(
        format!("{}/v1", server.url),
        "fixture-model",
        AuthKind::Bearer,
    );
    profile.max_output_tokens = Some(2048);
    let result = test(&profile, "dummy-key").await;
    assert_eq!(
        (
            result.text.status,
            result.streaming.status,
            result.tools.status
        ),
        (Status::Passed, Status::Passed, Status::Passed),
        "{}",
        result.tools.message
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        4,
        "one attempt per check, plus one synthetic result"
    );
    let bodies: Vec<Value> = requests
        .iter()
        .map(|(head, body)| {
            assert!(head.starts_with("POST /v1/chat/completions "));
            assert!(
                head.to_ascii_lowercase()
                    .contains("authorization: bearer dummy-key")
            );
            serde_json::from_slice(body).unwrap()
        })
        .collect();
    assert_eq!(bodies[0]["stream"], false);
    assert!(bodies[1..].iter().all(|body| body["stream"] == true));
    assert!(bodies.iter().all(|body| body["max_tokens"] == 1024));
    assert_eq!(bodies[2]["tools"].as_array().unwrap().len(), 1);
    assert_eq!(bodies[2]["tools"][0]["function"]["name"], TOOL);
    assert_eq!(
        bodies[3]["messages"][1]["tool_calls"][0]["id"],
        "fixture-call"
    );
    assert_eq!(bodies[3]["messages"][2]["role"], "tool");
    assert_eq!(
        bodies[3]["messages"][2]["content"],
        r#"{"value":"medha-check"}"#
    );
    assert!(
        !bodies[3]
            .get("tools")
            .is_some_and(|value| !value.as_array().unwrap().is_empty())
    );
    assert!(
        !bodies
            .iter()
            .any(|body| body.to_string().contains("/Users/"))
    );
}

fn gemini_stream(steps: Vec<Value>, status: &str) -> Response {
    let mut body = String::new();
    for (index, step) in steps.into_iter().enumerate() {
        body.push_str(&format!(
            "event: step.start\ndata: {}\n\nevent: step.stop\ndata: {}\n\n",
            json!({"event_type":"step.start","index":index,"step":step}),
            json!({"event_type":"step.stop","index":index})
        ));
    }
    body.push_str(&format!(
        "event: interaction.completed\ndata: {}\n\n",
        json!({"event_type":"interaction.completed","interaction":{"status":status}})
    ));
    Response::sse(body)
}
fn gemini_text() -> Value {
    json!({"type":"model_output","content":[{"type":"text","text":"MEDHA_OK"}]})
}

#[tokio::test]
async fn setup_replays_gemini_signature_and_function_result_through_the_production_adapter() {
    let text = gemini_text();
    let mut tools = gemini_stream(
        vec![
            json!({"type":"thought"}),
            json!({"type":"function_call","id":"gemini-call","name":TOOL,"arguments":{"value":"medha-check"}}),
        ],
        "requires_action",
    );
    // The signature arrives as a delta, just as in the vendor stream.
    tools.body = tools.body.replacen("event: step.stop", "event: step.delta\ndata: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"thought_signature\",\"signature\":\"setup-signature\"}}\n\nevent: step.stop", 1);
    let server = Server::scripted(vec![
        Response::json(json!({"status":"completed","steps":[text.clone()]}).to_string()),
        gemini_stream(vec![text.clone()], "completed"),
        tools,
        gemini_stream(vec![text], "completed"),
    ])
    .await;
    let profile = ProviderProfile {
        protocol: kernel::Protocol::GeminiInteractions,
        ..ProviderProfile::openai_chat(
            format!("{}/v1", server.url),
            "gemini-fixture",
            AuthKind::XGoogApiKey,
        )
    };
    let result = test(&profile, "gemini-dummy").await;
    assert_eq!(
        result.text.status,
        Status::Passed,
        "{}",
        result.text.message
    );
    assert_eq!(
        result.streaming.status,
        Status::Passed,
        "{}",
        result.streaming.message
    );
    assert_eq!(
        result.tools.status,
        Status::Passed,
        "{}",
        result.tools.message
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let final_body: Value = serde_json::from_slice(&requests[3].1).unwrap();
    assert!(
        requests[3]
            .0
            .to_ascii_lowercase()
            .contains("x-goog-api-key: gemini-dummy")
    );
    assert_eq!(final_body["store"], false);
    assert!(
        final_body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["signature"] == "setup-signature")
    );
    assert!(
        final_body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["type"] == "function_result" && part["call_id"] == "gemini-call")
    );
}

#[tokio::test]
async fn setup_classifies_failures_redacts_echoed_credentials_and_never_retries() {
    for (status, category) in [
        ("401 Unauthorized", "credentials_rejected"),
        ("403 Forbidden", "access_restricted"),
        ("429 Too Many Requests", "throttled"),
        ("503 Unavailable", "unavailable"),
    ] {
        let server = Server::scripted(vec![Response {
            status,
            ..Response::json(r#"{"error":{"message":"access failed for dummy-secret"}}"#)
        }])
        .await;
        let profile = ProviderProfile::openai_chat(&server.url, "fixture", AuthKind::Bearer);
        let result = test(&profile, "dummy-secret").await;
        assert_eq!(result.text.status, Status::Failed);
        assert_eq!(result.text.failure.as_deref(), Some(category));
        assert!(!result.text.message.contains("dummy-secret"));
        assert_eq!(result.streaming.status, Status::Skipped);
        assert_eq!(result.tools.status, Status::Skipped);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn partial_stream_does_not_pass_and_an_unoffered_tool_is_never_executed() {
    let server = Server::scripted(vec![text_json(), text_stream("partial", false)]).await;
    let profile = ProviderProfile::openai_chat(&server.url, "fixture", AuthKind::None);
    let result = test(&profile, "").await;
    assert_eq!(result.text.status, Status::Passed);
    assert_eq!(result.streaming.status, Status::Failed);
    assert_eq!(result.tools.status, Status::Skipped);
    let server = Server::scripted(vec![
        text_json(),
        text_stream("okay", true),
        tool_stream("shell"),
    ])
    .await;
    let profile = ProviderProfile::openai_chat(&server.url, "fixture", AuthKind::None);
    let result = test(&profile, "").await;
    assert_eq!(result.tools.status, Status::Failed);
    assert_eq!(
        server.requests.lock().unwrap().len(),
        3,
        "an unoffered call gets no result request"
    );
}

#[tokio::test]
async fn setup_output_and_absolute_deadline_are_bounded() {
    let server = Server::scripted(vec![text_stream(&"x".repeat(MAX_BYTES + 1), true)]).await;
    let profile = ProviderProfile::openai_chat(&server.url, "fixture", AuthKind::None);
    let client = ProviderClient::from_profile(profile, String::new()).unwrap();
    let context = CompiledContext {
        model: "fixture".into(),
        messages: vec![Message::user("synthetic")],
        ordered: None,
        tools: vec![],
    };
    let error = call(
        &client,
        &context,
        true,
        tokio::time::Instant::now() + Duration::from_secs(2),
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("exceeded"));
    let server = Server::scripted(vec![Response {
        delay: Duration::from_millis(250),
        ..text_json()
    }])
    .await;
    let client = ProviderClient::from_profile(
        ProviderProfile::openai_chat(&server.url, "fixture", AuthKind::None),
        String::new(),
    )
    .unwrap();
    let start = tokio::time::Instant::now();
    let error = call(&client, &context, false, start + Duration::from_millis(50))
        .await
        .err()
        .unwrap();
    assert!(matches!(error, ProviderError::ProgressTimeout { .. }));
    assert!(start.elapsed() < Duration::from_secs(1));
}
