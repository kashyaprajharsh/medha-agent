use super::*;
use kernel::{
    BlastRadius, Message, ReasoningPart, TextPart, ToolCallPart, ToolCategory, ToolResultPart,
    ToolSpec,
};

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: "read a file".into(),
        schema: json!({"type": "object"}),
        blast_radius: BlastRadius::Read,
        category: ToolCategory::Read,
        icon: "t".into(),
    }
}

fn user_with_image(mime: &str, source: kernel::MediaSource) -> CompiledContext {
    CompiledContext {
        model: String::new(),
        messages: Vec::new(),
        ordered: Some(vec![ModelMessage {
            role: Role::User,
            parts: vec![
                ContentPart::Text(TextPart {
                    text: "what is this?".into(),
                    provider_state: Vec::new(),
                }),
                ContentPart::Media(kernel::MediaPart {
                    mime_type: mime.into(),
                    source,
                    label: None,
                    width: None,
                    height: None,
                    byte_size: None,
                    provider_state: Vec::new(),
                }),
            ],
            trust: None,
        }]),
        tools: Vec::new(),
    }
}

fn gemini_body(context: &CompiledContext) -> Result<Value, ProviderError> {
    prepare_body(
        context,
        "gemini-model",
        false,
        &ReasoningConfig::default(),
        None,
    )
    .map(|(body, _)| body)
}

#[test]
fn auxiliary_reasoning_override_preserves_the_independent_output_cap() {
    let mut body = serde_json::json!({"generation_config": {
        "max_output_tokens": 4096, "thinking_level": "high", "thinking_summaries": "auto",
    }});
    apply_reasoning(
        &mut body,
        &ReasoningConfig {
            enabled: Some(true),
            effort: Some(ReasoningEffort::Minimal),
        },
    )
    .unwrap();
    assert_eq!(body["generation_config"]["max_output_tokens"], 4096);
    assert_eq!(body["generation_config"]["thinking_level"], "minimal");
}

#[test]
fn a_user_image_lowers_to_an_inline_interactions_image_part() {
    let context = user_with_image("image/png", kernel::MediaSource::Base64("QUJD".into()));
    let body = gemini_body(&context).unwrap();
    assert_eq!(
        body["input"][0]["content"],
        json!([
            {"type": "text", "text": "what is this?"},
            {"type": "image", "mime_type": "image/png", "data": "QUJD"},
        ])
    );
}

#[test]
fn an_uploaded_image_is_referenced_by_uri_instead_of_bytes() {
    let context = user_with_image(
        "image/jpeg",
        kernel::MediaSource::Url("https://generativelanguage.googleapis.com/v1/files/x".into()),
    );
    let body = gemini_body(&context).unwrap();
    assert_eq!(
        body["input"][0]["content"][1],
        json!({
            "type": "image",
            "mime_type": "image/jpeg",
            "uri": "https://generativelanguage.googleapis.com/v1/files/x",
        })
    );
}

#[test]
fn unresolved_and_unsupported_images_are_refused_with_the_reason() {
    let unresolved = user_with_image("image/png", kernel::MediaSource::Artifact("h".into()));
    assert!(
        gemini_body(&unresolved)
            .unwrap_err()
            .to_string()
            .contains("must be resolved")
    );
    let gif = user_with_image("image/gif", kernel::MediaSource::Base64("QUJD".into()));
    let error = gemini_body(&gif).unwrap_err().to_string();
    assert!(
        error.contains("image/gif") && error.contains("PNG, JPEG, WebP"),
        "{error}"
    );
}

