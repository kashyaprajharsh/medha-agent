use super::*;
use kernel::{Message, Role};

mod count_tokens_tests {
    use super::*;
    use kernel::{BlastRadius, ToolCategory, ToolSpec};

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "read a file".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            blast_radius: BlastRadius::Read,
            category: ToolCategory::Read,
            icon: "r".into(),
        }
    }

    #[test]
    fn tokenize_url_is_server_root_sibling_of_v1() {
        assert_eq!(
            vllm_tokenize_url("https://h.example/v1"),
            "https://h.example/tokenize"
        );
        assert_eq!(
            vllm_tokenize_url("https://h.example/v1/"),
            "https://h.example/tokenize"
        );
        // No /v1 suffix → append at the given root.
        assert_eq!(
            vllm_tokenize_url("https://h.example"),
            "https://h.example/tokenize"
        );
        assert_eq!(
            vllm_tokenize_url("https://h.example/v1/chat/completions"),
            "https://h.example/tokenize"
        );
    }

    #[test]
    fn openai_endpoint_accepts_api_root_or_full_chat_url() {
        assert_eq!(
            openai_endpoint_url("https://integrate.api.nvidia.com/v1", "/chat/completions")
                .unwrap(),
            "https://integrate.api.nvidia.com/v1/chat/completions"
        );
        assert_eq!(
            openai_endpoint_url(
                "https://integrate.api.nvidia.com/v1/chat/completions",
                "/chat/completions",
            )
            .unwrap(),
            "https://integrate.api.nvidia.com/v1/chat/completions"
        );
        assert_eq!(
            openai_endpoint_url(
                "https://integrate.api.nvidia.com/v1/chat/completions",
                "/models",
            )
            .unwrap(),
            "https://integrate.api.nvidia.com/v1/models"
        );
    }

    #[test]
    fn vllm_body_is_derived_from_the_complete_prepared_chat_request() {
        let msgs = vec![
            Message::system("s"),
            Message::user("u"),
            Message::tool_result("c1", "t"),
        ];
        let provider = OpenAiCompat::new("http://x/v1", "", "m");
        let prepared = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: msgs,
                ordered: None,
                tools: vec![tool("fs.read")],
            })
            .unwrap();
        let body = vllm_tokenize_body_from_prepared(&prepared);
        assert_eq!(body["add_generation_prompt"], serde_json::json!(true));
        assert!(body.get("stream").is_none());
        assert_eq!(body["tools"][0]["function"]["name"], "fs_read");
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[2]["role"], "tool");
    }

    #[test]
    fn vllm_body_hoists_a_mid_array_system_like_the_chat_path() {
        let msgs = vec![
            Message::system("SYS"),
            Message::user("u"),
            Message::system("earlier summary"), // mid-array (compaction)
            Message::new(Role::Assistant, "ok"),
        ];
        let provider = OpenAiCompat::new("http://x/v1", "", "m");
        let prepared = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: msgs,
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        let body = vllm_tokenize_body_from_prepared(&prepared);
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[0]["content"], "SYS\n\nearlier summary");
        assert_eq!(arr[1]["role"], "user");
        assert_eq!(arr[2]["role"], "assistant");
    }

    #[test]
    fn generic_openai_chat_does_not_probe_a_vendor_count_route() {
        let provider = OpenAiCompat::new("http://127.0.0.1:1/v1", "", "m");
        let prepared = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::user("hello")],
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        let result = futures::executor::block_on(provider.count_input_tokens(&prepared)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn fingerprint_changes_when_the_prepared_tool_schema_changes() {
        let provider = OpenAiCompat::new("http://x/v1", "", "m");
        let make = |tools| {
            provider
                .prepare_request(&CompiledContext {
                    model: String::new(),
                    messages: vec![Message::user("hello")],
                    ordered: None,
                    tools,
                })
                .unwrap()
        };
        let without = make(Vec::new());
        let with = make(vec![tool("fs.read")]);
        assert_ne!(without.request_fingerprint, with.request_fingerprint);
    }

    /// Serve `body` with `content_type` to one streaming request and return the
    /// blocks the provider produced from it.
    async fn answer_streaming_request(
        content_type: &'static str,
        body: &'static str,
    ) -> Result<Vec<Block>, ProviderError> {
        use futures::StreamExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 8192];
            let _ = socket.read(&mut buffer).await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let provider = ProviderClient::new(format!("http://{address}/v1"), "key", "m");
        assert!(provider.streaming(), "the request must ask for a stream");
        let context = CompiledContext {
            model: "m".into(),
            messages: vec![Message::user("hello")],
            ordered: None,
            tools: Vec::new(),
        };
        let mut stream = provider.stream(&context).await?;
        let mut blocks = Vec::new();
        while let Some(block) = stream.next().await {
            blocks.push(block?);
        }
        Ok(blocks)
    }

    /// Text arrives in whatever chunks the decoder emits; the reply is their sum.
    fn spoken(blocks: &[Block]) -> String {
        blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Streaming must not wait for the last byte to show the first word — not
    /// even when the endpoint mislabels its content type. Deciding how to read
    /// a response from its header did exactly that, turning every unlabelled
    /// stream into a full buffer before anything appeared.
    #[tokio::test]
    async fn the_first_token_does_not_wait_for_the_last() {
        use futures::StreamExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 8192];
            let _ = socket.read(&mut buffer).await.unwrap();
            // Deliberately not text/event-stream: plenty of gateways are.
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                      transfer-encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            let first = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello there, this is the first chunk.\"},\"finish_reason\":null}]}\n\n";
            socket
                .write_all(format!("{:X}\r\n{first}\r\n", first.len()).as_bytes())
                .await
                .unwrap();
            // The rest of the answer arrives much later.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let last = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";
            socket
                .write_all(format!("{:X}\r\n{last}\r\n0\r\n\r\n", last.len()).as_bytes())
                .await
                .unwrap();
        });

        let provider = ProviderClient::new(format!("http://{address}/v1"), "key", "m");
        let context = CompiledContext {
            model: "m".into(),
            messages: vec![Message::user("hi")],
            ordered: None,
            tools: Vec::new(),
        };
        let mut stream = provider.stream(&context).await.unwrap();

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .expect("the first token must not wait for the rest of the answer")
            .expect("a block")
            .unwrap();
        assert!(
            matches!(&first, Block::Text(text) if text.starts_with("Hello there")),
            "{first:?}"
        );
    }

    /// A deployment that ignores `stream: true` and answers with one ordinary
    /// completion must still produce the reply. Decoding that as an event
    /// stream yields nothing, which reaches the user as a turn that printed
    /// no answer and reported no error.
    #[tokio::test]
    async fn a_completion_answering_a_streaming_request_is_still_delivered() {
        let blocks = answer_streaming_request(
            "application/json",
            r#"{"choices":[{"message":{"role":"assistant","content":"hello there"},"finish_reason":"stop"}]}"#,
        )
        .await
        .unwrap();

        assert_eq!(spoken(&blocks), "hello there", "{blocks:?}");
    }

    /// The same body correctly streamed but mislabelled as JSON still works:
    /// the completion reading fails and the event-stream reading takes over.
    #[tokio::test]
    async fn a_mislabelled_event_stream_is_still_decoded() {
        let blocks = answer_streaming_request(
            "application/json",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
        )
        .await
        .unwrap();

        assert_eq!(spoken(&blocks), "hi", "{blocks:?}");
    }

    /// A body that is neither reports the decode failure rather than ending the
    /// turn with nothing.
    #[tokio::test]
    async fn an_undecodable_body_fails_loudly() {
        let error = answer_streaming_request("application/json", "<html>gateway error</html>")
            .await
            .expect_err("silence is not an acceptable outcome")
            .to_string();
        assert!(error.contains("parse"), "{error}");
    }

    #[tokio::test]
    async fn declared_vllm_counter_posts_the_full_prepared_input() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::sync::oneshot;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before headers");
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            assert!(headers.starts_with("POST /tokenize HTTP/1.1"), "{headers}");
            let content_length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())
                        .flatten()
                })
                .expect("content-length");
            while bytes.len() - header_end < content_length {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before body");
                bytes.extend_from_slice(&chunk[..read]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
            tx.send(body).unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 13\r\nconnection: close\r\n\r\n{\"count\":321}",
                )
                .await
                .unwrap();
        });

        let provider = OpenAiCompat::new(format!("http://{address}/v1"), "", "m")
            .with_token_counter(OpenAiTokenCounter::Vllm);
        let prepared = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::system("s"), Message::user("hello")],
                ordered: None,
                tools: vec![tool("fs.read")],
            })
            .unwrap();
        let count = provider
            .count_input_tokens(&prepared)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(count.tokens, 321);
        assert_eq!(count.quality, TokenCountQuality::Authoritative);
        assert_eq!(count.request_fingerprint, prepared.request_fingerprint);
        let body = rx.await.unwrap();
        assert_eq!(body["tools"][0]["function"]["name"], "fs_read");
        assert_eq!(body["add_generation_prompt"], true);
        assert!(body.get("stream").is_none());
        server.await.unwrap();
    }
}

