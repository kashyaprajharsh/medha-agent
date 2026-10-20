use super::*;

fn build_chat_messages(
    messages: &[kernel::Message],
    canonical_to_wire: &HashMap<&str, &str>,
) -> Vec<ChatMessage> {
    let ordered: Vec<_> = messages.iter().map(kernel::Message::ordered).collect();
    lower_chat_messages(&ordered, canonical_to_wire, false).expect("plain history lowers to Chat")
}

fn usage_of(body: serde_json::Value) -> kernel::Usage {
    let raw: UsageRaw = serde_json::from_value(body).expect("usage parses");
    match usage_block(raw) {
        Block::Usage(usage) => usage,
        other => panic!("expected a usage block, got {other:?}"),
    }
}

/// Both spellings of the same number. Endpoints differ on which they send,
/// and a harness that reads only one reports no caching on the other.
#[test]
fn a_cache_hit_is_read_under_either_spelling() {
    let openai = usage_of(serde_json::json!({
        "prompt_tokens": 10_000,
        "completion_tokens": 200,
        "total_tokens": 10_200,
        "prompt_tokens_details": { "cached_tokens": 9_600 },
    }));
    assert_eq!(openai.cached_prompt_tokens, Some(9_600));
    assert_eq!(openai.uncached_prompt_tokens(), 400);

    let deepseek = usage_of(serde_json::json!({
        "prompt_tokens": 10_000,
        "completion_tokens": 200,
        "total_tokens": 10_200,
        "prompt_cache_hit_tokens": 9_600,
        "prompt_cache_miss_tokens": 400,
    }));
    assert_eq!(deepseek.cached_prompt_tokens, Some(9_600));
}

/// A proxy routing Claude or Kimi reports re-reads and cache writes under
/// its own names. Missing them reads as "no caching" on a route that caches.
#[test]
fn cache_reads_and_writes_are_read_under_every_spelling() {
    let counts = |extra: serde_json::Value| {
        let mut body = serde_json::json!({ "prompt_tokens": 10_000, "total_tokens": 10_200 });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let usage = usage_of(body);
        (usage.cached_prompt_tokens, usage.cache_write_tokens)
    };
    let nested = serde_json::json!({
        "prompt_tokens_details": { "cached_tokens": 9_000, "cache_write_tokens": 800 },
    });
    assert_eq!(counts(nested), (Some(9_000), Some(800)));
    let passed_through = serde_json::json!({
        "cache_read_input_tokens": 9_000, "cache_creation_input_tokens": 800,
    });
    assert_eq!(counts(passed_through), (Some(9_000), Some(800)));
    let flat = serde_json::json!({ "cached_tokens": 9_000, "cache_write_tokens": 800 });
    assert_eq!(counts(flat), (Some(9_000), Some(800)));
    assert_eq!(counts(serde_json::json!({})), (None, None));
}

/// Silence is not a miss. Every endpoint that reports no bucket would
/// otherwise show a 0% hit rate and look like caching had stopped working.
#[test]
fn an_endpoint_that_reports_no_bucket_reports_nothing() {
    let usage = usage_of(serde_json::json!({
        "prompt_tokens": 10_000,
        "completion_tokens": 200,
        "total_tokens": 10_200,
    }));
    assert_eq!(usage.cached_prompt_tokens, None);
    assert_eq!(usage.cache_hit_ratio(), None);
}

/// A gateway reporting more hits than prompt tokens is reporting something
/// else; the count stays inside the prompt it is a share of.
#[test]
fn a_cache_count_larger_than_the_prompt_is_clamped() {
    let usage = usage_of(serde_json::json!({
        "prompt_tokens": 1_000,
        "completion_tokens": 10,
        "total_tokens": 1_010,
        "prompt_tokens_details": { "cached_tokens": 5_000 },
    }));
    assert_eq!(usage.cached_prompt_tokens, Some(1_000));
    assert_eq!(usage.cache_hit_ratio(), Some(1.0));
}

#[test]
fn incomplete_tool_json_is_rejected_in_both_response_modes() {
    let arguments = r#"{"path":"build_deck.js","content":"unfinished"#;
    let body = serde_json::json!({"choices":[{"message":{"tool_calls":[
        {"id":"write","function":{"name":"fs_write","arguments":arguments}}
    ]}}]})
    .to_string();
    let error = parse_completion(&body, &HashMap::new()).unwrap_err();
    assert!(error.to_string().contains("incomplete JSON arguments"));
    assert!(error.to_string().contains("fs_write"));
    assert!(matches!(error, ProviderError::InvalidToolCall(_)));
    for completed in [false, true] {
        let mut decoder = ResponseDecoder::new(HashMap::new());
        decoder
            .push(&SseEvent {
                event: None,
                data: serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"write","function":{"name":"fs_write","arguments":arguments}}
                ]}}]})
                .to_string(),
            })
            .unwrap();
        if completed {
            decoder
                .push(&SseEvent {
                    event: None,
                    data: "[DONE]".into(),
                })
                .unwrap();
            assert!(matches!(
                decoder.finish(),
                Err(ProviderError::InvalidToolCall(_))
            ));
        } else {
            let error = decoder.finish().unwrap_err();
            assert!(matches!(error, ProviderError::Stream(_)));
            assert!(error.is_retryable());
        }
    }
}