#[test]
fn standard_function_call_rejects_misplaced_signature() {
    let sig = ProviderState {
        protocol: Protocol::GeminiInteractions,
        kind: THOUGHT_SIGNATURE.into(),
        value: json!("fc-sig"),
    };
    let ordered = vec![
        Message::user("do it").ordered(),
        ModelMessage {
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "c1".into(),
                tool: "fs.read".into(),
                args: json!({"path": "a"}),
                provider_state: vec![sig],
            })],
            trust: None,
        },
        ModelMessage {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResultPart {
                tool_call_id: "c1".into(),
                content: "x".into(),
                provider_state: Vec::new(),
            })],
            trust: None,
        },
    ];
    let context = CompiledContext {
        model: String::new(),
        messages: Vec::new(),
        ordered: Some(ordered),
        tools: vec![tool("fs.read")],
    };
    let error = prepare_body(
        &context,
        "gemini-model",
        true,
        &ReasoningConfig {
            enabled: Some(true),
            effort: Some(ReasoningEffort::Minimal),
        },
        Some(4096),
    )
    .unwrap_err();
    assert!(error.to_string().contains("standard function call"));
}

#[test]
fn completed_text_turn_thought_is_replayed_exactly() {
    let signature = ProviderState {
        protocol: Protocol::GeminiInteractions,
        kind: THOUGHT_SIGNATURE.into(),
        value: json!("stale-sig"),
    };
    let ordered = vec![
        Message::user("explain databases").ordered(),
        ModelMessage {
            role: Role::Assistant,
            parts: vec![
                ContentPart::Reasoning(ReasoningPart {
                    text: None,
                    provider_state: vec![signature],
                }),
                ContentPart::Text(TextPart {
                    text: "here is the explanation".into(),
                    provider_state: Vec::new(),
                }),
            ],
            trust: None,
        },
        Message::user("now pick one").ordered(),
    ];
    let context = CompiledContext {
        model: String::new(),
        messages: Vec::new(),
        ordered: Some(ordered),
        tools: Vec::new(),
    };
    let (body, _) = prepare_body(
        &context,
        "gemini-model",
        true,
        &ReasoningConfig {
            enabled: Some(true),
            effort: Some(ReasoningEffort::Minimal),
        },
        Some(4096),
    )
    .unwrap();
    let thought = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["type"] == "thought")
        .expect("stateless mode must replay every thought");
    assert_eq!(thought["signature"], "stale-sig");
    assert_eq!(thought["summary"], json!([]));
}

#[test]
fn stateless_request_replays_thought_signature_and_function_timeline() {
    let signature = ProviderState {
        protocol: Protocol::GeminiInteractions,
        kind: THOUGHT_SIGNATURE.into(),
        value: json!("signed-value"),
    };
    let ordered = vec![
        Message::system("system").ordered(),
        Message::user("read it").ordered(),
        ModelMessage {
            role: Role::Assistant,
            parts: vec![
                ContentPart::Reasoning(ReasoningPart {
                    text: Some("checking".into()),
                    provider_state: vec![signature],
                }),
                ContentPart::ToolCall(ToolCallPart {
                    id: "call-1".into(),
                    tool: "fs.read".into(),
                    args: json!({"path": "README.md"}),
                    provider_state: Vec::new(),
                }),
            ],
            trust: None,
        },
        ModelMessage {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResultPart {
                tool_call_id: "call-1".into(),
                content: "contents".into(),
                provider_state: Vec::new(),
            })],
            trust: None,
        },
    ];
    let context = CompiledContext {
        model: String::new(),
        messages: Vec::new(),
        ordered: Some(ordered),
        tools: vec![tool("fs.read")],
    };
    let (body, _) = prepare_body(
        &context,
        "gemini-model",
        true,
        &ReasoningConfig {
            enabled: Some(true),
            effort: Some(ReasoningEffort::Medium),
        },
        Some(4096),
    )
    .unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["system_instruction"], "system");
    assert_eq!(body["generation_config"]["thinking_level"], "medium");
    assert_eq!(body["input"][1]["signature"], "signed-value");
    assert_eq!(body["input"][2]["name"], "fs_read");
    assert_eq!(body["input"][3]["type"], "function_result");
    assert_eq!(body["input"][3]["name"], "fs_read");
    assert_eq!(body["input"][3]["call_id"], "call-1");
}

