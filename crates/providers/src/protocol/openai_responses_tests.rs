use super::*;
use kernel::{
    BlastRadius, ReasoningPart, RequestScope, TextPart, ToolCallPart, ToolCategory, ToolResultPart,
    ToolSpec,
};

fn profile(plan: bool) -> ProviderProfile {
    let auth = if plan {
        AuthKind::ChatGptPlan
    } else {
        AuthKind::Bearer
    };
    let mut profile = ProviderProfile::openai_chat("https://API.openai.com/v1/", "gpt-x", auth);
    profile.protocol = Protocol::OpenAiResponses;
    profile.reasoning = ReasoningSupport::Effort;
    profile.max_output_tokens = Some(4_096);
    profile
}

fn sealed(server: &str, model: &str) -> Vec<ProviderState> {
    vec![ProviderState {
        protocol: Protocol::OpenAiResponses,
        kind: REASONING.into(),
        value: json!({
            "item": {"encrypted_content": format!("blob-{model}"), "summary": [{"type": "summary_text", "text": "thought"}]},
            "server": server,
            "model": model,
        }),
    }]
}

fn message(role: Role, parts: Vec<ContentPart>) -> ModelMessage {
    ModelMessage {
        role,
        parts,
        trust: None,
    }
}

fn text(value: &str) -> ContentPart {
    ContentPart::Text(TextPart {
        text: value.into(),
        provider_state: Vec::new(),
    })
}

fn thought(state: Vec<ProviderState>) -> ContentPart {
    ContentPart::Reasoning(ReasoningPart {
        text: Some("thought".into()),
        provider_state: state,
    })
}

fn history(reasoning: Vec<ContentPart>) -> Vec<ModelMessage> {
    let mut assistant = reasoning;
    assistant.push(text("Reading it."));
    assistant.push(ContentPart::ToolCall(ToolCallPart {
        id: "call_1".into(),
        tool: "fs.read".into(),
        args: json!({"path": "a.rs"}),
        provider_state: Vec::new(),
    }));
    vec![
        message(Role::System, vec![text("Be brief.")]),
        message(Role::User, vec![text("open a.rs")]),
        message(Role::Assistant, assistant),
        message(
            Role::Tool,
            vec![ContentPart::ToolResult(ToolResultPart {
                tool_call_id: "call_1".into(),
                content: "fn main() {}".into(),
                provider_state: Vec::new(),
            })],
        ),
    ]
}

fn body(profile: &ProviderProfile, messages: Vec<ModelMessage>) -> Result<Value, ProviderError> {
    let context = CompiledContext {
        model: String::new(),
        messages: Vec::new(),
        ordered: Some(messages),
        tools: vec![ToolSpec {
            name: "fs.read".into(),
            description: "read a file".into(),
            schema: json!({"type": "object"}),
            blast_radius: BlastRadius::Read,
            category: ToolCategory::Read,
            icon: "r".into(),
        }],
    };
    prepare_body(
        &context,
        &Settings {
            profile,
            model: "gpt-x",
            streaming: false,
            reasoning: &ReasoningConfig::default(),
            plain_schemas: false,
            scope: &RequestScope::default(),
            stable_system: None,
            replay_after: None,
        },
    )
}

fn kinds(body: &Value) -> Vec<&str> {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["type"].as_str().unwrap())
        .collect()
}

fn event(value: Value) -> SseEvent {
    SseEvent {
        event: None,
        data: value.to_string(),
    }
}

fn decoder() -> ResponseDecoder {
    let names = HashMap::from([("fs_read".to_string(), "fs.read".to_string())]);
    let origin = Origin {
        server: "https://api.openai.com/v1/".into(),
        model: "gpt-x".into(),
    };
    ResponseDecoder::new(names, origin)
}

#[test]
fn api_route_request_carries_instructions_tools_and_history_without_ids() {
    let profile = profile(false);
    let here = sealed("https://api.openai.com/v1", "gpt-x");
    let body = body(&profile, history(vec![thought(here)])).unwrap();

    assert_eq!(body["instructions"], "Be brief.");
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 4_096);
    assert_eq!(body["reasoning"], json!({"summary": "auto"}));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "fs_read");
    assert_eq!(
        kinds(&body),
        [
            "message",
            "reasoning",
            "message",
            "function_call",
            "function_call_output"
        ]
    );
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[1]["encrypted_content"], "blob-gpt-x");
    assert_eq!(input[3]["arguments"], r#"{"path":"a.rs"}"#);
    assert!(input[3].get("namespace").is_none());
    assert_eq!(input[4]["call_id"], "call_1");
    assert!(
        input.iter().all(|item| item.get("id").is_none()),
        "store:false cannot look ids up"
    );
}

#[test]
fn plan_route_request_leaves_out_what_plan_tokens_refuse() {
    let profile = profile(true);
    let mut body = body(&profile, history(Vec::new())).unwrap();

    assert_eq!(body["stream"], true, "the plan route only streams");
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("temperature").is_none() && body.get("previous_response_id").is_none());
    assert_eq!(body["tools"][0]["type"], "namespace");
    assert_eq!(body["tools"][0]["tools"][0]["name"], "fs_read");
    let call = &body["input"][2];
    assert_eq!(
        (call["type"].as_str(), call["namespace"].as_str()),
        (Some("function_call"), Some("functions"))
    );
    let object = body.as_object_mut().unwrap();
    assert!(!Responses.set_output_limit(object, 100, &profile).unwrap());
    assert!(object.get("max_output_tokens").is_none());
}