#[test]
fn invalid_parallel_call_never_yields_a_partial_executable_batch() {
    let accum = BTreeMap::from([
        (
            0,
            (
                "a".into(),
                "fs_write".into(),
                r#"{"path":"a","content":"ok"}"#.into(),
            ),
        ),
        (1, ("b".into(), "fs_write".into(), r#"{"path":"b"#.into())),
    ]);
    assert!(finalize_tool_calls(accum, &HashMap::new()).is_err());
    assert!(parse_tool_arguments("fs_write", "a", "[]").is_err());
}

#[test]
fn missing_response_choice_cannot_silently_finish_the_turn() {
    for body in ["{}", r#"{"choices":[],"usage":{"prompt_tokens":10}}"#] {
        assert!(matches!(
            parse_completion(body, &HashMap::new()),
            Err(ProviderError::Stream(_))
        ));
    }
}

#[test]
fn nameless_calls_are_rejected_instead_of_silently_finishing() {
    for function in [
        serde_json::Value::Null,
        serde_json::json!({"arguments":"{}"}),
    ] {
        let body = serde_json::json!({"choices":[{"message":{"tool_calls":[
            {"id":"bad", "function":function}
        ]}}]})
        .to_string();
        assert!(matches!(
            parse_completion(&body, &HashMap::new()),
            Err(ProviderError::InvalidToolCall(_))
        ));
    }
    let accum = BTreeMap::from([(0, ("bad".into(), String::new(), "{}".into()))]);
    assert!(matches!(
        finalize_tool_calls(accum, &HashMap::new()),
        Err(ProviderError::InvalidToolCall(_))
    ));
}

#[test]
fn truncated_or_refused_responses_are_fatal_in_both_response_modes() {
    for (finish, message, reason) in [
        (
            "length",
            serde_json::json!({"content":"partial"}),
            "truncated at its output limit",
        ),
        (
            "stop",
            serde_json::json!({"content":"filler", "refusal":"cannot summarize"}),
            "declined to answer",
        ),
    ] {
        let body = serde_json::json!({"choices":[{"finish_reason":finish, "message":message}]})
            .to_string();
        let error = parse_completion(&body, &HashMap::new()).unwrap_err();
        assert!(error.to_string().contains(reason));
        assert!(!error.is_retryable());
        let mut decoder = ResponseDecoder::new(HashMap::new());
        let error = decoder
            .push(&SseEvent {
                event: None,
                data: serde_json::json!({"choices":[{"finish_reason":finish, "delta":message}]})
                    .to_string(),
            })
            .unwrap_err();
        assert!(error.to_string().contains(reason));
    }
}

#[test]
fn clean_eof_cannot_complete_an_unfinished_response() {
    let mut text_only = ResponseDecoder::new(HashMap::new());
    text_only
        .push(&SseEvent {
            event: None,
            data: r#"{"choices":[{"delta":{"content":"partial handoff"}}]}"#.into(),
        })
        .unwrap();
    assert!(matches!(text_only.finish(), Err(ProviderError::Stream(_))));

    let event = SseEvent {
        event: None,
        data: serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"write","function":{"name":"fs_write","arguments":"{}"}}
        ]}}]})
        .to_string(),
    };
    let mut incomplete = ResponseDecoder::new(HashMap::new());
    incomplete.push(&event).unwrap();
    assert!(matches!(incomplete.finish(), Err(ProviderError::Stream(_))));
    for terminal in [
        r#"{"choices":[{"finish_reason":"tool_calls","delta":{}}]}"#,
        "[DONE]",
    ] {
        let mut complete = ResponseDecoder::new(HashMap::new());
        complete.push(&event).unwrap();
        complete
            .push(&SseEvent {
                event: None,
                data: terminal.into(),
            })
            .unwrap();
        assert!(
            complete
                .finish()
                .unwrap()
                .iter()
                .any(|b| matches!(b, Block::ToolIntent(_)))
        );
    }
}

#[test]
fn in_band_error_preserves_the_structured_context_code() {
    let body = r#"{"error":{"message":"request too large","code":"context_length_exceeded"}}"#;
    let error = parse_completion(body, &HashMap::new()).unwrap_err();
    assert!(error.is_context_overflow());
    let mut decoder = ResponseDecoder::new(HashMap::new());
    assert!(
        decoder
            .push(&SseEvent {
                event: None,
                data: body.into()
            })
            .unwrap_err()
            .is_context_overflow()
    );
}
use kernel::{
    BlastRadius, MediaPart, MediaSource, Message, ProviderState, TextPart, ToolCallPart,
    ToolCategory, ToolIntent, ToolResultPart, ToolSpec,
};

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("call {name}"),
        schema: serde_json::json!({"type": "object"}),
        blast_radius: BlastRadius::Read,
        category: ToolCategory::Read,
        icon: "t".into(),
    }
}