#[test]
fn minimal_effort_maps_to_the_stable_gemini_level() {
    let context = CompiledContext {
        model: String::new(),
        messages: vec![Message::user("hello")],
        ordered: None,
        tools: Vec::new(),
    };
    let (body, _) = prepare_body(
        &context,
        "gemini-model",
        true,
        &ReasoningConfig {
            enabled: Some(true),
            effort: Some(ReasoningEffort::Minimal),
        },
        None,
    )
    .unwrap();
    assert_eq!(body["generation_config"]["thinking_level"], "minimal");
    assert_eq!(body["generation_config"]["thinking_summaries"], "auto");
}

#[test]
fn nonstream_response_preserves_signature_and_normalizes_usage() {
    let body = json!({
        "status": "requires_action",
        "steps": [
            {"type":"thought","signature":"sig","summary":[{"type":"text","text":"plan"}]},
            {"type":"function_call","id":"c1","name":"fs_read","arguments":{"path":"a"}}
        ],
        "usage": {"total_input_tokens":10,"total_output_tokens":4,"total_tokens":14}
    })
    .to_string();
    let blocks = parse_interaction(
        &body,
        &HashMap::from([("fs_read".into(), "fs.read".into())]),
    )
    .unwrap();
    let completed = blocks
        .iter()
        .find_map(|block| match block {
            Block::CompletedMessage(message) => Some(message),
            _ => None,
        })
        .unwrap();
    assert!(completed.has_provider_state());
    assert!(matches!(&completed.parts[1], ContentPart::ToolCall(call) if call.tool == "fs.read"));
    assert!(matches!(
        blocks.last(),
        Some(Block::Usage(Usage {
            prompt_tokens: 10,
            total_tokens: 14,
            ..
        }))
    ));
}

#[test]
fn incomplete_responses_never_complete_or_release_tool_calls() {
    // Reconstruct the documented terminal shape with synthetic tool data.
    // Validate status before decoding even a partial function argument.
    let body = json!({
        "status": "incomplete",
        "steps": [{"type":"function_call","id":"c1","name":"write","arguments":"{\"path\":"}],
        "usage": {"total_input_tokens":10,"total_output_tokens":20,"total_tokens":30}
    });
    let error = parse_interaction(&body.to_string(), &HashMap::new()).unwrap_err();
    assert!(matches!(error, ProviderError::Truncated), "{error}");
    assert!(!error.is_retryable());

    let mut decoder = ResponseDecoder::new(HashMap::new());
    decoder.push(&SseEvent {
            event: Some("step.start".into()),
            data: json!({"event_type":"step.start","index":0,"step":{"type":"function_call","id":"c1","name":"write","arguments":{}}}).to_string(),
        }).unwrap();
    let error = decoder
        .push(&SseEvent {
            event: Some("interaction.completed".into()),
            data: json!({"event_type":"interaction.completed","interaction":body}).to_string(),
        })
        .unwrap_err();
    assert!(matches!(error, ProviderError::Truncated), "{error}");
    assert!(!decoder.completed());
}

#[test]
fn an_incomplete_stream_with_stopped_steps_is_not_success() {
    let mut decoder = ResponseDecoder::new(HashMap::new());
    for data in [
        json!({"event_type":"step.start","index":0,"step":{"type":"model_output","content":[{"type":"text","text":"partial"}]}}),
        json!({"event_type":"step.stop","index":0}),
    ] {
        decoder
            .push(&SseEvent {
                event: None,
                data: data.to_string(),
            })
            .unwrap();
    }
    let result = decoder.push(&SseEvent {
        event: Some("interaction.completed".into()),
        data: json!({"event_type":"interaction.completed","interaction":{"status":"incomplete"}})
            .to_string(),
    });
    assert!(matches!(result, Err(ProviderError::Truncated)));
    assert!(!decoder.completed());
}