mod gemini_client_tests {
    use super::*;
    use kernel::{BlastRadius, ToolCategory, ToolSpec};
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn native_model_discovery_uses_v1_google_auth_and_normalizes_ids() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before headers");
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&bytes);
            assert!(
                headers.starts_with("GET /v1/models?pageSize=1000 HTTP/1.1"),
                "{headers}"
            );
            assert!(
                headers
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("x-goog-api-key: discovery-secret")),
                "{headers}"
            );
            assert!(
                headers
                    .lines()
                    .any(|line| line
                        .eq_ignore_ascii_case(&format!("user-agent: {}", http::USER_AGENT))),
                "{headers}"
            );
            let body = serde_json::json!({
                "models": [
                    {
                        "name": "models/gemini-3.5-flash",
                        "baseModelId": "gemini-3.5-flash",
                        "inputTokenLimit": 1_048_576,
                        "supportedGenerationMethods": ["generateContent", "countTokens"]
                    },
                    {
                        "name": "models/text-embedding-004",
                        "baseModelId": "text-embedding-004",
                        "inputTokenLimit": 2_048,
                        "supportedGenerationMethods": ["embedContent"]
                    }
                ]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut profile = ProviderProfile::openai_chat(
            format!("http://{address}/v1"),
            "model-discovery",
            AuthKind::XGoogApiKey,
        );
        profile.protocol = Protocol::GeminiInteractions;
        let models = list_models_for_profile(&profile, "discovery-secret")
            .await
            .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.5-flash");
        assert_eq!(models[0].context_length, Some(1_048_576));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn openai_chat_discovery_handles_groq_dual_context_keys() {
        // Groq reports BOTH `context_length` and `context_window` on each model.
        // Aliasing them to one field made serde raise "duplicate field" and the
        // whole discovery failed with an opaque decode error. It must now parse,
        // preferring `context_length`.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before headers");
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            assert!(String::from_utf8_lossy(&bytes).lines().any(|line| {
                line.eq_ignore_ascii_case(&format!("user-agent: {}", http::USER_AGENT))
            }));
            let body = serde_json::json!({
                "object": "list",
                "data": [
                    {
                        "id": "openai/gpt-oss-120b",
                        "object": "model",
                        "owned_by": "OpenAI",
                        "active": true,
                        "context_window": 131_072,
                        "context_length": 131_072,
                        "max_completion_tokens": 8_192
                    }
                ]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let models = list_models(&format!("http://{address}/openai/v1"), "gsk_test")
            .await
            .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "openai/gpt-oss-120b");
        assert_eq!(models[0].context_length, Some(131_072));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_stream_posts_v1_interactions_with_google_auth_and_decodes_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before headers");
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
            assert!(
                headers.starts_with("POST /v1/interactions HTTP/1.1"),
                "{headers}"
            );
            assert!(
                headers
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("x-goog-api-key: test-secret")),
                "{headers}"
            );
            assert!(
                !headers
                    .lines()
                    .any(|line| line.to_ascii_lowercase().starts_with("authorization:")),
                "{headers}"
            );
            let content_length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())
                        .flatten()
                })
                .expect("content-length");
            while bytes.len() - header_end < content_length {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before body");
                bytes.extend_from_slice(&chunk[..read]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
            tx.send(body).unwrap();

            let events = concat!(
                "event: step.start\n",
                "data: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"thought\",\"summary\":[]}}\n\n",
                "event: step.delta\n",
                "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"thought_summary\",\"content\":{\"type\":\"text\",\"text\":\"checking\"}}}\n\n",
                "event: step.delta\n",
                "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"thought_signature\",\"signature\":\"opaque-signed-state\"}}\n\n",
                "event: step.stop\n",
                "data: {\"event_type\":\"step.stop\",\"index\":0}\n\n",
                "event: step.start\n",
                "data: {\"event_type\":\"step.start\",\"index\":1,\"step\":{\"type\":\"model_output\",\"content\":[]}}\n\n",
                "event: step.delta\n",
                "data: {\"event_type\":\"step.delta\",\"index\":1,\"delta\":{\"type\":\"text\",\"text\":\"hello\"}}\n\n",
                "event: step.stop\n",
                "data: {\"event_type\":\"step.stop\",\"index\":1}\n\n",
                "event: interaction.completed\n",
                "data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"status\":\"completed\",\"usage\":{\"total_input_tokens\":3,\"total_output_tokens\":4,\"total_thought_tokens\":2,\"total_tokens\":9}}}\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                events.len(),
                events
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut profile = ProviderProfile::openai_chat(
            format!("http://{address}/v1"),
            "gemini-3.6-flash",
            AuthKind::XGoogApiKey,
        );
        profile.protocol = Protocol::GeminiInteractions;
        profile.reasoning = ReasoningSupport::Effort;
        let provider = ProviderClient::from_profile(profile, "test-secret")
            .unwrap()
            .with_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Minimal),
            })
            .unwrap();
        let request = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::user("hello")],
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        assert_eq!(request.protocol, Protocol::GeminiInteractions);
        assert_eq!(request.body["store"], false);
        assert_eq!(
            request.body["generation_config"]["thinking_level"],
            "minimal"
        );

        let blocks: Vec<Block> = provider
            .stream_prepared(&request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::Text(text) if text == "hello"))
        );
        let completed = blocks
            .iter()
            .find_map(|block| match block {
                Block::CompletedMessage(message) => Some(message),
                _ => None,
            })
            .expect("canonical completed message");
        assert!(completed.parts.iter().any(|part| matches!(
            part,
            kernel::ContentPart::Reasoning(reasoning)
                if reasoning.provider_state.iter().any(|state| {
                    state.protocol == Protocol::GeminiInteractions
                        && state.value == serde_json::json!("opaque-signed-state")
                })
        )));
        assert!(
            matches!(blocks.last(), Some(Block::Usage(usage)) if usage.total_tokens == 9 && usage.completion_tokens == 6)
        );

        let body = rx.await.unwrap();
        assert_eq!(body["model"], "gemini-3.6-flash");
        assert_eq!(body["store"], false);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_http_reports_usage_and_refuses_incomplete_calls_in_both_modes() {
        // Synthetic v1 responses: a syntactically valid call is executable only
        // after a successful terminal status, regardless of response mode.
        for streaming in [false, true] {
            for status in ["requires_action", "incomplete"] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let steps = vec![
                    json!({"type":"model_output","content":[{"type":"text","text":"checking"}]}),
                    json!({"type":"function_call","id":"c1","name":"fs_read","arguments":{"path":"dummy.txt"}}),
                ];
                let usage = json!({"total_input_tokens":21576,"total_cached_tokens":16356,
                    "total_output_tokens":1,"total_thought_tokens":56,"total_tokens":21633});
                let (kind, response) = if streaming {
                    let mut events = String::new();
                    for (index, step) in steps.iter().enumerate() {
                        for event in [
                            json!({"event_type":"step.start","index":index,"step":step}),
                            json!({"event_type":"step.stop","index":index}),
                        ] {
                            events.push_str(&format!("data: {event}\n\n"));
                        }
                    }
                    let terminal = json!({"event_type":"interaction.completed",
                        "interaction":{"status":status,"usage":usage}});
                    events.push_str(&format!("data: {terminal}\n\n"));
                    ("text/event-stream", events)
                } else {
                    (
                        "application/json",
                        json!({"status":status,"steps":steps,"usage":usage}).to_string(),
                    )
                };
                let served = async {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let header_end = loop {
                        let read = socket.read(&mut chunk).await.unwrap();
                        assert!(read > 0, "client closed before headers");
                        bytes.extend_from_slice(&chunk[..read]);
                        assert!(bytes.len() <= 32768, "fixture request exceeded its bound");
                        if let Some(position) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            break position + 4;
                        }
                    };
                    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
                    assert!(headers.starts_with("POST /v1/interactions HTTP/1.1"));
                    assert!(
                        headers
                            .lines()
                            .any(|line| line.eq_ignore_ascii_case("x-goog-api-key: dummy-key"))
                    );
                    assert!(headers.lines().any(|line| {
                        line.eq_ignore_ascii_case(&format!("user-agent: {}", http::USER_AGENT))
                    }));
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().ok())
                                .flatten()
                        })
                        .expect("request length");
                    assert!(header_end + length <= 32768);
                    while bytes.len() < header_end + length {
                        let read = socket.read(&mut chunk).await.unwrap();
                        assert!(read > 0, "client closed before body");
                        bytes.extend_from_slice(&chunk[..read]);
                        assert!(bytes.len() <= 32768);
                    }
                    let body: Value =
                        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                    assert_eq!(body["stream"], streaming);
                    assert_eq!(body["model"], "gemini-3.8-flash");
                    assert_eq!(body["generation_config"]["thinking_level"], "high");
                    assert_eq!(body["tools"][0]["name"], "fs_read");
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                        response.len()
                    );
                    socket.write_all(reply.as_bytes()).await.unwrap();
                };

                let mut profile = ProviderProfile::openai_chat(
                    format!("http://{address}/v1"),
                    "gemini-3.8-flash",
                    AuthKind::XGoogApiKey,
                );
                profile.protocol = Protocol::GeminiInteractions;
                let provider = ProviderClient::from_profile(profile, "dummy-key")
                    .unwrap()
                    .with_reasoning(ReasoningConfig {
                        enabled: Some(true),
                        effort: Some(ReasoningEffort::High),
                    })
                    .unwrap();
                provider.set_streaming(streaming);
                let request = provider.prepare_request(&CompiledContext {
                    model: String::new(), messages: vec![Message::user("synthetic request")], ordered: None,
                    tools: vec![ToolSpec { name: "fs.read".into(), description: "synthetic read".into(),
                        schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
                        blast_radius: BlastRadius::Read, category: ToolCategory::Read, icon: "r".into() }],
                }).unwrap();
                let received = async {
                    match provider.stream_prepared(&request).await {
                        Ok(stream) => stream.collect::<Vec<_>>().await,
                        Err(error) => vec![Err(error)],
                    }
                };
                // No detached server tasks remain if the request or assertion fails.
                let (blocks, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    tokio::join!(received, served)
                })
                .await
                .expect("local interaction timed out");
                if status == "incomplete" {
                    assert!(
                        blocks
                            .iter()
                            .any(|block| matches!(block, Err(ProviderError::Truncated)))
                    );
                    assert!(blocks.iter().all(|block| !matches!(
                        block,
                        Ok(Block::ToolIntent(_) | Block::CompletedMessage(_))
                    )));
                    assert!(
                        blocks
                            .iter()
                            .filter_map(|block| block.as_ref().err())
                            .all(|error| !error.is_retryable())
                    );
                } else {
                    let blocks: Vec<_> = blocks.into_iter().collect::<Result<_, _>>().unwrap();
                    assert!(
                        blocks
                            .iter()
                            .any(|block| matches!(block, Block::ToolIntent(call)
                        if call.tool == "fs.read" && call.args["path"] == "dummy.txt"))
                    );
                    assert_eq!(
                        blocks
                            .iter()
                            .filter(|block| matches!(block, Block::Usage(_)))
                            .count(),
                        1
                    );
                    let Some(Block::Usage(usage)) = blocks.last() else {
                        panic!("usage missing")
                    };
                    assert_eq!(usage.prompt_tokens, 21576);
                    assert_eq!(usage.completion_tokens, 57);
                    assert_eq!(usage.total_tokens, 21633);
                    assert_eq!(usage.cached_prompt_tokens, Some(16356));
                }
            }
        }
    }

    #[test]
    fn gemini_profile_switch_is_atomic_and_output_retry_rewrites_generation_config() {
        let provider = OpenAiCompat::new("http://chat.test/v1", "", "chat-model");
        let mut profile = ProviderProfile::openai_chat(
            "https://generativelanguage.googleapis.com/v1",
            "gemini-model",
            AuthKind::XGoogApiKey,
        );
        profile.protocol = Protocol::GeminiInteractions;
        provider.switch_provider_profile(profile, "secret").unwrap();
        assert_eq!(provider.protocol(), Protocol::GeminiInteractions);
        assert_eq!(provider.active_model(), "gemini-model");

        let request = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::user("hello")],
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        let retried = provider.with_output_limit(&request, 512).unwrap().unwrap();
        assert_eq!(retried.body["generation_config"]["max_output_tokens"], 512);
        assert_ne!(request.request_fingerprint, retried.request_fingerprint);
    }
}