/// Servers that serialise their whole schema (mlx-vlm, some llama.cpp
/// builds) send every field, empties spelled as `null`, and both reasoning
/// spellings at once. Absent-only defaults reject that; a `reasoning` alias
/// would call it a duplicate field and drop the entire turn.
#[test]
fn completion_accepts_null_fields_and_both_reasoning_spellings() {
    let body = r#"{
          "choices": [{"index": 0, "finish_reason": "stop", "message": {
            "role": "assistant",
            "content": "Hello!",
            "reasoning_content": null,
            "reasoning": null,
            "tool_calls": null,
            "tool_call_id": null,
            "name": null
          }, "logprobs": null}],
          "usage": null
        }"#;
    let blocks = parse_completion(body, &HashMap::new()).expect("null fields must parse");
    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, Block::Text(text) if text == "Hello!")),
        "expected the assistant text, got {blocks:?}"
    );
}

#[test]
fn stream_chunk_accepts_null_fields_and_both_reasoning_spellings() {
    let chunk = r#"{"choices":[{"index":0,"finish_reason":null,"delta":{
            "role":"assistant","content":"hi","reasoning_content":null,
            "reasoning":null,"tool_calls":null}}],"usage":null}"#;
    let parsed: StreamChunk = serde_json::from_str(chunk).expect("null fields must parse");
    let delta = &parsed.choices.first().expect("one choice").delta;
    assert_eq!(delta.content.as_deref(), Some("hi"));
    assert!(delta.tool_calls.is_empty());
}

/// `reasoning_content` is canonical, so it wins; a gateway that only sends
/// `reasoning` still has its text surfaced.
#[test]
fn reasoning_content_takes_precedence_and_reasoning_alone_still_reads() {
    let both = r#"{"choices":[{"message":{
            "content":"a","reasoning_content":"canonical","reasoning":"fallback"}}]}"#;
    let blocks = parse_completion(both, &HashMap::new()).expect("parses");
    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, Block::Reasoning(text) if text == "canonical")),
        "reasoning_content should win, got {blocks:?}"
    );

    let only_alias = r#"{"choices":[{"message":{"content":"a","reasoning":"fallback"}}]}"#;
    let blocks = parse_completion(only_alias, &HashMap::new()).expect("parses");
    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, Block::Reasoning(text) if text == "fallback")),
        "reasoning alone should still surface, got {blocks:?}"
    );
}

#[test]
fn collisions_use_the_same_unique_names_in_definitions_and_history() {
    let context = CompiledContext {
        model: String::new(),
        messages: vec![Message::assistant_calls(
            "",
            vec![
                ToolIntent {
                    id: "one".into(),
                    tool: "a.b".into(),
                    args: serde_json::json!({}),
                },
                ToolIntent {
                    id: "two".into(),
                    tool: "a_b".into(),
                    args: serde_json::json!({}),
                },
            ],
        )],
        ordered: None,
        tools: vec![tool("a.b"), tool("a_b")],
    };

    let body = prepare_body(&context, "model", true, None, Some(1_000), false).unwrap();
    let definitions: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    let history: Vec<&str> = body["messages"][0]["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["function"]["name"].as_str().unwrap())
        .collect();

    assert_eq!(definitions.len(), 2);
    assert_ne!(definitions[0], definitions[1]);
    assert_eq!(history, definitions);
}

#[test]
fn request_body_contains_only_portable_chat_fields() {
    let context = CompiledContext {
        model: String::new(),
        messages: vec![Message::user("hello")],
        ordered: None,
        tools: Vec::new(),
    };
    let body = prepare_body(&context, "model", true, Some("high"), Some(2_000), false).unwrap();
    assert_eq!(body["model"], "model");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["max_tokens"], 2_000);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(body.get("chat_template_kwargs").is_none());
}

#[test]
fn build_chat_hoists_and_merges_system_to_the_front() {
    let msgs = vec![
        Message::system("SYS PROMPT"),
        Message::user("do X"),
        Message::system("earlier conversation summary"),
        Message::new(Role::Assistant, "ok"),
        Message::tool_result("c1", "out"),
    ];
    let built = build_chat_messages(&msgs, &HashMap::new());
    assert_eq!(built[0].role, "system");
    assert!(built[0].content.as_str().unwrap().starts_with("SYS PROMPT"));
    assert!(
        built[0]
            .content
            .as_str()
            .unwrap()
            .contains("earlier conversation summary")
    );
    assert_eq!(built.iter().filter(|m| m.role == "system").count(), 1);
    // non-system messages keep their exact order (tool pairing untouched)
    assert_eq!(built[1].role, "user");
    assert_eq!(built[2].role, "assistant");
    assert_eq!(built[3].role, "tool");
}

