use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::{
    AllowAll, AutoDeny, BlastRadius, Block, Budget, CompileResult, CompiledContext, ContentPart,
    ContextEngine, EventLog, InMemoryLog, InputTokenCount, Kernel, Message, ModelMessage, NoVerify,
    Observation, PreparedModelRequest, Protocol, Provider, ProviderCaps, ProviderError,
    ProviderState, ReasoningPart, Role, Session, StopReason, TokenAccountingMode, TokenCountError,
    TokenCountQuality, ToolCallPart, ToolCallStrategy, ToolCategory, ToolIntent, ToolResultPart,
    ToolSpec,
};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct RecordingProvider {
    caps: ProviderCaps,
    turns: Mutex<VecDeque<Result<Vec<Block>, ProviderError>>>,
    counted: Mutex<Vec<String>>,
    sent: Mutex<Vec<PreparedModelRequest>>,
    counter_available: bool,
    count_quality: TokenCountQuality,
    strict: bool,
}

impl RecordingProvider {
    fn new(turns: Vec<Result<Vec<Block>, ProviderError>>) -> Self {
        Self {
            caps: ProviderCaps {
                vision: false,
                caching: false,
                max_ctx: Some(32_000),
                tool_calls: ToolCallStrategy::Native,
            },
            turns: Mutex::new(turns.into()),
            counted: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
            counter_available: true,
            count_quality: TokenCountQuality::Authoritative,
            strict: true,
        }
    }

    fn without_counter(mut self) -> Self {
        self.counter_available = false;
        self
    }

    fn with_count_quality(mut self, quality: TokenCountQuality) -> Self {
        self.count_quality = quality;
        self
    }
}

#[async_trait]
impl Provider for RecordingProvider {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    fn token_accounting_mode(&self) -> TokenAccountingMode {
        if self.strict {
            TokenAccountingMode::Strict
        } else {
            TokenAccountingMode::Adaptive
        }
    }

    fn requested_output_tokens(&self) -> Option<u64> {
        Some(2_000)
    }

    fn prepare_request(
        &self,
        ctx: &CompiledContext,
    ) -> Result<PreparedModelRequest, ProviderError> {
        let body = json!({
            "model": "recording-model",
            "messages": ctx.messages,
            "tools": ctx.tools,
            "max_tokens": 2_000,
        });
        Ok(PreparedModelRequest::new(
            Protocol::OpenAiChat,
            "recording-model",
            body,
            ctx.clone(),
        ))
    }

    async fn count_input_tokens(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<Option<InputTokenCount>, TokenCountError> {
        if !self.counter_available {
            return Ok(None);
        }
        self.counted
            .lock()
            .unwrap()
            .push(request.request_fingerprint.clone());
        Ok(Some(InputTokenCount {
            tokens: 100,
            quality: self.count_quality,
            request_fingerprint: request.request_fingerprint.clone(),
        }))
    }

    fn with_output_limit(
        &self,
        request: &PreparedModelRequest,
        max_output_tokens: u64,
    ) -> Result<Option<PreparedModelRequest>, ProviderError> {
        let mut body = request.body.clone();
        body["max_tokens"] = json!(max_output_tokens);
        Ok(Some(request.with_body(body)))
    }

    async fn stream(
        &self,
        _ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        Err(ProviderError::Decode(
            "kernel bypassed the prepared-request path".into(),
        ))
    }

    async fn stream_prepared(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        self.sent.lock().unwrap().push(request.clone());
        match self.turns.lock().unwrap().pop_front() {
            Some(Ok(blocks)) => Ok(stream::iter(blocks.into_iter().map(Ok)).boxed()),
            Some(Err(error)) => Err(error),
            None => Ok(stream::iter(vec![Ok(Block::Text("done".into()))]).boxed()),
        }
    }
}

struct ToolExecutor;

#[async_trait]
impl kernel::Executor for ToolExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "test.tool".into(),
            description: "return a result".into(),
            schema: json!({ "type": "object", "properties": {} }),
            blast_radius: BlastRadius::Read,
            category: ToolCategory::Read,
            icon: "t".into(),
        }]
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        Observation::ok(
            intent.id.clone(),
            json!({ "large_result": "x".repeat(20_000) }),
        )
    }
}

struct MemArtifacts;