#[test]
fn usage_accounts_for_cached_input_and_billed_thought_once() {
    let usage = json!({"total_input_tokens":21576,"total_output_tokens":1,
            "total_thought_tokens":56,"total_cached_tokens":16356,"total_tokens":21633});
    let body = json!({"status":"completed","steps":[],"usage":usage});
    let blocks = parse_interaction(&body.to_string(), &HashMap::new()).unwrap();
    let Some(Block::Usage(counts)) = blocks.last() else {
        panic!("usage missing");
    };
    assert_eq!(counts.prompt_tokens, 21576);
    assert_eq!(counts.completion_tokens, 57);
    assert_eq!(counts.total_tokens, 21633);
    assert_eq!(counts.cached_prompt_tokens, Some(16356));
    assert_eq!(counts.uncached_prompt_tokens(), 5220);
    let price = kernel::Pricing {
        input_per_mtok: 2.0,
        output_per_mtok: 12.0,
        cached_input_per_mtok: Some(0.2),
        cache_write_per_mtok: None,
        indicative: false,
    };
    let expected = (5220.0 * 2.0 + 16356.0 * 0.2 + 57.0 * 12.0) / 1_000_000.0;
    assert!((price.cost(counts) - expected).abs() < 1e-12);

    // Streaming usage is cumulative, not a sum of every SSE update.
    let mut decoder = ResponseDecoder::new(HashMap::new());
    decoder
        .push(&SseEvent {
            event: None,
            data: json!({"event_type":"interaction.start","metadata":{"total_usage":{
                "total_input_tokens":100,"total_output_tokens":0,"total_thought_tokens":2}}})
            .to_string(),
        })
        .unwrap();
    decoder.push(&SseEvent { event: None,
            data: json!({"event_type":"interaction.status_update","status":"in_progress","metadata":{"total_usage":usage}}).to_string() }).unwrap();
    let blocks = decoder
        .push(&SseEvent {
            event: Some("interaction.completed".into()),
            data:
                json!({"event_type":"interaction.completed","interaction":{"status":"completed"}})
                    .to_string(),
        })
        .unwrap();
    let Some(Block::Usage(streamed)) = blocks.last() else {
        panic!("usage missing");
    };
    assert_eq!(
        serde_json::to_value(streamed).unwrap(),
        serde_json::to_value(counts).unwrap()
    );
    decoder.finish().unwrap();
}

#[test]
fn usage_keeps_unknown_cache_distinct_from_zero_and_bounds_arithmetic() {
    for (value, cached) in [(Value::Null, None), (json!(0), Some(0))] {
        let body = json!({"status":"completed","steps":[],"usage":{
                "total_input_tokens":1,"total_output_tokens":1,"total_cached_tokens":value}});
        let blocks = parse_interaction(&body.to_string(), &HashMap::new()).unwrap();
        let Some(Block::Usage(counts)) = blocks.last() else {
            panic!("usage missing");
        };
        assert_eq!(counts.cached_prompt_tokens, cached);
        assert_eq!(
            counts.completion_tokens, 1,
            "missing thought count adds no invented tokens"
        );
    }
    let body = json!({"status":"completed","steps":[],"usage":{
            "total_input_tokens":u64::MAX,"total_output_tokens":u64::MAX,
            "total_thought_tokens":u64::MAX,"total_cached_tokens":u64::MAX,"total_tokens":u64::MAX}});
    let blocks = parse_interaction(&body.to_string(), &HashMap::new()).unwrap();
    let Some(Block::Usage(counts)) = blocks.last() else {
        panic!("usage missing");
    };
    assert_eq!(counts.prompt_tokens, u32::MAX);
    assert_eq!(counts.completion_tokens, u32::MAX);
    assert_eq!(counts.cached_prompt_tokens, Some(u32::MAX));
}