mod reasoning_request_tests {
    use super::*;

    fn gemini_profile(model: &str) -> ProviderProfile {
        let mut profile =
            ProviderProfile::openai_chat("http://example.test/v1", model, AuthKind::None);
        profile.protocol = Protocol::GeminiInteractions;
        profile
    }

    #[test]
    fn gemini_efforts_are_model_specific_and_explicit_overrides_win() {
        let flash = ProviderClient::from_profile(gemini_profile("gemini-3.8-flash"), "").unwrap();
        assert_eq!(
            flash.reasoning_efforts(),
            [
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High
            ]
        );
        assert!(
            flash
                .set_reasoning(ReasoningConfig::from_effort_text("minimal").unwrap())
                .is_err()
        );
        for model in ["gemini-3.6-flash", "gemini-3.5-flash-lite"] {
            let provider = ProviderClient::from_profile(gemini_profile(model), "").unwrap();
            assert!(
                provider
                    .reasoning_efforts()
                    .contains(&ReasoningEffort::Minimal)
            );
            provider
                .set_reasoning(ReasoningConfig::from_effort_text("minimal").unwrap())
                .unwrap();
        }
        let mut custom = gemini_profile("custom-model");
        custom.reasoning_efforts = Some(vec![ReasoningEffort::Minimal, ReasoningEffort::High]);
        let provider = ProviderClient::from_profile(custom, "").unwrap();
        assert_eq!(
            provider.reasoning_efforts(),
            [ReasoningEffort::Minimal, ReasoningEffort::High]
        );
        provider
            .set_reasoning(ReasoningConfig::from_effort_text("minimal").unwrap())
            .unwrap();
        let unknown = ProviderClient::from_profile(gemini_profile("unknown-model"), "").unwrap();
        assert!(
            unknown.reasoning_efforts().is_empty(),
            "unknown Gemini levels stay unverified; use the server default or an explicit profile"
        );
    }