/// A strict chat template answers 400 to two user messages in a row, which
/// Medha's history holds after feedback or a report. They go as one, in
/// order; a turn from the model between them keeps them apart.
#[test]
fn user_messages_in_a_row_are_sent_as_one() {
    let history = vec![
        Message::system("system"),
        Message::user("first"),
        Message::user("second"),
        Message::user(""),
        Message::assistant_calls("answer", Vec::new()),
        Message::user("third"),
    ];
    let sent = serde_json::to_value(build_chat_messages(&history, &HashMap::new())).unwrap();
    let said: Vec<_> = sent
        .as_array()
        .unwrap()
        .iter()
        .map(|message| (message["role"].as_str().unwrap(), &message["content"]))
        .collect();
    assert_eq!(
        said,
        [
            ("system", &serde_json::json!("system")),
            ("user", &serde_json::json!("first\n\nsecond")),
            ("assistant", &serde_json::json!("answer")),
            ("user", &serde_json::json!("third")),
        ]
    );
}

/// Where a server requires a model's thinking back, every assistant turn
/// carries it: its own, or a single space when it had none. Any other
/// server is sent none of it. A turn that was thinking only goes to neither.
#[test]
fn thinking_goes_back_only_where_a_server_requires_it() {
    use kernel::{ReasoningPart, TextPart};
    let thought = |text: &str| {
        ContentPart::Reasoning(ReasoningPart {
            text: Some(text.into()),
            provider_state: Vec::new(),
        })
    };
    let said = |text: &str| {
        ContentPart::Text(TextPart {
            text: text.into(),
            provider_state: Vec::new(),
        })
    };
    let turn = |role, parts| ModelMessage {
        role,
        parts,
        trust: None,
    };
    let messages = vec![
        turn(Role::User, vec![said("add 2 and 2")]),
        turn(Role::Assistant, vec![thought("weighing it"), said("4")]),
        turn(Role::User, vec![said("and 3 and 3?")]),
        turn(Role::Assistant, vec![said("6")]),
        turn(Role::User, vec![said("and 5 and 5?")]),
        turn(Role::Assistant, vec![thought("five and five make ten")]),
    ];
    let sent = |echo| {
        let lowered = lower_chat_messages(&messages, &HashMap::new(), echo).unwrap();
        serde_json::to_value(lowered).unwrap()
    };

    let required = sent(true);
    assert_eq!(required[1]["reasoning_content"], "weighing it");
    assert_eq!(
        required[3]["reasoning_content"], " ",
        "a blank would be refused"
    );
    let strict = sent(false);
    let strict = strict.as_array().unwrap();
    assert!(strict.iter().all(|m| m.get("reasoning_content").is_none()));

    // The turn that was thinking only goes to neither: no request ends on
    // the model's own turn, which servers refuse.
    for sent in [required.as_array().unwrap(), strict] {
        assert_eq!(sent.len(), 5, "{sent:?}");
        assert_eq!(sent[4]["role"], "user");
    }
}

/// Two turns of the model in a row are refused by the same templates. They
/// go as one: the text on separate lines, and every tool call kept, in order.
#[test]
fn assistant_messages_in_a_row_are_sent_as_one_with_all_their_calls() {
    let call = |id: &str| ToolIntent {
        id: id.into(),
        tool: "read".into(),
        args: serde_json::json!({ "path": id }),
    };
    let history = vec![
        Message::user("go"),
        Message::assistant_calls("Looking.", vec![call("a")]),
        Message::assistant_calls("", vec![call("b")]),
        Message::assistant_calls("Both read.", Vec::new()),
    ];
    let sent = serde_json::to_value(build_chat_messages(&history, &HashMap::new())).unwrap();
    let sent = sent.as_array().unwrap();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[1]["content"], "Looking.\nBoth read.");
    let calls: Vec<_> = sent[1]["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["id"].as_str().unwrap())
        .collect();
    assert_eq!(calls, ["a", "b"]);
}

#[test]
fn ordered_user_image_is_lowered_to_chat_content() {
    let mut messages = vec![ModelMessage {
        role: Role::User,
        parts: vec![
            ContentPart::Text(TextPart {
                text: "what is this?".into(),
                provider_state: Vec::new(),
            }),
            ContentPart::Media(MediaPart {
                mime_type: "image/png".into(),
                source: MediaSource::Base64("QUJD".into()),
                label: None,
                width: None,
                height: None,
                byte_size: None,
                provider_state: Vec::new(),
            }),
        ],
        trust: None,
    }];

    let lowered = lower_chat_messages(&messages, &HashMap::new(), false).unwrap();
    assert_eq!(lowered[0].content[0]["text"], "what is this?");
    assert_eq!(
        lowered[0].content[1]["image_url"]["url"],
        "data:image/png;base64,QUJD"
    );
    if let ContentPart::Media(image) = &mut messages[0].parts[1] {
        image.source = MediaSource::Artifact("sha256:unresolved".into());
    }
    assert!(
        lower_chat_messages(&messages, &HashMap::new(), false)
            .err()
            .expect("unresolved artifact is refused")
            .to_string()
            .contains("must be resolved")
    );
}