#[test]
fn reasoning_goes_back_only_to_the_server_and_model_that_sealed_it() {
    let profile = profile(false);
    let gemini = vec![ProviderState {
        protocol: Protocol::GeminiInteractions,
        kind: "thought_signature".into(),
        value: json!("sig"),
    }];
    let sent = body(
        &profile,
        history(vec![
            thought(sealed("https://api.openai.com/v1", "gpt-x")),
            thought(sealed("https://api.openai.com/v1", "gpt-other")),
            thought(sealed("https://proxy.example/v1", "gpt-x")),
            thought(gemini),
        ]),
    )
    .unwrap();
    let replayed: Vec<_> = sent["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "reasoning")
        .map(|item| item["encrypted_content"].as_str().unwrap())
        .collect();
    assert_eq!(replayed, ["blob-gpt-x"]);

    let misplaced = vec![message(
        Role::Assistant,
        vec![ContentPart::Text(TextPart {
            text: "hi".into(),
            provider_state: sealed("https://api.openai.com/v1", "gpt-x"),
        })],
    )];
    assert!(body(&profile, misplaced).is_err());
}

#[test]
fn a_streamed_reply_releases_tools_only_at_completion_with_stamped_reasoning_and_usage_once() {
    let mut decoder = decoder();
    let mut blocks = Vec::new();
    let call =
        json!({"type": "function_call", "call_id": "call_9", "name": "fs_read", "arguments": ""});
    for value in [
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning"}}),
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "summary_index": 0, "delta": "first"}),
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "summary_index": 1, "delta": "second"}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": call}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"path\":\"src/a.rs\"}"}),
    ] {
        blocks.extend(decoder.push(&event(value)).unwrap());
    }
    assert!(
        !blocks
            .iter()
            .any(|block| matches!(block, Block::ToolIntent(_)))
    );
    assert!(blocks.iter().any(
        |block| matches!(block, Block::ToolStarted { target: Some(path), .. } if path == "src/a.rs")
    ));
    let thinking: String = blocks
        .iter()
        .filter_map(|block| match block {
            Block::Reasoning(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, "first\n\nsecond");

    let output = json!([
        {"type": "reasoning", "id": "rs_1", "encrypted_content": "sealed", "summary": [{"type": "summary_text", "text": "first"}]},
        {"type": "function_call", "id": "fc_1", "call_id": "call_9", "name": "fs_read", "arguments": "{\"path\":\"src/a.rs\"}"},
    ]);
    let usage = json!({"input_tokens": 100, "input_tokens_details": {"cached_tokens": 40}, "output_tokens": 50, "output_tokens_details": {"reasoning_tokens": 30}, "total_tokens": 150});
    let done = decoder
        .push(&event(json!({"type": "response.completed", "response": {"status": "completed", "output": output, "usage": usage}})))
        .unwrap();
    assert!(decoder.completed());
    let Some(Block::ToolIntent(intent)) = done.first() else {
        panic!("{done:?}")
    };
    assert_eq!(
        (intent.id.as_str(), intent.tool.as_str()),
        ("call_9", "fs.read")
    );
    let Some(Block::CompletedMessage(kept)) = done.get(1) else {
        panic!("{done:?}")
    };
    let ContentPart::Reasoning(reasoning) = &kept.parts[0] else {
        panic!("{kept:?}")
    };
    assert_eq!(
        reasoning.provider_state[0].value["server"],
        "https://api.openai.com/v1"
    );
    assert_eq!(reasoning.provider_state[0].value["model"], "gpt-x");
    let Some(Block::Usage(usage)) = done.last() else {
        panic!("{done:?}")
    };
    assert_eq!(
        (usage.completion_tokens, usage.cached_prompt_tokens),
        (50, Some(40))
    );
}

#[test]
fn a_reply_that_did_not_complete_runs_no_tool() {
    let call = json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": "fs_read", "arguments": "{}"}});
    for (terminal, check) in [
        (
            json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}}),
            (|error: &ProviderError| matches!(error, ProviderError::Truncated))
                as fn(&ProviderError) -> bool,
        ),
        (
            json!({"type": "response.failed", "response": {"status": "failed", "error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "limit"}}}),
            |error| {
                error
                    .to_string()
                    .contains("subscription_sharing_usage_limit_exceeded")
            },
        ),
    ] {
        let mut decoder = decoder();
        assert!(decoder.push(&event(call.clone())).unwrap().is_empty());
        let error = decoder.push(&event(terminal)).unwrap_err();
        assert!(check(&error), "{error:?}");
        assert!(decoder.finish().is_err());
    }
    let whole = json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": []});
    assert!(matches!(
        decoder().parse_whole(&whole.to_string()),
        Err(ProviderError::Truncated)
    ));
}

#[test]
fn a_malformed_event_fails_and_an_unknown_one_passes() {
    let mut decoder = decoder();
    let unknown = json!({"type": "response.some_future_event", "data": 1});
    assert!(decoder.push(&event(unknown)).unwrap().is_empty());
    let broken = SseEvent {
        event: None,
        data: "{\"type\": \"response.output_text.delta\", ".into(),
    };
    assert!(decoder.push(&broken).is_err());
}