#[test]
fn partial_usage_updates_preserve_reported_buckets_and_later_zeroes() {
    let mut decoder = ResponseDecoder::new(HashMap::new());
    for usage in [
        json!({"total_input_tokens":100,"total_cached_tokens":80,"total_output_tokens":2,"total_thought_tokens":3}),
        json!({"total_output_tokens":4}),
        json!({"total_cached_tokens":0}),
    ] {
        decoder.push(&SseEvent { event: None,
                data: json!({"event_type":"interaction.status_update","status":"in_progress","metadata":{"total_usage":usage}}).to_string() }).unwrap();
    }
    let blocks = decoder.push(&SseEvent { event: None,
            data: json!({"event_type":"interaction.completed","interaction":{"status":"completed","usage":{"total_tokens":107}}}).to_string() }).unwrap();
    let Some(Block::Usage(counts)) = blocks.last() else {
        panic!("usage missing");
    };
    assert_eq!(counts.prompt_tokens, 100);
    assert_eq!(counts.completion_tokens, 7);
    assert_eq!(counts.cached_prompt_tokens, Some(0));
    assert_eq!(counts.total_tokens, 107);
    decoder.finish().unwrap();

    let body = json!({"status":"completed","steps":[],"usage":{
            "total_input_tokens":10,"total_cached_tokens":100}});
    let blocks = parse_interaction(&body.to_string(), &HashMap::new()).unwrap();
    assert!(
        matches!(blocks.last(), Some(Block::Usage(usage)) if usage.cached_prompt_tokens == Some(10))
    );
}

#[test]
fn nonstream_thought_without_signature_fails_closed() {
    let body = json!({
        "status": "completed",
        "steps": [
            {"type":"thought","summary":[{"type":"text","text":"plan"}]},
            {"type":"model_output","content":[{"type":"text","text":"done"}]}
        ]
    })
    .to_string();
    let error = parse_interaction(&body, &HashMap::new()).unwrap_err();
    assert!(error.to_string().contains("required signature"));
}

#[test]
fn google_rpc_status_controls_in_band_retry_classification() {
    for status in [
        "INTERNAL",
        "UNAVAILABLE",
        "DEADLINE_EXCEEDED",
        "ABORTED",
        "RESOURCE_EXHAUSTED",
    ] {
        let body = json!({
            "status": "failed",
            "error": {
                "code": 500,
                "status": status,
                "message": "request failed"
            }
        })
        .to_string();
        let error = parse_interaction(&body, &HashMap::new()).unwrap_err();
        assert!(matches!(error, ProviderError::Response(_)), "{status}");
        assert!(error.is_retryable(), "{status}");
    }

    let invalid = json!({
        "status": "failed",
        "error": {
            "code": 400,
            "status": "INVALID_ARGUMENT",
            "message": "invalid request parameter"
        }
    })
    .to_string();
    let error = parse_interaction(&invalid, &HashMap::new()).unwrap_err();
    assert!(matches!(error, ProviderError::Response(_)));
    assert!(!error.is_retryable());
}