#[test]
fn chat_rejects_its_own_replay_state_but_ignores_foreign_state() {
    let state = |protocol| ProviderState {
        protocol,
        kind: "opaque".into(),
        value: serde_json::json!({"secret": "state"}),
    };
    let own = vec![ModelMessage {
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "hello".into(),
            provider_state: vec![state(Protocol::OpenAiChat)],
        })],
        trust: None,
    }];
    let error = lower_chat_messages(&own, &HashMap::new(), false)
        .err()
        .expect("own replay state must be rejected");
    assert!(error.to_string().contains("cannot replay provider state"));

    let foreign = vec![ModelMessage {
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "hello".into(),
            provider_state: vec![state(Protocol::AnthropicMessages)],
        })],
        trust: None,
    }];
    let lowered = lower_chat_messages(&foreign, &HashMap::new(), false).unwrap();
    assert_eq!(lowered[0].content, "hello");
}

#[test]
fn multiple_ordered_tool_results_become_ordered_chat_messages() {
    let messages = vec![ModelMessage {
        role: Role::Tool,
        parts: vec![
            ContentPart::ToolResult(ToolResultPart {
                tool_call_id: "one".into(),
                content: "first".into(),
                provider_state: Vec::new(),
            }),
            ContentPart::ToolResult(ToolResultPart {
                tool_call_id: "two".into(),
                content: "second".into(),
                provider_state: Vec::new(),
            }),
        ],
        trust: None,
    }];

    let lowered = lower_chat_messages(&messages, &HashMap::new(), false).unwrap();
    assert_eq!(lowered.len(), 2);
    assert_eq!(lowered[0].tool_call_id.as_deref(), Some("one"));
    assert_eq!(lowered[1].tool_call_id.as_deref(), Some("two"));
}

#[test]
fn assistant_interleaving_that_chat_cannot_express_fails_closed() {
    let messages = vec![ModelMessage {
        role: Role::Assistant,
        parts: vec![
            ContentPart::ToolCall(ToolCallPart {
                id: "call".into(),
                tool: "fs.read".into(),
                args: serde_json::json!({}),
                provider_state: Vec::new(),
            }),
            ContentPart::Text(TextPart {
                text: "after the call".into(),
                provider_state: Vec::new(),
            }),
        ],
        trust: None,
    }];

    let error = lower_chat_messages(&messages, &HashMap::new(), false)
        .err()
        .expect("unrepresentable interleaving must be rejected");
    assert!(error.to_string().contains("text after a tool call"));
}

mod repair_args_tests {
    use super::repair_args;
    use serde_json::json;

    #[test]
    fn well_formed_args_pass_through_untouched() {
        let args = json!({"path": "src/main.rs", "limit": 5});
        assert_eq!(repair_args(args.clone()), args);
    }

    #[test]
    fn double_encoded_args_are_unwrapped() {
        // The whole object arrived as a JSON string — the exact shape behind
        // the "expected string 'path'" failures.
        let args = json!("{\"path\": \"/w/PROGRESS.md\"}");
        assert_eq!(repair_args(args), json!({"path": "/w/PROGRESS.md"}));
    }

    #[test]
    fn envelope_keys_are_unwrapped_object_or_stringified() {
        assert_eq!(
            repair_args(json!({"arguments": {"path": "a.md"}})),
            json!({"path": "a.md"})
        );
        assert_eq!(
            repair_args(json!({"input": "{\"path\": \"a.md\"}"})),
            json!({"path": "a.md"})
        );
    }

    #[test]
    fn plain_strings_and_real_single_keys_are_not_mangled() {
        // A non-JSON string stays as-is (still fails honestly downstream).
        assert_eq!(repair_args(json!("just text")), json!("just text"));
        // A single key that isn't an envelope name is a real argument.
        assert_eq!(
            repair_args(json!({"path": "arguments.md"})),
            json!({"path": "arguments.md"})
        );
        // An envelope-named key holding a plain string is a real argument too.
        assert_eq!(
            repair_args(json!({"input": "hello"})),
            json!({"input": "hello"})
        );
    }
}

mod sniff_tests {
    use super::sniff_target;