    #[test]
    fn switching_from_minimal_to_gemini_flash_uses_server_default() {
        let provider =
            ProviderClient::from_profile(gemini_profile("gemini-3.6-flash"), "").unwrap();
        provider
            .set_reasoning(ReasoningConfig::from_effort_text("minimal").unwrap())
            .unwrap();
        provider
            .switch_provider_profile(gemini_profile("gemini-3.8-flash"), "")
            .unwrap();
        assert_eq!(provider.reasoning(), ReasoningConfig::default());
        let request = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::user("hi")],
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        assert!(request.body.get("generation_config").is_none());
        provider
            .set_reasoning(ReasoningConfig::from_effort_text("medium").unwrap())
            .unwrap();
        assert_eq!(
            provider.prepare_request(&request.context).unwrap().body["generation_config"]["thinking_level"],
            "medium"
        );
    }

    #[test]
    fn reading_model_limits_never_relocks_the_connection() {
        let provider = std::sync::Arc::new(
            OpenAiCompat::new("http://x", "", "m")
                .with_max_ctx(32_768)
                .with_max_output_tokens(4_096),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(provider.model_limits()).unwrap());

        let limits = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("model_limits deadlocked while reading the connection");
        assert_eq!(limits.max_combined_tokens, Some(32_768));
        assert_eq!(limits.max_output_tokens, None);
        assert_eq!(limits.input_allowance(Some(4_096)), Some(28_672));
    }

    #[test]
    fn server_default_output_keeps_context_protection_without_inventing_a_wire_cap() {
        let provider = OpenAiCompat::new("http://x", "", "m").with_max_ctx(128_000);
        assert_eq!(
            provider
                .model_limits()
                .input_allowance(provider.requested_output_tokens()),
            Some(128_000)
        );
        let request = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::user("hello")],
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        assert!(request.body.get("max_tokens").is_none());
        assert!(request.body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn effort_uses_the_portable_top_level_openai_field() {
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::High),
            })
            .unwrap();
        assert_eq!(p.reasoning_effort_value(), Some("high"));
    }

    #[test]
    fn minimal_is_not_silently_raised_on_the_compatible_route() {
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Minimal),
            })
            .unwrap();
        assert_eq!(p.reasoning_effort_value(), Some("minimal"));
    }

    #[test]
    fn explicit_off_sends_none_not_a_silent_omit() {
        // Compatible servers disable thinking via reasoning_effort:"none".
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_reasoning(ReasoningConfig {
                enabled: Some(false),
                effort: None,
            })
            .unwrap();
        assert_eq!(p.reasoning_effort_value(), Some("none"));
    }

    #[test]
    fn profile_switch_resets_carried_effort_for_an_unsupported_model() {
        let provider = OpenAiCompat::new("http://one", "", "model")
            .with_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Low),
            })
            .unwrap();
        let mut next = ProviderProfile::openai_chat("http://two", "other-model", AuthKind::None);
        next.reasoning = ReasoningSupport::Unsupported;

        provider.switch_provider_profile(next, "").unwrap();
        assert_eq!(provider.active_model(), "other-model");
        assert_eq!(provider.reasoning(), ReasoningConfig::default());
    }

    #[test]
    fn gemini_reasoning_reconciles_on_switch_to_openai() {
        // A Gemini session runs with "reasoning on" and no explicit effort —
        // valid for gemini-interactions. Start a provider in exactly that state.
        let mut gemini = ProviderProfile::openai_chat(
            "http://gemini/v1",
            "gemini-3.5-flash",
            AuthKind::XGoogApiKey,
        );
        gemini.protocol = Protocol::GeminiInteractions;
        let provider = ProviderClient::from_profile(gemini, "key").unwrap();
        provider
            .set_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: None,
            })
            .unwrap();
        assert_eq!(provider.reasoning().effort, None);

        // open-ai-chat can't enable reasoning without a concrete effort. The
        // switch must NOT fail: the carried "on, no effort" is reconciled to
        // untouched (server default), never a fabricated level.
        let next = ProviderProfile::openai_chat("http://vllm/v1", "qwen", AuthKind::None);
        provider
            .switch_provider_profile(next, "")
            .expect("switch reconciles instead of erroring");
        assert_eq!(provider.active_model(), "qwen");
        assert_eq!(provider.reasoning(), ReasoningConfig::default());
    }

    #[test]
    fn only_explicit_first_run_constructor_allows_an_empty_profile() {
        let invalid = ProviderProfile::openai_chat("", "", AuthKind::None);
        assert!(ProviderClient::from_profile(invalid, "").is_err());

        let provider = ProviderClient::unconfigured();
        let connection = provider.connection.lock().unwrap();
        assert!(connection.profile.base_url.is_empty());
        assert!(connection.profile.model.is_empty());
    }

    #[test]
    fn observed_image_support_is_scoped_to_the_exact_profile() {
        let first = ProviderProfile::openai_chat("http://one/v1", "model-a", AuthKind::None);
        let provider = ProviderClient::from_profile(first.clone(), "").unwrap();
        assert_eq!(provider.image_support(), ImageSupport::Unknown);
        provider.observe_image_support(ImageSupport::Unsupported);
        assert_eq!(provider.image_support(), ImageSupport::Unsupported);

        let second = ProviderProfile::openai_chat("http://one/v1", "model-b", AuthKind::None);
        provider.switch_provider_profile(second, "").unwrap();
        assert_eq!(provider.image_support(), ImageSupport::Unknown);

        provider.switch_provider_profile(first, "").unwrap();
        assert_eq!(provider.image_support(), ImageSupport::Unsupported);
    }

    /// A catalogue describes the model; the server's refusal describes this
    /// deployment, and wins. The operator's own setting stays final.
    #[test]
    fn what_the_server_showed_outranks_the_catalogue_but_not_the_operator() {
        let mut listed = ProviderProfile::openai_chat("http://one/v1", "vision", AuthKind::None);
        listed.capabilities = Some(crate::ModelCapabilities {
            modalities: Some(crate::ModalitySet {
                input: Some(["text".into(), "image".into()].into_iter().collect()),
                output: Some(["text".into()].into_iter().collect()),
            }),
            ..crate::ModelCapabilities::default()
        });
        let provider = ProviderClient::from_profile(listed.clone(), "").unwrap();
        assert_eq!(provider.image_support(), ImageSupport::Supported);
        provider.observe_image_support(ImageSupport::Unsupported);
        assert_eq!(provider.image_support(), ImageSupport::Unsupported);

        let mut forced = listed;
        forced.image_input = ImageInputMode::Native;
        let provider = ProviderClient::from_profile(forced, "").unwrap();
        provider.observe_image_support(ImageSupport::Unsupported);
        assert_eq!(provider.image_support(), ImageSupport::Supported);
    }

    #[test]
    fn explicit_image_mode_overrides_catalog_metadata() {
        let mut profile = ProviderProfile::openai_chat("http://one/v1", "model", AuthKind::None);
        profile.capabilities = Some(crate::ModelCapabilities {
            modalities: Some(crate::ModalitySet {
                input: Some(["text".into()].into_iter().collect()),
                output: Some(["text".into()].into_iter().collect()),
            }),
            attachment: Some(false),
            tool_calls: None,
            reasoning: None,
            ..crate::ModelCapabilities::default()
        });
        profile.image_input = ImageInputMode::Native;
        let provider = ProviderClient::from_profile(profile.clone(), "").unwrap();
        assert_eq!(provider.image_support(), ImageSupport::Supported);

        profile.image_input = ImageInputMode::Text;
        provider.switch_provider_profile(profile, "").unwrap();
        assert_eq!(provider.image_support(), ImageSupport::Unsupported);
    }

    #[test]
    fn every_declared_effort_reaches_the_wire_without_substitution() {
        let mut profile =
            ProviderProfile::openai_chat("http://example.test/v1", "custom", AuthKind::None);
        profile.reasoning_efforts = Some(ReasoningEffort::ALL.to_vec());
        let provider = ProviderClient::from_profile(profile, "").unwrap();
        let context = CompiledContext {
            model: String::new(),
            messages: vec![kernel::Message::user("hi")],
            ordered: None,
            tools: Vec::new(),
        };
        for effort in ReasoningEffort::ALL {
            provider
                .set_reasoning(ReasoningConfig::from_effort_text(effort.as_str()).unwrap())
                .unwrap();
            let body = provider.prepare_request(&context).unwrap().body;
            assert_eq!(body["reasoning_effort"], effort.as_str());
        }
        provider
            .set_reasoning(ReasoningConfig::from_effort_text("auto").unwrap())
            .unwrap();
        assert!(
            provider
                .prepare_request(&context)
                .unwrap()
                .body
                .get("reasoning_effort")
                .is_none()
        );
    }

    #[test]
    fn model_and_protocol_effort_restrictions_fail_before_sending() {
        let mut profile =
            ProviderProfile::openai_chat("http://example.test/v1", "custom", AuthKind::None);
        profile.reasoning_efforts = Some(vec![ReasoningEffort::Low, ReasoningEffort::High]);
        let provider = ProviderClient::from_profile(profile, "").unwrap();
        provider
            .set_reasoning(ReasoningConfig::from_effort_text("high").unwrap())
            .unwrap();
        assert!(
            provider
                .set_reasoning(ReasoningConfig::from_effort_text("xhigh").unwrap())
                .is_err()
        );
        assert_eq!(provider.reasoning().effort, Some(ReasoningEffort::High));
        let mut gemini = ProviderProfile::openai_chat(
            "http://example.test/v1",
            "gemini-3.8-flash",
            AuthKind::None,
        );
        gemini.protocol = Protocol::GeminiInteractions;
        provider.switch_provider_profile(gemini, "").unwrap();
        for effort in ["xhigh", "max", "ultra", "none"] {
            assert!(
                provider
                    .set_reasoning(ReasoningConfig::from_effort_text(effort).unwrap())
                    .is_err()
            );
        }
        assert_eq!(provider.reasoning().effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn openai_output_caps_use_modern_field_and_survive_correction() {
        let mut profile =
            ProviderProfile::openai_chat("https://api.openai.com/v1", "o3", AuthKind::Bearer);
        profile.max_output_tokens = Some(4096);
        let provider = ProviderClient::from_profile(profile, "test").unwrap();
        let request = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![kernel::Message::user("hi")],
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        assert_eq!(request.body["max_completion_tokens"], 4096);
        assert!(request.body.get("max_tokens").is_none());
        let corrected = provider.with_output_limit(&request, 2048).unwrap().unwrap();
        assert_eq!(corrected.body["max_completion_tokens"], 2048);
        assert!(corrected.body.get("max_tokens").is_none());
        let count = openai_chat::vllm_tokenize_body(&corrected);
        assert!(count.get("max_completion_tokens").is_none());
    }

    #[test]
    fn auxiliary_reasoning_override_does_not_change_chat_setting_or_original_request() {
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_max_ctx(128_000)
            .with_max_output_tokens(512)
            .with_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Medium),
            })
            .unwrap();
        let request = p
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::user("hello")],
                ordered: None,
                tools: vec![],
            })
            .unwrap();
        let bounded = p.with_output_limit(&request, 6_400).unwrap().unwrap();
        let summary = p
            .with_request_reasoning(
                &bounded,
                &ReasoningConfig {
                    enabled: Some(false),
                    effort: None,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(summary.body["reasoning_effort"], "none");
        assert_eq!(summary.body["max_tokens"], 6_400);
        assert_eq!(request.body["max_tokens"], 512);
        assert_eq!(p.requested_output_tokens(), Some(512));
        assert_eq!(p.model_limits().max_output_tokens, None);
        assert_eq!(request.body["reasoning_effort"], "medium");
        assert_eq!(p.reasoning().effort, Some(ReasoningEffort::Medium));
        assert_ne!(request.request_fingerprint, summary.request_fingerprint);
        assert_eq!(
            serde_json::to_value(&request.context.messages).unwrap(),
            serde_json::to_value(&summary.context.messages).unwrap()
        );
    }

    #[test]
    fn untouched_reasoning_sends_no_effort_field() {
        let p = OpenAiCompat::new("http://x", "", "m");
        assert!(p.reasoning_effort_value().is_none());
    }

    #[test]
    fn switching_connection_updates_model_and_context_for_the_next_turn() {
        let p = OpenAiCompat::new("http://one", "", "first").with_max_ctx(8_192);
        assert_eq!(p.active_model(), "first");
        assert_eq!(p.context_window(), Some(8_192));

        p.switch_connection_with_context("http://two", "secret", "second", Some(32_768));
        assert_eq!(p.active_model(), "second");
        assert_eq!(p.context_window(), Some(32_768));
    }

    #[test]
    fn auth_header_is_omitted_when_empty_and_normalizes_bearer_prefix() {
        let client = reqwest::Client::new();
        let no_key = http::with_bearer(client.get("http://localhost"), "")
            .build()
            .unwrap();
        assert!(
            no_key
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );

        let key = http::with_bearer(client.get("http://localhost"), "Bearer secret")
            .build()
            .unwrap();
        assert_eq!(
            key.headers().get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer secret"
        );
    }

    #[test]
    fn full_request_json_actually_contains_the_field() {
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Medium),
            })
            .unwrap();
        let req = p
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::user("hello")],
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        assert_eq!(req.body["reasoning_effort"], "medium");
        assert!(req.body.get("chat_template_kwargs").is_none());
    }
}