#[test]
fn streaming_reassembles_arguments_and_signature_before_completion() {
    let mut decoder = ResponseDecoder::new(HashMap::from([("fs_read".into(), "fs.read".into())]));
    let events = [
        (
            "step.start",
            json!({"type":"step.start","index":0,"step":{"type":"thought"}}),
        ),
        (
            "step.delta",
            json!({"type":"step.delta","index":0,"delta":{"type":"thought_summary","content":{"type":"text","text":"plan"}}}),
        ),
        (
            "step.delta",
            json!({"type":"step.delta","index":0,"delta":{"type":"thought_signature","signature":"sig"}}),
        ),
        ("step.stop", json!({"type":"step.stop","index":0})),
        (
            "step.start",
            json!({"type":"step.start","index":1,"step":{"type":"function_call","id":"c1","name":"fs_read","arguments":{}}}),
        ),
        (
            "step.delta",
            json!({"type":"step.delta","index":1,"delta":{"type":"arguments_delta","arguments":"{\"path\":"}}),
        ),
        (
            "step.delta",
            json!({"type":"step.delta","index":1,"delta":{"type":"arguments_delta","arguments":"\"a\"}"},"metadata":{"total_usage":{"total_input_tokens":9,"total_output_tokens":3,"total_tokens":12}}}),
        ),
        ("step.stop", json!({"type":"step.stop","index":1})),
        (
            "interaction.requires_action",
            json!({"type":"interaction.requires_action","interaction":{"status":"requires_action"}}),
        ),
    ];
    let mut blocks = Vec::new();
    for (name, data) in events {
        blocks.extend(
            decoder
                .push(&SseEvent {
                    event: Some(name.into()),
                    data: data.to_string(),
                })
                .unwrap(),
        );
    }
    decoder.finish().unwrap();
    assert!(blocks.iter().any(|block| matches!(block, Block::ToolIntent(intent) if intent.tool == "fs.read" && intent.args["path"] == "a")));
    let completed = blocks
        .iter()
        .find_map(|block| match block {
            Block::CompletedMessage(message) => Some(message),
            _ => None,
        })
        .unwrap();
    assert!(completed.has_provider_state());
    assert!(matches!(
        blocks.last(),
        Some(Block::Usage(Usage {
            prompt_tokens: 9,
            total_tokens: 12,
            ..
        }))
    ));
}

#[test]
fn thought_without_signature_and_missing_step_stop_fail_closed() {
    let mut decoder = ResponseDecoder::new(HashMap::new());
    decoder
            .push(&SseEvent {
                event: Some("step.start".into()),
                data: json!({"event_type":"step.start","index":0,"step":{"type":"thought","signature":"","summary":[]}}).to_string(),
            })
            .unwrap();
    let stop_error = decoder
        .push(&SseEvent {
            event: Some("step.stop".into()),
            data: json!({"event_type":"step.stop","index":0}).to_string(),
        })
        .unwrap_err();
    assert!(stop_error.to_string().contains("required signature"));

    let mut decoder = ResponseDecoder::new(HashMap::new());
    decoder
            .push(&SseEvent {
                event: Some("step.start".into()),
                data: json!({"event_type":"step.start","index":0,"step":{"type":"model_output","content":[{"type":"text","text":"done"}]}}).to_string(),
            })
            .unwrap();
    let terminal_error = decoder
        .push(&SseEvent {
            event: Some("interaction.completed".into()),
            data:
                json!({"event_type":"interaction.completed","interaction":{"status":"completed"}})
                    .to_string(),
        })
        .unwrap_err();
    assert!(terminal_error.to_string().contains("before step.stop"));
}

#[test]
fn stream_cutoff_and_invalid_owned_state_fail_closed() {
    assert!(ResponseDecoder::new(HashMap::new()).finish().is_err());
    let context = CompiledContext {
        model: String::new(),
        messages: Vec::new(),
        ordered: Some(vec![ModelMessage {
            role: Role::Assistant,
            parts: vec![ContentPart::Reasoning(ReasoningPart {
                text: None,
                provider_state: vec![ProviderState {
                    protocol: Protocol::GeminiInteractions,
                    kind: "wrong".into(),
                    value: json!("sig"),
                }],
            })],
            trust: None,
        }]),
        tools: Vec::new(),
    };
    assert!(prepare_body(&context, "m", false, &ReasoningConfig::default(), None).is_err());
}

#[test]
fn stable_error_event_is_not_ignored() {
    let mut decoder = ResponseDecoder::new(HashMap::new());
    let error = SseEvent {
        event: Some("error".into()),
        data: json!({
            "event_type": "error",
            "error": {"code": "not_found", "message": "interaction missing"}
        })
        .to_string(),
    };
    assert!(matches!(
        decoder.push(&error),
        Err(ProviderError::Response(_))
    ));
}