    #[test]
    fn extracts_path_from_partial_args_before_content_finishes() {
        // Path is complete but the huge `content` is still streaming — we still get it.
        assert_eq!(
            sniff_target(r#"{"path":"medha.html","content":"<!doctype html"#),
            Some("medha.html".to_string())
        );
    }

    #[test]
    fn none_until_closing_quote_arrives() {
        assert_eq!(sniff_target(r#"{"file_path": "src/mai"#), None);
        assert_eq!(
            sniff_target(r#"{"file_path":"src/main.rs"}"#),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn command_and_unknown_keys() {
        assert_eq!(
            sniff_target(r#"{"command":"cargo build"}"#),
            Some("cargo build".to_string())
        );
        assert_eq!(sniff_target(r#"{"pattern":"foo"}"#), None);
    }

    #[test]
    fn escaped_quotes_do_not_cut_the_value_short() {
        assert_eq!(
            sniff_target(r#"{"command":"echo \"hello world\" > f.txt"}"#),
            Some(r#"echo "hello world" > f.txt"#.to_string())
        );
        assert_eq!(
            sniff_target(r#"{"path":"dir\\file.rs"}"#),
            Some(r"dir\file.rs".to_string())
        );
        assert_eq!(sniff_target(r#"{"command":"echo \"unfinished"#), None);
    }
}

mod sse_tests {
    use super::*;
    use std::collections::{BTreeMap, HashSet};

    fn process_sse_record(
        record: &str,
        accum: &mut BTreeMap<u32, (String, String, String)>,
        think_filter: &mut ThinkTagFilter,
        target_announced: &mut HashSet<u32>,
    ) -> Result<Vec<Block>, ProviderError> {
        let Some(event) = crate::transport::sse::decode_record(record.as_bytes()) else {
            return Ok(Vec::new());
        };
        process_sse_event(&event, accum, think_filter, target_announced)
    }

    fn drive(record: &str) -> Vec<Block> {
        let mut accum = BTreeMap::new();
        let mut tf = ThinkTagFilter::default();
        let mut announced = HashSet::new();
        let mut blocks = process_sse_record(record, &mut accum, &mut tf, &mut announced).unwrap();
        // The think-tag filter holds a tail back pending a possible `<think>`;
        // flush at end-of-stream (as the real driver does) to emit it.
        if let Some(b) = tf.flush() {
            blocks.push(b);
        }
        blocks
    }

    /// Drive several SSE content records through the real parsing path (record
    /// → delta → think filter), flushing at end-of-stream like the driver does.
    fn drive_many(deltas: &[&str]) -> Vec<Block> {
        let mut accum = BTreeMap::new();
        let mut tf = ThinkTagFilter::default();
        let mut announced = HashSet::new();
        let mut blocks = Vec::new();
        for d in deltas {
            let record = format!(
                "data: {}\n",
                serde_json::json!({"choices":[{"delta":{"content": d}}]})
            );
            blocks
                .extend(process_sse_record(&record, &mut accum, &mut tf, &mut announced).unwrap());
        }
        if let Some(b) = tf.flush() {
            blocks.push(b);
        }
        blocks
    }

    fn join(blocks: &[Block]) -> (String, String) {
        let mut text = String::new();
        let mut reasoning = String::new();
        for b in blocks {
            match b {
                Block::Text(t) => text.push_str(t),
                Block::Reasoning(r) => reasoning.push_str(r),
                _ => {}
            }
        }
        (text, reasoning)
    }

    #[test]
    fn a_tool_calls_arguments_are_passed_on_as_written_under_the_id_the_call_will_have() {
        let mut accum = BTreeMap::new();
        let mut tf = ThinkTagFilter::default();
        let mut announced = HashSet::new();
        let pieces = [
            serde_json::json!({"index": 0, "id": "call_a", "function": {"name": "mcp__draw__view", "arguments": ""}}),
            serde_json::json!({"index": 0, "function": {"arguments": "{\"elements\":\"[{\\\"type\\\":"}}),
            serde_json::json!({"index": 0, "function": {"arguments": "\\\"rect\\\"}]\"}"}}),
            // A second call with no id of its own is known by its place, as its finished call is.
            serde_json::json!({"index": 1, "function": {"name": "mcp__draw__view", "arguments": "{}"}}),
        ];
        let mut written: BTreeMap<String, String> = BTreeMap::new();
        for piece in pieces {
            let record = format!(
                "data: {}\n",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[piece]}}]})
            );
            for block in process_sse_record(&record, &mut accum, &mut tf, &mut announced).unwrap() {
                if let Block::ToolInput { id, name, delta } = block {
                    assert_eq!(name, "mcp__draw__view");
                    assert!(!delta.is_empty(), "an empty piece says nothing");
                    written.entry(id).or_default().push_str(&delta);
                }
            }
        }
        let calls = finalize_tool_calls(accum, &std::collections::HashMap::new()).unwrap();
        assert_eq!(calls.len(), 2);
        for call in calls {
            let pieces: serde_json::Value = serde_json::from_str(&written[&call.id]).unwrap();
            assert_eq!(
                pieces, call.args,
                "the pieces of {} are its arguments",
                call.id
            );
        }
    }

    #[test]
    fn answer_mentioning_think_tags_streams_fully_visible() {
        let full = "Reasoning support (Shape 1: `reasoning_content` field; Shape 2: `<think>` tags)\n- Tool call strategies: `Native`, `Guided` (planned)\n- `models.dev` integration for pricing";
        let deltas = [
            "Reasoning support (Shape 1: `reasoning_content` field; Shape 2: `<th",
            "ink>` tags)\n- Tool call strategies: `Nati",
            "ve`, `Guided` (planned)\n- `models.dev` integration for pricing",
        ];
        let (text, reasoning) = join(&drive_many(&deltas));
        assert_eq!(
            reasoning, "",
            "no part of the answer may become hidden reasoning"
        );
        assert_eq!(text, full, "the full answer must stay visible");
    }

    #[test]
    fn genuine_leading_thinking_still_separates_from_answer() {
        let deltas = ["<think>weigh the", " options</think>", "The answer is 42."];
        let (text, reasoning) = join(&drive_many(&deltas));
        assert_eq!(reasoning, "weigh the options");
        assert_eq!(text, "The answer is 42.");
    }

    #[test]
    fn crlf_record_is_parsed_not_dropped() {
        let blocks = drive("data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\r\n");
        let text: String = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "hello",
            "CRLF record must be parsed, not dropped: {blocks:?}"
        );
    }

    #[test]
    fn missing_tool_call_id_is_synthesized() {
        let mut accum = BTreeMap::new();
        accum.insert(
            0u32,
            (
                String::new(),
                "fs.read".to_string(),
                r#"{"path":"a"}"#.to_string(),
            ),
        );
        let intents = finalize_tool_calls(accum, &std::collections::HashMap::new()).unwrap();
        assert_eq!(intents.len(), 1);
        assert_eq!(
            intents[0].id, "call_0",
            "empty id must be synthesized, not left blank"
        );
        assert_eq!(intents[0].tool, "fs.read");
        assert_eq!(intents[0].args["path"], "a");
    }

    #[test]
    fn present_tool_call_id_is_preserved() {
        let mut accum = BTreeMap::new();
        accum.insert(
            0u32,
            ("real-id".to_string(), "fs.read".to_string(), String::new()),
        );
        let intents = finalize_tool_calls(accum, &std::collections::HashMap::new()).unwrap();
        assert_eq!(intents[0].id, "real-id");
        assert_eq!(
            intents[0].args,
            serde_json::json!({}),
            "empty args → empty object"
        );
    }
}

mod think_tag_tests {
    use super::*;

    fn collect(blocks: &[Block]) -> (String, String) {
        let mut text = String::new();
        let mut reasoning = String::new();
        for b in blocks {
            match b {
                Block::Text(t) => text.push_str(t),
                Block::Reasoning(r) => reasoning.push_str(r),
                _ => {}
            }
        }
        (text, reasoning)
    }

    #[test]
    fn strips_a_leading_think_block() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("<think>hmm let me consider</think> after");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, " after");
        assert_eq!(reasoning, "hmm let me consider");
    }

    #[test]
    fn leading_tag_split_across_chunks_still_matches() {
        let mut f = ThinkTagFilter::default();
        let mut all = Vec::new();
        // Whitespace before the tag is fine; "<think>"/"</think>" split mid-tag.
        all.extend(f.feed("  <thi"));
        all.extend(f.feed("nk>reasoning here</th"));
        all.extend(f.feed("ink> end"));
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "   end");
        assert_eq!(reasoning, "reasoning here");
    }

    #[test]
    fn literal_think_mid_answer_is_content_not_a_tag() {
        let mut f = ThinkTagFilter::default();
        let mut all = Vec::new();
        all.extend(f.feed("Reasoning support (Shape 2: `<think>"));
        all.extend(f.feed("` tags). The rest of the answer must stay visible."));
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(
            reasoning, "",
            "mid-answer literal must not open a think block"
        );
        assert_eq!(
            text,
            "Reasoning support (Shape 2: `<think>` tags). The rest of the answer must stay visible."
        );
    }

    #[test]
    fn unclosed_literal_tag_at_answer_end_is_not_swallowed() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("use <think");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "use <think");
        assert_eq!(reasoning, "");
    }

    #[test]
    fn mid_response_block_at_line_start_is_reasoning() {
        // Chat templates emit think blocks on their own line — one arriving
        // mid-response after a newline is real thinking, not content.
        let mut f = ThinkTagFilter::default();
        let mut all = Vec::new();
        all.extend(f.feed("para one\n\n<think>plan the "));
        all.extend(f.feed("next step</think>\npara two"));
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "para one\n\n\npara two");
        assert_eq!(reasoning, "plan the next step");
    }

    #[test]
    fn literal_tag_pair_mid_line_stays_content() {
        // Both tags mentioned inline (docs/code sample) — nothing is stripped.
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("wrap reasoning in `<think>` and `</think>` in templates");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(
            text,
            "wrap reasoning in `<think>` and `</think>` in templates"
        );
        assert_eq!(reasoning, "");
    }

    #[test]
    fn back_to_back_leading_blocks_both_strip() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("<think>a</think><think>b</think>hello");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "hello");
        assert_eq!(reasoning, "ab");
    }

    #[test]
    fn no_tags_passes_through_as_text() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("just a normal reply, no thinking here");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "just a normal reply, no thinking here");
        assert!(reasoning.is_empty());
    }
}