impl kernel::ArtifactStore for MemArtifacts {
    fn put(&self, _bytes: &[u8]) -> Result<String, String> {
        Ok("hash".into())
    }
    fn get(&self, _hash: &str, _offset: usize, _len: Option<usize>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
    fn size(&self, _hash: &str) -> Result<usize, String> {
        Ok(0)
    }
}

struct Passthrough;

#[async_trait]
impl ContextEngine for Passthrough {
    async fn compile(&self, messages: &[Message], _max_ctx: Option<u32>) -> CompileResult {
        CompileResult {
            messages: messages.to_vec(),
            source_indices: (0..messages.len()).map(Some).collect(),
            compacted: false,
            summarized: false,
            before_tokens: 0,
            after_tokens: 0,
            overflow: false,
            summary: None,
        }
    }
}

struct ForcedCompactor {
    force: AtomicBool,
    limits: Mutex<Vec<Option<u32>>>,
}

impl ForcedCompactor {
    fn new() -> Self {
        Self {
            force: AtomicBool::new(false),
            limits: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ContextEngine for ForcedCompactor {
    fn force_next_compaction(&self) {
        self.force.store(true, Ordering::Release);
    }

    async fn compile(&self, messages: &[Message], max_ctx: Option<u32>) -> CompileResult {
        self.limits.lock().unwrap().push(max_ctx);
        if self.force.swap(false, Ordering::AcqRel) && messages.len() > 1 {
            return CompileResult {
                messages: messages[1..].to_vec(),
                source_indices: (1..messages.len()).map(Some).collect(),
                compacted: true,
                summarized: true,
                before_tokens: 100,
                after_tokens: 50,
                overflow: false,
                summary: Some("older context compacted".into()),
            };
        }
        CompileResult {
            messages: messages.to_vec(),
            source_indices: (0..messages.len()).map(Some).collect(),
            compacted: false,
            summarized: false,
            before_tokens: 100,
            after_tokens: 100,
            overflow: false,
            summary: None,
        }
    }
}

struct DuplicateTailCompactor {
    compact_next: AtomicBool,
}

impl DuplicateTailCompactor {
    fn new() -> Self {
        Self {
            compact_next: AtomicBool::new(true),
        }
    }
}

#[async_trait]
impl ContextEngine for DuplicateTailCompactor {
    async fn compile(&self, messages: &[Message], _max_ctx: Option<u32>) -> CompileResult {
        if self.compact_next.swap(false, Ordering::AcqRel) {
            assert_eq!(messages.len(), 9, "test history shape changed");
            let source_indices = vec![Some(0), Some(1), Some(2), None, Some(7), Some(8)];
            let compacted = vec![
                messages[0].clone(),
                messages[1].clone(),
                messages[2].clone(),
                Message::new(Role::Assistant, "COMPACTED MIDDLE"),
                messages[7].clone(),
                messages[8].clone(),
            ];
            return CompileResult {
                messages: compacted,
                source_indices,
                compacted: true,
                summarized: true,
                before_tokens: 10_000,
                after_tokens: 1_000,
                overflow: false,
                summary: Some("COMPACTED MIDDLE".into()),
            };
        }
        CompileResult {
            messages: messages.to_vec(),
            source_indices: (0..messages.len()).map(Some).collect(),
            compacted: false,
            summarized: false,
            before_tokens: 1_000,
            after_tokens: 1_000,
            overflow: false,
            summary: None,
        }
    }
}

fn kernel_with_context(
    provider: Arc<RecordingProvider>,
    context: Arc<dyn ContextEngine>,
) -> Kernel<RecordingProvider, InMemoryLog> {
    Kernel::new(
        provider,
        Arc::new(InMemoryLog::new()),
        Arc::new(ToolExecutor),
        context,
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    )
}

fn signed_text(text: &str, signature: &str) -> ModelMessage {
    ModelMessage {
        role: Role::Assistant,
        parts: vec![ContentPart::Text(kernel::TextPart {
            text: text.into(),
            provider_state: vec![ProviderState {
                protocol: Protocol::AnthropicMessages,
                kind: "thinking-signature".into(),
                value: json!({"signature": signature}),
            }],
        })],
        trust: None,
    }
}

fn signatures(messages: &[ModelMessage], text: &str) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            ContentPart::Text(part) if part.text == text => part
                .provider_state
                .first()
                .and_then(|state| state.value["signature"].as_str())
                .map(str::to_string),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn tool_result_is_prepared_and_recounted_before_the_following_model_call() {
    let provider = Arc::new(RecordingProvider::new(vec![
        Ok(vec![Block::ToolIntent(ToolIntent {
            id: "call-1".into(),
            tool: "test.tool".into(),
            args: json!({}),
        })]),
        Ok(vec![Block::Text("finished".into())]),
    ]));
    let kernel = kernel_with_context(provider.clone(), Arc::new(Passthrough));
    let (messages, stop) = kernel
        .run_session(
            &Session::new(),
            vec![Message::user("go")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();

    assert_eq!(stop, StopReason::Finished);
    assert!(messages.iter().any(|message| message.role == Role::Tool));
    let counted = provider.counted.lock().unwrap().clone();
    let sent = provider.sent.lock().unwrap().clone();
    assert_eq!(counted.len(), 2);
    assert_eq!(sent.len(), 2);
    assert_eq!(counted[0], sent[0].request_fingerprint);
    assert_eq!(counted[1], sent[1].request_fingerprint);
    assert_ne!(counted[0], counted[1]);
    assert!(
        sent[1].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool")
    );
    assert_eq!(sent[1].body["tools"][0]["name"], "test.tool");
    let live_result = sent[1]
        .context
        .ordered
        .as_ref()
        .unwrap()
        .iter()
        .flat_map(|message| &message.parts)
        .find_map(|part| match part {
            ContentPart::ToolResult(result) => Some(&result.content),
            _ => None,
        })
        .expect("the live ordered request includes the tool result");
    assert!(live_result.len() < 8_000);
    assert!(live_result.contains("read_artifact"));
}

#[tokio::test]
async fn canonical_provider_state_survives_a_tool_loop_and_session_rehydration() {
    let state = ProviderState {
        protocol: Protocol::GeminiInteractions,
        kind: "thought-signature".into(),
        value: json!({"signature": "opaque-signed-value"}),
    };
    let call = ToolIntent {
        id: "call-signed".into(),
        tool: "test.tool".into(),
        args: json!({}),
    };
    let completed = ModelMessage {
        role: Role::Assistant,
        parts: vec![
            ContentPart::Reasoning(ReasoningPart {
                text: Some("summary".into()),
                provider_state: vec![state.clone()],
            }),
            ContentPart::ToolCall(ToolCallPart {
                id: call.id.clone(),
                tool: call.tool.clone(),
                args: call.args.clone(),
                provider_state: vec![state],
            }),
        ],
        trust: None,
    };
    let provider = Arc::new(RecordingProvider::new(vec![
        Ok(vec![
            Block::ToolIntent(call),
            Block::CompletedMessage(completed),
        ]),
        Ok(vec![Block::Text("finished".into())]),
        Ok(vec![Block::Text("resumed".into())]),
    ]));
    let kernel = kernel_with_context(provider.clone(), Arc::new(Passthrough));
    let session = Session::new();
    kernel
        .run_session(
            &session,
            vec![Message::user("go")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();

    let sent = provider.sent.lock().unwrap().clone();
    let replay = sent[1]
        .context
        .ordered
        .as_ref()
        .expect("kernel must prepare exact ordered history");
    assert!(replay.iter().any(|message| {
        message.parts.iter().any(|part| match part {
            ContentPart::Reasoning(part) => part
                .provider_state
                .iter()
                .any(|state| state.value["signature"] == "opaque-signed-value"),
            _ => false,
        })
    }));

    let events = kernel.log.events(session.id).await;
    let ordered = kernel::project_ordered_messages(&events);
    assert!(ordered.iter().any(ModelMessage::has_provider_state));

    let mut resumed = kernel::project_messages(&events);
    resumed.push(Message::user("continue"));
    kernel
        .run_session(
            &session,
            resumed,
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    let sent = provider.sent.lock().unwrap();
    assert!(
        sent[2]
            .context
            .ordered
            .as_ref()
            .unwrap()
            .iter()
            .any(ModelMessage::has_provider_state)
    );
}

#[tokio::test]
async fn duplicate_legacy_text_keeps_the_exact_tail_state_through_compaction_and_resume() {
    let provider = Arc::new(RecordingProvider::new(vec![
        Ok(vec![Block::Text("first run finished".into())]),
        Ok(vec![Block::Text("resume finished".into())]),
    ]));
    let kernel = kernel_with_context(provider.clone(), Arc::new(DuplicateTailCompactor::new()));
    let session = Session::new();

    kernel
        .log
        .append(kernel::Event::user_message(&session, "head"))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::model_text(&session, "head response"))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::model_message(
            &session,
            &signed_text("head response", "head-state"),
        ))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::user_message(&session, "repeat"))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::model_text(&session, "same answer"))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::model_message(
            &session,
            &signed_text("same answer", "discarded-middle-state"),
        ))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::user_message(&session, "filler"))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::model_text(&session, "filler response"))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::model_message(
            &session,
            &signed_text("filler response", "filler-state"),
        ))
        .await
        .unwrap();
    // The protected tail has the same legacy values as the discarded middle
    // pair, but a different provider-owned signature.
    kernel
        .log
        .append(kernel::Event::user_message(&session, "repeat"))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::model_text(&session, "same answer"))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::model_message(
            &session,
            &signed_text("same answer", "protected-tail-state"),
        ))
        .await
        .unwrap();

    let mut initial = vec![Message::system("SYSTEM")];
    initial.extend(kernel::project_messages(
        &kernel.log.events(session.id).await,
    ));
    assert_eq!(initial.len(), 9);
    kernel
        .run_session(
            &session,
            initial,
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();

    {
        let sent = provider.sent.lock().unwrap();
        let compacted = sent[0].context.ordered.as_ref().unwrap();
        assert_eq!(
            signatures(compacted, "same answer"),
            vec!["protected-tail-state"],
            "value matching rebound the protected tail to the discarded occurrence"
        );
    }

    let events = kernel.log.events(session.id).await;
    let mut resumed = vec![Message::system(
        "DIFFERENT CURRENT SYSTEM — MUST NOT REPLACE CHECKPOINT",
    )];
    resumed.extend(kernel::project_messages(&events));
    resumed.push(Message::user("continue"));
    kernel
        .run_session(
            &session,
            resumed,
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();

    let sent = provider.sent.lock().unwrap();
    let resumed_ordered = sent[1].context.ordered.as_ref().unwrap();
    assert_eq!(
        signatures(resumed_ordered, "same answer"),
        vec!["protected-tail-state"],
        "the exact protected-tail state must survive durable resume"
    );
}

#[tokio::test]
async fn ordered_compaction_checkpoint_alone_hydrates_canonical_resume_state() {
    let provider = Arc::new(RecordingProvider::new(vec![Ok(vec![Block::Text(
        "resumed".into(),
    )])]));
    let kernel = kernel_with_context(provider.clone(), Arc::new(Passthrough));
    let session = Session::new();
    let legacy = vec![
        Message::system("SYSTEM"),
        Message::user("protected request"),
        Message::new(Role::Assistant, "signed response"),
    ];
    let ordered = vec![
        legacy[0].ordered(),
        legacy[1].ordered(),
        signed_text("signed response", "checkpoint-only-state"),
    ];
    kernel
        .log
        .append(kernel::Event::compaction_snapshot(
            &session,
            10_000,
            1_000,
            Some("HANDOFF"),
            &legacy,
            &ordered,
        ))
        .await
        .unwrap();

    let mut resumed = vec![Message::system(
        "DIFFERENT CURRENT SYSTEM — MUST NOT REPLACE CHECKPOINT",
    )];
    resumed.extend(kernel::project_messages(
        &kernel.log.events(session.id).await,
    ));
    resumed.push(Message::user("continue"));
    kernel
        .run_session(
            &session,
            resumed,
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();

    let sent = provider.sent.lock().unwrap();
    let request_messages = &sent[0].context.messages;
    let request_systems = request_messages
        .iter()
        .filter(|message| message.role == Role::System)
        .collect::<Vec<_>>();
    assert_eq!(
        request_systems.len(),
        1,
        "resume duplicated the system sheath"
    );
    assert_eq!(
        request_systems[0].content, "SYSTEM",
        "resume substituted the newly generated system for the checkpoint system"
    );
    assert_eq!(
        signatures(sent[0].context.ordered.as_ref().unwrap(), "signed response"),
        vec!["checkpoint-only-state"],
        "a checkpoint is itself sufficient canonical replay state"
    );
}

#[tokio::test]
async fn resumed_ordered_tool_results_are_spilled_without_losing_identity_or_opaque_state() {
    let provider = Arc::new(RecordingProvider::new(vec![Ok(vec![Block::Text(
        "resumed".into(),
    )])]));
    let kernel = kernel_with_context(provider.clone(), Arc::new(Passthrough));
    let session = Session::new();
    let call = ToolIntent {
        id: "large-call".into(),
        tool: "test.tool".into(),
        args: json!({}),
    };
    let provider_state = ProviderState {
        protocol: Protocol::GeminiInteractions,
        kind: "opaque-tool-result-state".into(),
        value: json!({"signature": "retain-me"}),
    };
    let large = "z".repeat(40_000);
    let legacy = vec![
        Message::system("SYSTEM"),
        Message::user("run it"),
        Message::assistant_calls("", vec![call.clone()]),
        Message::tool_result(&call.id, &large),
    ];
    let ordered = vec![
        legacy[0].ordered(),
        legacy[1].ordered(),
        legacy[2].ordered(),
        ModelMessage {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResultPart {
                tool_call_id: call.id.clone(),
                content: large,
                provider_state: vec![provider_state.clone()],
            })],
            trust: None,
        },
    ];
    kernel
        .log
        .append(kernel::Event::compaction_snapshot(
            &session,
            20_000,
            2_000,
            Some("handoff"),
            &legacy,
            &ordered,
        ))
        .await
        .unwrap();

    let mut resumed = kernel::project_messages(&kernel.log.events(session.id).await);
    resumed.push(Message::user("continue"));
    kernel
        .run_session(
            &session,
            resumed,
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();

    let sent = provider.sent.lock().unwrap();
    let result = sent[0]
        .context
        .ordered
        .as_ref()
        .unwrap()
        .iter()
        .flat_map(|message| &message.parts)
        .find_map(|part| match part {
            ContentPart::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("resumed canonical tool result");
    assert_eq!(result.tool_call_id, call.id);
    assert_eq!(result.provider_state, vec![provider_state]);
    assert!(result.content.len() < 8_000);
    assert!(result.content.contains("read_artifact"));
}

#[tokio::test]
async fn half_valid_compaction_checkpoint_is_atomically_inert_on_resume() {
    let provider = Arc::new(RecordingProvider::new(vec![Ok(vec![Block::Text(
        "resumed".into(),
    )])]));
    let kernel = kernel_with_context(provider.clone(), Arc::new(Passthrough));
    let session = Session::new();
    kernel
        .log
        .append(kernel::Event::user_message(&session, "discarded request"))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::model_text(&session, "discarded response"))
        .await
        .unwrap();
    kernel
        .log
        .append(kernel::Event::model_message(
            &session,
            &signed_text("discarded response", "old-state"),
        ))
        .await
        .unwrap();

    let mut malformed =
        kernel::Event::compaction(&session, 10_000, 1_000, Some("CONSISTENT FALLBACK"));
    malformed.payload["snapshot"] = json!({
        "version": 1,
        "messages": [
            Message::system("SYSTEM"),
            Message::user("must-not-accept-half-snapshot"),
            Message::new(Role::Assistant, "half snapshot")
        ],
        "ordered": {"malformed": true}
    });
    kernel.log.append(malformed).await.unwrap();

    let events = kernel.log.events(session.id).await;
    let legacy_projection = kernel::project_messages(&events);
    let ordered_projection = kernel::project_ordered_messages(&events);
    assert_eq!(legacy_projection.len(), 2);
    assert_eq!(legacy_projection[0].content, "discarded request");
    assert_eq!(legacy_projection[1].content, "discarded response");
    assert_eq!(
        signatures(&ordered_projection, "discarded response"),
        vec!["old-state"],
        "an invalid versioned checkpoint must not alter either durable view"
    );
    assert!(
        !legacy_projection
            .iter()
            .any(|message| message.content.contains("CONSISTENT FALLBACK"))
    );

    let mut resumed = vec![Message::system("SYSTEM")];
    resumed.extend(legacy_projection);
    resumed.push(Message::user("continue"));
    kernel
        .run_session(
            &session,
            resumed,
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();

    let sent = provider.sent.lock().unwrap();
    let request = &sent[0].context;
    assert!(
        !request
            .messages
            .iter()
            .any(|message| message.content.contains("must-not-accept-half-snapshot"))
    );
    assert!(
        !request
            .ordered
            .as_ref()
            .unwrap()
            .iter()
            .flat_map(|message| &message.parts)
            .any(|part| matches!(
                part,
                ContentPart::Text(text) if text.text.contains("must-not-accept-half-snapshot")
            ))
    );
    assert!(
        !request
            .messages
            .iter()
            .any(|message| message.content.contains("CONSISTENT FALLBACK"))
    );
    assert_eq!(
        signatures(request.ordered.as_ref().unwrap(), "discarded response"),
        vec!["old-state"]
    );
}

#[tokio::test]
async fn strict_accounting_refuses_to_send_without_an_authoritative_counter() {
    let provider = Arc::new(
        RecordingProvider::new(vec![Ok(vec![Block::Text("must not send".into())])])
            .without_counter(),
    );
    let kernel = kernel_with_context(provider.clone(), Arc::new(Passthrough));
    let error = kernel
        .run_session(
            &Session::new(),
            vec![Message::user("go")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("strict token accounting"));
    assert!(provider.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn strict_accounting_refuses_provider_estimates() {
    let provider = Arc::new(
        RecordingProvider::new(vec![Ok(vec![Block::Text("must not send".into())])])
            .with_count_quality(TokenCountQuality::ProviderEstimate),
    );
    let kernel = kernel_with_context(provider.clone(), Arc::new(Passthrough));
    let error = kernel
        .run_session(
            &Session::new(),
            vec![Message::user("go")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("requires an authoritative"));
    assert!(provider.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn output_cap_error_retries_with_a_lower_cap_without_compacting_history() {
    let provider = Arc::new(RecordingProvider::new(vec![
        Err(ProviderError::Status(
            400,
            "max_tokens exceeds available_tokens: 1000".into(),
        )),
        Ok(vec![Block::Text("finished".into())]),
    ]));
    let kernel = kernel_with_context(provider.clone(), Arc::new(Passthrough));
    let (_, stop) = kernel
        .run_session(
            &Session::new(),
            vec![Message::user("go")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Finished);
    let sent = provider.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].body["max_tokens"], 2_000);
    assert_eq!(sent[1].body["max_tokens"], 936);
    assert_eq!(sent[0].body["messages"], sent[1].body["messages"]);
    let counted = provider.counted.lock().unwrap();
    assert_eq!(counted.len(), 2, "the adjusted request must be recounted");
    assert_eq!(counted[1], sent[1].request_fingerprint);
}

#[tokio::test]
async fn input_overflow_forces_compaction_without_halving_the_known_window() {
    let provider = Arc::new(RecordingProvider::new(vec![
        Err(ProviderError::Status(
            400,
            "input exceeds the context window".into(),
        )),
        Ok(vec![Block::Text("finished".into())]),
    ]));
    let context = Arc::new(ForcedCompactor::new());
    let kernel = kernel_with_context(provider.clone(), context.clone());
    let session = Session::new();
    let (_, stop) = kernel
        .run_session(
            &session,
            vec![Message::system("system"), Message::user("go")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Finished);
    {
        let limits = context.limits.lock().unwrap();
        assert!(limits.len() >= 3);
        assert!(limits.iter().all(|limit| *limit == Some(30_000)));
    }

    // Compare the exact request sent immediately after compaction with replay
    // at that durable checkpoint. This exercises the kernel logging path, not
    // merely the Event constructor/projectors in isolation.
    let sent = provider.sent.lock().unwrap().clone();
    let events = kernel.log.events(session.id).await;
    let checkpoint = events
        .iter()
        .position(|event| event.kind == kernel::EventKind::Compaction)
        .expect("forced compaction must be durable");
    assert_eq!(events[checkpoint].payload["snapshot"]["version"], 1);
    let replayed = kernel::project_messages(&events[..=checkpoint]);
    assert_eq!(
        serde_json::to_vec(&replayed).unwrap(),
        serde_json::to_vec(&sent[1].context.messages).unwrap(),
        "legacy request replay diverged from the live compacted input"
    );
    let replayed_ordered = kernel::project_ordered_messages(&events[..=checkpoint]);
    assert_eq!(
        serde_json::to_vec(&replayed_ordered).unwrap(),
        serde_json::to_vec(sent[1].context.ordered.as_ref().unwrap()).unwrap(),
        "ordered request replay diverged from the live compacted input"
    );
}