#[test]
fn done_sentinel_is_accepted_only_as_transport_termination() {
    let mut completed = ResponseDecoder::new(HashMap::new());
    completed
        .push(&SseEvent {
            event: Some("interaction.completed".into()),
            data: json!({
                "event_type": "interaction.completed",
                "interaction": {"status": "completed"}
            })
            .to_string(),
        })
        .unwrap();
    assert!(
        completed
            .push(&SseEvent {
                event: Some("done".into()),
                data: "[DONE]".into(),
            })
            .unwrap()
            .is_empty()
    );
    completed.finish().unwrap();

    let mut cutoff = ResponseDecoder::new(HashMap::new());
    cutoff
        .push(&SseEvent {
            event: Some("done".into()),
            data: "[DONE]".into(),
        })
        .unwrap();
    assert!(cutoff.finish().is_err());

    let mut malformed = ResponseDecoder::new(HashMap::new());
    assert!(
        malformed
            .push(&SseEvent {
                event: Some("step.delta".into()),
                data: "[DONE]".into(),
            })
            .is_err()
    );
}

#[test]
fn arbitrary_sse_byte_splits_preserve_the_complete_interaction() {
    let fixture = concat!(
            "event: step.start\n",
            "data: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"thought\"}}\n\n",
            "event: step.delta\n",
            "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"thought_signature\",\"signature\":\"sig\"}}\n\n",
            "event: step.stop\n",
            "data: {\"event_type\":\"step.stop\",\"index\":0}\n\n",
            "event: step.start\n",
            "data: {\"event_type\":\"step.start\",\"index\":1,\"step\":{\"type\":\"model_output\",\"content\":[{\"type\":\"text\",\"text\":\"He\"}]}}\n\n",
            "event: step.delta\n",
            "data: {\"event_type\":\"step.delta\",\"index\":1,\"delta\":{\"type\":\"text\",\"text\":\"llo\"}}\n\n",
            "event: step.stop\n",
            "data: {\"event_type\":\"step.stop\",\"index\":1}\n\n",
            "event: interaction.completed\n",
            "data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"status\":\"completed\"}}\n\n",
            "event: done\n",
            "data: [DONE]\n\n"
        )
        .as_bytes();

    for split in 0..=fixture.len() {
        let mut framing = crate::transport::sse::SseDecoder::default();
        let mut decoder = ResponseDecoder::new(HashMap::new());
        let mut blocks = Vec::new();
        for chunk in [&fixture[..split], &fixture[split..]] {
            for event in framing.push(chunk).unwrap() {
                blocks.extend(decoder.push(&event).unwrap());
            }
        }
        if let Some(event) = framing.finish() {
            blocks.extend(decoder.push(&event).unwrap());
        }
        decoder.finish().unwrap();
        let visible: String = blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(visible, "Hello", "failed at byte split {split}");
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::CompletedMessage(message) if message.has_provider_state()
        )));
    }
}

#[test]
fn nonterminal_and_cancelled_responses_cannot_authorize_calls() {
    for status in [Value::Null, json!("in_progress")] {
        let body = json!({"status":status, "steps":[
            {"type":"function_call", "id":"c", "name":"write", "arguments":{}}
        ]})
        .to_string();
        assert!(matches!(
            parse_interaction(&body, &HashMap::new()),
            Err(ProviderError::Stream(_))
        ));
    }
    let body = json!({"status":"cancelled", "steps":[
        {"type":"function_call","id":"c","name":"write","arguments":{}}
    ]})
    .to_string();
    let error = parse_interaction(&body, &HashMap::new()).unwrap_err();
    assert!(matches!(error, ProviderError::Response(_)));
    assert!(
        !error.is_retryable(),
        "a cancelled interaction must not resubmit the prompt"
    );
}