mod wire_tool_name_tests {
    use super::*;
    use kernel::ToolIntent;

    fn is_strict_valid(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    #[test]
    fn dotted_names_become_strict_valid() {
        for n in ["fs.edit", "shell.exec", "memory.write", "web.fetch"] {
            let wire = wire_tool_name(n);
            assert!(is_strict_valid(&wire), "{n} -> {wire} still invalid");
        }
        assert_eq!(wire_tool_name("fs.edit"), "fs_edit");
        // Already-valid names pass through unchanged.
        assert_eq!(wire_tool_name("read_artifact"), "read_artifact");
    }

    #[test]
    fn map_round_trips_wire_back_to_canonical() {
        let canonical = vec!["fs.edit".to_string(), "shell.exec".to_string()];
        let map = wire_name_map(&canonical);
        assert_eq!(map.get("fs_edit").unwrap(), "fs.edit");
        assert_eq!(map.get("shell_exec").unwrap(), "shell.exec");
    }

    #[test]
    fn colliding_wire_names_stay_distinct() {
        // `fs.edit` and `fs-edit` both sanitize toward `fs_edit`/`fs-edit`;
        // force a real collision with two dot variants.
        let canonical = vec!["a.b".to_string(), "a_b".to_string()];
        let map = wire_name_map(&canonical);
        assert_eq!(map.len(), 2, "collision must not drop a tool");
        let mut targets: Vec<_> = map.values().cloned().collect();
        targets.sort();
        assert_eq!(targets, vec!["a.b", "a_b"]);
    }

    #[test]
    fn finalize_maps_wire_call_back_to_dotted_tool() {
        let mut accum = std::collections::BTreeMap::new();
        accum.insert(
            0u32,
            (
                "id".to_string(),
                "fs_edit".to_string(),
                r#"{"path":"x"}"#.to_string(),
            ),
        );
        let map = wire_name_map(&["fs.edit".to_string()]);
        let intents: Vec<ToolIntent> = finalize_tool_calls(accum, &map).unwrap();
        assert_eq!(
            intents[0].tool, "fs.edit",
            "kernel must see the canonical name"
        );
    }

    #[test]
    fn parse_completion_yields_reasoning_text_tools_and_usage_in_order() {
        // The exact shape the Crystal gateway returns non-streamed: separate
        // reasoning_content, plus a wire-named tool call.
        let body = r#"{
          "choices": [{"message": {
            "content": "The answer is 391.",
            "reasoning_content": "17*23 = 17*(20+3) = 340+51 = 391",
            "tool_calls": [{"id":"c1","function":{"name":"fs_read","arguments":"{\"path\":\"a\"}"}}]
          }}],
          "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
        }"#;
        let map = wire_name_map(&["fs.read".to_string()]);
        let blocks = parse_completion(body, &map).unwrap();
        // Reasoning first, then any text, then the tool intent, then usage last.
        assert!(matches!(&blocks[0], Block::Reasoning(r) if r.contains("391")));
        let text: String = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(text.contains("391"), "text was: {text:?}");
        let intent = blocks
            .iter()
            .find_map(|b| match b {
                Block::ToolIntent(it) => Some(it),
                _ => None,
            })
            .expect("a tool intent");
        assert_eq!(intent.tool, "fs.read", "wire name mapped back to canonical");
        assert_eq!(intent.args["path"], "a");
        assert!(matches!(blocks.last(), Some(Block::Usage(u)) if u.total_tokens == 30));
    }

    #[test]
    fn parse_completion_splits_inline_think_tags_when_no_reasoning_field() {
        let body = r#"{"choices":[{"message":{"content":"<think>weighing it</think>Done."}}]}"#;
        let blocks = parse_completion(body, &std::collections::HashMap::new()).unwrap();
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, Block::Reasoning(r) if r.contains("weighing")))
        );
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, Block::Text(t) if t.contains("Done")))
        );
    }

    #[test]
    fn parse_completion_surfaces_an_error_body() {
        let body = r#"{"error":{"message":"maximum context length is 8192 tokens","type":"context_length_exceeded"}}"#;
        let error = parse_completion(body, &std::collections::HashMap::new()).unwrap_err();
        assert!(matches!(error, ProviderError::Response(_)));
        assert!(error.is_context_overflow());
        assert!(!error.is_retryable());
    }

    #[test]
    fn assistant_history_tool_calls_are_sanitized() {
        let intent = ToolIntent {
            id: "c1".into(),
            tool: "fs.edit".into(),
            args: serde_json::json!({}),
        };
        let msgs = vec![Message::assistant_calls("", vec![intent])];
        let built = build_chat_messages(&msgs, &HashMap::new());
        assert_eq!(
            built[0].tool_calls[0].function.name, "fs_edit",
            "history name must be wire-valid"
        );
    }
}

#[test]
fn endpoint_accepts_api_root_or_full_chat_url() {
    assert_eq!(
        endpoint_url("https://integrate.api.nvidia.com/v1", "/chat/completions").unwrap(),
        "https://integrate.api.nvidia.com/v1/chat/completions"
    );
    assert_eq!(
        endpoint_url(
            "https://integrate.api.nvidia.com/v1/chat/completions",
            "/chat/completions",
        )
        .unwrap(),
        "https://integrate.api.nvidia.com/v1/chat/completions"
    );
    assert_eq!(
        endpoint_url(
            "https://integrate.api.nvidia.com/v1/chat/completions",
            "/models",
        )
        .unwrap(),
        "https://integrate.api.nvidia.com/v1/models"
    );
}