#[test]
fn invalid_calls_never_release_a_partial_batch() {
    for arguments in [json!([]), json!("encoded"), Value::Null] {
        let body = json!({"status":"requires_action", "steps":[
            {"type":"function_call", "id":"ok", "name":"read", "arguments":{}},
            {"type":"function_call", "id":"bad", "name":"read", "arguments":arguments}
        ]})
        .to_string();
        assert!(matches!(
            parse_interaction(&body, &HashMap::new()),
            Err(ProviderError::InvalidToolCall(_))
        ));
    }
    let body =
            json!({"status":"requires_action", "steps":[{"type":"function_call", "id":"bad", "arguments":{}}]}).to_string();
    assert!(matches!(
        parse_interaction(&body, &HashMap::new()),
        Err(ProviderError::InvalidToolCall(_))
    ));

    for arguments in [r#"{"query":"unfinished"#, "[]"] {
        let mut decoder = ResponseDecoder::new(HashMap::new());
        for value in [
            json!({"event_type":"step.start", "index":0, "step":{"type":"function_call", "id":"ok", "name":"read", "arguments":{}}}),
            json!({"event_type":"step.stop", "index":0}),
            json!({"event_type":"step.start", "index":1, "step":{"type":"function_call", "id":"bad", "name":"web"}}),
            json!({"event_type":"step.delta", "index":1, "delta":{"type":"arguments_delta", "arguments":arguments}}),
        ] {
            // What is being written may be shown as it arrives; it is never a call to run.
            let blocks = decoder
                .push(&SseEvent {
                    event: None,
                    data: value.to_string(),
                })
                .unwrap();
            assert!(
                blocks
                    .iter()
                    .all(|block| matches!(block, Block::ToolInput { id, .. } if id == "bad")),
                "{blocks:?}"
            );
        }
        let error = decoder
            .push(&SseEvent {
                event: None,
                data: json!({"event_type":"step.stop", "index":1}).to_string(),
            })
            .unwrap_err();
        assert!(matches!(error, ProviderError::InvalidToolCall(_)));
    }
}

#[test]
fn parallel_function_calls_are_released_in_step_index_order() {
    let mut decoder = ResponseDecoder::new(HashMap::from([
        ("first_wire".into(), "first.tool".into()),
        ("second_wire".into(), "second.tool".into()),
    ]));
    let events = [
        json!({"event_type":"step.start","index":0,"step":{"type":"function_call","id":"c0","name":"first_wire"}}),
        json!({"event_type":"step.start","index":1,"step":{"type":"function_call","id":"c1","name":"second_wire"}}),
        json!({"event_type":"step.delta","index":1,"delta":{"type":"arguments_delta","arguments":"{\"n\":2}"}}),
        json!({"event_type":"step.delta","index":0,"delta":{"type":"arguments_delta","arguments":"{\"n\":1}"}}),
        json!({"event_type":"step.stop","index":1}),
        json!({"event_type":"step.stop","index":0}),
        json!({"event_type":"interaction.requires_action","interaction":{"status":"requires_action"}}),
    ];
    let mut blocks = Vec::new();
    for value in events {
        blocks.extend(
            decoder
                .push(&SseEvent {
                    event: None,
                    data: value.to_string(),
                })
                .unwrap(),
        );
    }
    decoder.finish().unwrap();
    let calls = blocks
        .iter()
        .filter_map(|block| match block {
            Block::ToolIntent(intent) => Some((intent.id.as_str(), intent.tool.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls, vec![("c0", "first.tool"), ("c1", "second.tool")]);
}

#[test]
fn foreign_state_is_ignored_not_consumed() {
    let message = ModelMessage {
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "hello".into(),
            provider_state: vec![ProviderState {
                protocol: Protocol::AnthropicMessages,
                kind: "signature".into(),
                value: json!("foreign"),
            }],
        })],
        trust: None,
    };
    let context = CompiledContext {
        model: String::new(),
        messages: Vec::new(),
        ordered: Some(vec![message]),
        tools: Vec::new(),
    };
    let (body, _) = prepare_body(&context, "m", false, &ReasoningConfig::default(), None).unwrap();
    assert_eq!(body["input"][0]["content"][0]["text"], "hello");
}
