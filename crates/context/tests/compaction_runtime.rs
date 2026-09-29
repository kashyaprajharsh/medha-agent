//! Exercise the production compiler through the kernel, including durable replay.
use ::context::{CompactionPolicy, HeuristicCounter, PipelineEngine};
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::*;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Endpoint {
    caps: ProviderCaps,
    limit: AtomicU32,
    output: AtomicU32,
    reasoning: ReasoningSupport,
    counter_available: bool,
    block_delay: std::time::Duration,
    truncated: bool,
    turns: Mutex<VecDeque<Vec<Block>>>,
    sent: Mutex<Vec<PreparedModelRequest>>,
}

fn request_tokens(request: &PreparedModelRequest) -> u64 {
    // A deterministic authoritative counter supplied by this test endpoint.
    request
        .context
        .messages
        .iter()
        .map(|m| {
            (m.content.len().div_ceil(4)
                + m.tool_calls
                    .iter()
                    .map(|t| t.args.to_string().len().div_ceil(4))
                    .sum::<usize>()
                + 8) as u64
        })
        .sum::<u64>()
        + 64
}

impl Endpoint {
    fn new(limit: u32, turns: Vec<Vec<Block>>) -> Arc<Self> {
        Arc::new(Self {
            caps: ProviderCaps {
                images: kernel::ImageSupport::Unsupported,
                caching: false,
                max_ctx: Some(limit),
                tool_calls: ToolCallStrategy::Native,
            },
            limit: AtomicU32::new(limit),
            output: AtomicU32::new(0),
            reasoning: ReasoningSupport::Effort,
            counter_available: true,
            block_delay: std::time::Duration::ZERO,
            truncated: false,
            turns: Mutex::new(turns.into()),
            sent: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl Provider for Endpoint {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }
    fn context_window(&self) -> Option<u32> {
        Some(self.limit.load(Ordering::SeqCst))
    }
    fn requested_output_tokens(&self) -> Option<u64> {
        let output = self.output.load(Ordering::SeqCst);
        (output > 0).then_some(u64::from(output))
    }
    fn reasoning_support(&self) -> ReasoningSupport {
        self.reasoning
    }
    fn with_request_reasoning(
        &self,
        request: &PreparedModelRequest,
        config: &ReasoningConfig,
    ) -> Result<Option<PreparedModelRequest>, ProviderError> {
        let mut body = request.body.clone();
        if config.enabled == Some(false) {
            body["reasoning_effort"] = json!("none");
        } else if let Some(effort) = config.effort {
            body["reasoning_effort"] = json!(effort.as_str());
        } else {
            body.as_object_mut().unwrap().remove("reasoning_effort");
        }
        Ok(Some(request.with_body(body)))
    }
    async fn count_input_tokens(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<Option<InputTokenCount>, TokenCountError> {
        if !self.counter_available {
            return Ok(None);
        }
        Ok(Some(InputTokenCount {
            tokens: request_tokens(request),
            quality: TokenCountQuality::Authoritative,
            request_fingerprint: request.request_fingerprint.clone(),
        }))
    }
    fn with_output_limit(
        &self,
        request: &PreparedModelRequest,
        cap: u64,
    ) -> Result<Option<PreparedModelRequest>, ProviderError> {
        let mut body = request.body.clone();
        body["max_tokens"] = json!(cap);
        Ok(Some(request.with_body(body)))
    }
    async fn stream(
        &self,
        _: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        panic!("must prepare and check the actual request before sending")
    }
    async fn stream_prepared(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        assert!(
            request_tokens(request) < u64::from(self.limit.load(Ordering::SeqCst)),
            "oversized request reached generation"
        );
        self.sent.lock().unwrap().push(request.clone());
        let blocks = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![Block::Text("finished".into())]);
        let delay = self.block_delay;
        let cut_off = self.truncated.then_some(Err(ProviderError::Truncated));
        Ok(stream::iter(blocks.into_iter().map(Ok).chain(cut_off))
            .then(move |block| async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                block
            })
            .boxed())
    }
}

struct NoArtifacts;
impl ArtifactStore for NoArtifacts {
    fn put(&self, _: &[u8]) -> Result<String, String> {
        Err("test: storage unavailable".into())
    }
    fn get(&self, _: &str, _: usize, _: Option<usize>) -> Result<Vec<u8>, String> {
        Err("unavailable".into())
    }
    fn size(&self, _: &str) -> Result<usize, String> {
        Err("unavailable".into())
    }
}

struct LargeTool(AtomicUsize);
#[async_trait]
impl Executor for LargeTool {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "test.read".into(),
            description: "read".into(),
            schema: json!({"type":"object"}),
            blast_radius: BlastRadius::Read,
            category: ToolCategory::Read,
            icon: "r".into(),
        }]
    }
    async fn execute(&self, intent: &ToolIntent) -> Observation {
        self.0.fetch_add(1, Ordering::SeqCst);
        Observation::ok(&intent.id, json!({"content":"x".repeat(100_000)}))
    }
}

fn runtime(
    endpoint: Arc<Endpoint>,
    log: Arc<InMemoryLog>,
    tool: Arc<LargeTool>,
) -> Kernel<Endpoint, InMemoryLog> {
    Kernel::new(
        endpoint,
        log,
        tool,
        Arc::new(PipelineEngine::with_counter(
            CompactionPolicy::default(),
            Arc::new(HeuristicCounter),
        )),
        Arc::new(NoArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    )
}

fn history() -> Vec<Message> {
    let mut messages = vec![Message::system("Keep the user's constraints.")];
    messages.extend((0..80).map(|n| Message::user(format!("request {n}: {}", "a".repeat(800)))));
    messages
}

#[derive(Default)]
struct PressureSink(Mutex<Vec<ContextPressure>>);
impl StreamSink for PressureSink {
    fn context_pressure(&self, pressure: ContextPressure) {
        self.0.lock().unwrap().push(pressure);
    }
}

#[tokio::test]
async fn overfull_history_without_output_cap_compacts_before_send_and_resumes_checkpoint() {
    let endpoint = Endpoint::new(8_000, vec![]);
    assert_eq!(endpoint.requested_output_tokens(), None);
    let log = Arc::new(InMemoryLog::new());
    let session = Session::new();
    let tool = Arc::new(LargeTool(AtomicUsize::new(0)));
    let kernel = runtime(endpoint.clone(), log.clone(), tool.clone());
    let sink = PressureSink::default();
    let (_, stop) = kernel
        .run_session(&session, history(), Budget::default(), &sink, None)
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Finished);
    let events = log.events(session.id).await;
    assert!(events.iter().any(|e| e.kind == EventKind::Compaction));
    assert!(sink.0.lock().unwrap().first().unwrap().percent().unwrap() > 200);
    assert!(sink.0.lock().unwrap().last().unwrap().percent().unwrap() < 100);
    assert_eq!(endpoint.sent.lock().unwrap().len(), 1);
    assert!(endpoint.sent.lock().unwrap()[0].context.messages.len() < 40);

    // A new compiler/process replays the checkpoint instead of the large log.
    let resumed = runtime(endpoint.clone(), log.clone(), tool);
    resumed
        .run_session(
            &session,
            vec![Message::user("continue after restart")],
            Budget::default(),
            &sink,
            None,
        )
        .await
        .unwrap();
    let sent = endpoint.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert!(
        sent[1]
            .context
            .messages
            .iter()
            .any(|m| m.content == "continue after restart")
    );
    assert!(
        sent[1]
            .context
            .messages
            .iter()
            .any(|m| m.content.contains("summary") || m.content.contains("User asks:"))
    );
    assert!(sent[1].context.messages.len() < 40);
}

#[tokio::test]
async fn unchanged_prune_band_history_sends_without_compaction_checkpoints() {
    let endpoint = Endpoint::new(26_000, vec![]);
    let log = Arc::new(InMemoryLog::new());
    let session = Session::new();
    let kernel = runtime(
        endpoint.clone(),
        log.clone(),
        Arc::new(LargeTool(AtomicUsize::new(0))),
    );
    let mut messages = history();
    for _ in 0..3 {
        let (next, stop) = kernel
            .run_session(&session, messages, Budget::default(), &NullSink, None)
            .await
            .unwrap();
        assert_eq!(stop, StopReason::Finished);
        messages = next;
    }
    assert_eq!(endpoint.sent.lock().unwrap().len(), 3);
    assert!(
        !log.events(session.id)
            .await
            .iter()
            .any(|event| event.kind == EventKind::Compaction),
        "unchanged pruning must not append snapshots to the durable log"
    );
}

#[tokio::test]
async fn switching_to_smaller_window_checks_the_next_request_before_send() {
    let endpoint = Endpoint::new(50_000, vec![]);
    let log = Arc::new(InMemoryLog::new());
    let session = Session::new();
    let kernel = runtime(
        endpoint.clone(),
        log.clone(),
        Arc::new(LargeTool(AtomicUsize::new(0))),
    );
    let (mut messages, _) = kernel
        .run_session(&session, history(), Budget::default(), &NullSink, None)
        .await
        .unwrap();
    assert!(
        !log.events(session.id)
            .await
            .iter()
            .any(|e| e.kind == EventKind::Compaction)
    );
    endpoint.limit.store(8_000, Ordering::SeqCst);
    messages.push(Message::user("continue with the smaller model"));
    let (_, stop) = kernel
        .run_session(&session, messages, Budget::default(), &NullSink, None)
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Finished);
    assert!(
        log.events(session.id)
            .await
            .iter()
            .any(|e| e.kind == EventKind::Compaction)
    );
    assert!(endpoint.sent.lock().unwrap()[1].context.messages.len() < 40);
}

#[tokio::test]
async fn oversized_tool_result_blocks_followup_even_if_artifact_storage_fails() {
    let endpoint = Endpoint::new(
        8_000,
        vec![vec![Block::ToolIntent(ToolIntent {
            id: "read-1".into(),
            tool: "test.read".into(),
            args: json!({}),
        })]],
    );
    let log = Arc::new(InMemoryLog::new());
    let tool = Arc::new(LargeTool(AtomicUsize::new(0)));
    let kernel = runtime(endpoint.clone(), log.clone(), tool.clone());
    let session = Session::new();
    let (_, stop) = kernel
        .run_session(
            &session,
            vec![Message::user("read")],
            Budget::default(),
            &NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Budget(BudgetStop::ContextOverflow));
    assert_eq!(endpoint.sent.lock().unwrap().len(), 1);
    assert_eq!(tool.0.load(Ordering::SeqCst), 1);
    assert!(
        log.events(session.id)
            .await
            .iter()
            .any(|e| e.kind == EventKind::ToolObs && e.payload.to_string().len() > 100_000)
    );
}

#[tokio::test]
async fn protected_oversized_head_stops_after_bounded_compaction_and_keeps_checkpoint() {
    let endpoint = Endpoint::new(8_000, vec![]);
    let log = Arc::new(InMemoryLog::new());
    let session = Session::new();
    let kernel = runtime(
        endpoint.clone(),
        log.clone(),
        Arc::new(LargeTool(AtomicUsize::new(0))),
    );
    let mut messages = history();
    messages[0] = Message::system("mandatory instruction ".repeat(3_000));
    let (retained, stop) = kernel
        .run_session(&session, messages, Budget::default(), &NullSink, None)
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Budget(BudgetStop::ContextOverflow));
    assert!(endpoint.sent.lock().unwrap().is_empty());
    let events = log.events(session.id).await;
    let checkpoints = events
        .iter()
        .filter(|e| e.kind == EventKind::Compaction)
        .count();
    assert!((1..=3).contains(&checkpoints));
    let checkpoint = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::Compaction)
        .unwrap();
    assert!(
        checkpoint.payload["snapshot"]["messages"] == serde_json::to_value(retained).unwrap(),
        "the latest durable request checkpoint must equal the retained active history"
    );
}

#[tokio::test]
async fn summary_generation_is_bounded_and_oversized_summary_input_never_sends() {
    use ::context::{HistoryItem, LlmSummarizer, Summarizer};
    let endpoint = Endpoint::new(8_000, vec![]);
    let summarizer = LlmSummarizer::new(endpoint.clone());
    let large = vec![HistoryItem::text(Role::User, "x".repeat(100_000))];
    assert!(summarizer.summarize(None, &large).await.is_err());
    assert!(endpoint.sent.lock().unwrap().is_empty());
    let small = vec![HistoryItem::text(Role::User, "keep this task")];
    assert!(summarizer.summarize(None, &small).await.is_ok());
    assert_eq!(endpoint.sent.lock().unwrap()[0].body["max_tokens"], 400);
    assert_eq!(
        endpoint.sent.lock().unwrap()[0].body["reasoning_effort"],
        "none"
    );
}

#[tokio::test]
async fn a_fitting_replay_preserves_the_chat_prefix_and_reserves_summary_output() {
    use ::context::{HistoryItem, LlmSummarizer, Summarizer};
    let endpoint = Endpoint::new(8_000, vec![vec![Block::Text("SUMMARY".into())]]);
    let summarizer = LlmSummarizer::new(endpoint.clone());
    // Replay the compatible chat prefix when it fits with the summary's
    // own output reservation; do not flatten a cacheable prefix needlessly.
    let filler = "word ".repeat(4_800);
    let sent = CompiledContext {
        model: String::new(),
        messages: vec![Message::system("SYSTEM"), Message::user(&filler)],
        ordered: None,
        tools: Vec::new(),
    };
    let items = vec![HistoryItem::text(Role::User, filler.clone())];
    let summary = summarizer
        .summarize_bounded(None, &items, Some(&sent), Some("\"word word word\""), 128)
        .await;
    assert_eq!(summary.unwrap(), "SUMMARY");
    let requests = endpoint.sent.lock().unwrap();
    assert_eq!(requests[0].body["max_tokens"], 128);
    assert_eq!(
        requests.len(),
        1,
        "the replay is sent, never re-sent flattened"
    );
    assert_eq!(
        requests[0].context.messages.len(),
        3,
        "the replay carries the sent messages plus one trailing instruction"
    );
}

#[tokio::test]
async fn auxiliary_summary_scales_independently_of_the_chat_output_setting() {
    use ::context::{HistoryItem, LlmSummarizer, Summarizer};
    for support in [ReasoningSupport::Effort, ReasoningSupport::Unknown] {
        let mut endpoint = Endpoint::new(128_000, vec![]);
        Arc::get_mut(&mut endpoint).unwrap().reasoning = support;
        endpoint.output.store(512, Ordering::SeqCst);
        let summarizer = LlmSummarizer::new(endpoint.clone());
        let items = vec![HistoryItem::text(
            Role::Assistant,
            "source material ".repeat(25_000),
        )];
        summarizer.summarize(None, &items).await.unwrap();
        let requests = endpoint.sent.lock().unwrap();
        let cap = requests[0].body["max_tokens"].as_u64().unwrap();
        if support == ReasoningSupport::Unknown {
            assert!(requests[0].body.get("reasoning_effort").is_none());
            assert!(
                cap > 6_400,
                "a model that may think gets the room left: {cap}"
            );
        } else {
            assert_eq!(requests[0].body["reasoning_effort"], "none");
            assert!(cap > 2_048 && cap <= 6_400);
        }
        assert_eq!(endpoint.requested_output_tokens(), Some(512));
    }
}

#[tokio::test]
async fn auxiliary_profile_summarizes_only_the_middle_without_chat_images_or_tools() {
    use ::context::{HistoryItem, LlmSummarizer, Summarizer};
    let endpoint = Endpoint::new(128_000, vec![]);
    let summarizer = LlmSummarizer::new(endpoint.clone()).with_replay(false);
    let sent = CompiledContext {
        model: "chat-model".into(),
        messages: vec![Message::user("CHAT PREFIX THAT MUST NOT REPLAY")],
        ordered: None,
        tools: vec![],
    };
    let items = vec![HistoryItem::text(Role::User, "LATEST TASK")];
    summarizer
        .summarize_replaying(None, &items, &sent, None)
        .await
        .unwrap();
    let requests = endpoint.sent.lock().unwrap();
    assert_eq!(requests[0].context.messages.len(), 2);
    assert!(
        requests[0].context.messages[1]
            .content
            .contains("LATEST TASK")
    );
    assert!(!requests[0].body.to_string().contains("CHAT PREFIX"));
}

#[tokio::test]
async fn unusable_summary_output_is_rejected_instead_of_persisting_a_partial_handoff() {
    use ::context::{HistoryItem, LlmSummarizer, Summarizer};
    for (blocks, reason) in [
        (
            vec![
                Block::Text("incomplete summary".into()),
                Block::Usage(Usage {
                    completion_tokens: 400,
                    ..Default::default()
                }),
            ],
            "partial handoff",
        ),
        (
            vec![Block::Text("I cannot summarize this conversation.".into())],
            "declined",
        ),
        (vec![], "empty summary"),
    ] {
        let summarizer = LlmSummarizer::new(Endpoint::new(8_000, vec![blocks]));
        let items = vec![HistoryItem::text(Role::User, "TASK")];
        let error = summarizer.summarize(None, &items).await.unwrap_err();
        assert!(matches!(error, ::context::SummarizeError::Invalid(_)));
        assert!(error.to_string().contains(reason), "{error}");
    }
    let mut endpoint = Endpoint::new(8_000, vec![vec![Block::Text("incomplete".into())]]);
    Arc::get_mut(&mut endpoint).unwrap().truncated = true;
    let error = LlmSummarizer::new(endpoint)
        .summarize(None, &[HistoryItem::text(Role::User, "TASK")])
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("summary hit its output limit"),
        "{error}"
    );
}

#[tokio::test]
async fn summary_does_not_inherit_a_large_chat_reserve_or_tokenize_image_base64() {
    use ::context::{HistoryItem, LlmSummarizer, Summarizer};
    let endpoint = Endpoint::new(128_000, vec![]);
    endpoint.output.store(64_000, Ordering::SeqCst);
    let summarizer = LlmSummarizer::new(endpoint.clone());
    let mut image = Message::user("old screenshot");
    image.attachments.push(MediaPart {
        mime_type: "image/png".into(),
        source: MediaSource::Base64("IMAGE_BYTES".repeat(100_000)),
        label: Some("diagram.png".into()),
        width: None,
        height: None,
        byte_size: None,
        provider_state: Vec::new(),
    });
    let sent = CompiledContext {
        model: String::new(),
        messages: vec![image],
        ordered: None,
        tools: vec![],
    };
    // Fits the bounded auxiliary request but not the chat's 64k output reserve.
    let items = vec![HistoryItem::text(
        Role::Assistant,
        "source material ".repeat(20_000),
    )];
    summarizer
        .summarize_replaying(None, &items, &sent, None)
        .await
        .unwrap();
    let requests = endpoint.sent.lock().unwrap();
    assert!(!requests[0].body.to_string().contains("IMAGE_BYTES"));
    assert!(
        requests[0]
            .context
            .messages
            .iter()
            .all(|m| m.attachments.is_empty())
    );
    assert!(requests[0].body["max_tokens"].as_u64().unwrap() <= 6400);
    assert_eq!(endpoint.requested_output_tokens(), Some(64_000));
}

#[derive(Default)]
struct RestoredSummaryProbe(Mutex<Vec<Option<String>>>);
#[async_trait]
impl ::context::Summarizer for RestoredSummaryProbe {
    async fn summarize(
        &self,
        previous: Option<&str>,
        items: &[::context::HistoryItem],
    ) -> Result<String, ::context::SummarizeError> {
        assert!(items.iter().all(|item| {
            item.role != Role::Assistant
                || (!item.content.contains("[MEDHA durable user context]")
                    && previous != Some(item.content.as_str()))
        }));
        self.0.lock().unwrap().push(previous.map(str::to_owned));
        Ok("UPDATED HANDOFF".into())
    }
}

#[tokio::test]
async fn old_checkpoint_migrates_once_and_survives_cancel_and_restart() {
    let endpoint = Endpoint::new(128_000, vec![]);
    let log = Arc::new(InMemoryLog::new());
    let session = Session::new();
    let durable = "DURABLE GOAL, USER CONSTRAINTS AND UNFINISHED WORK";
    let mut checkpoint = vec![
        Message::system("SYSTEM"),
        Message::user("INITIAL TASK"),
        Message::new(Role::Assistant, durable),
    ];
    checkpoint.extend((0..40).map(|i| {
        Message::new(
            Role::Assistant,
            format!("execution {i}: {}", "x".repeat(2000)),
        )
    }));
    let ordered = checkpoint.iter().map(Message::ordered).collect::<Vec<_>>();
    log.append(Event::user_message(&session, "OBSOLETE HISTORY"))
        .await
        .unwrap();
    log.append(Event::compaction_snapshot(
        &session,
        100_000,
        20_000,
        Some(durable),
        &checkpoint,
        &ordered,
    ))
    .await
    .unwrap();
    let probe = Arc::new(RestoredSummaryProbe::default());
    let make_runtime = || {
        Kernel::new(
            endpoint.clone(),
            log.clone(),
            Arc::new(LargeTool(AtomicUsize::new(0))),
            Arc::new(
                PipelineEngine::with_counter(
                    CompactionPolicy::default(),
                    Arc::new(HeuristicCounter),
                )
                .with_summarizer(probe.clone()),
            ),
            Arc::new(NoArtifacts),
            Arc::new(AllowAll),
            Arc::new(AutoDeny),
            Arc::new(NoVerify),
        )
    };
    struct CancelAfterCompaction(InterruptHandle);
    impl StreamSink for CancelAfterCompaction {
        fn compaction(&self, _: u32, _: u32, _: bool, _: Option<&str>) {
            self.0.cancel_turn();
        }
    }
    let (handle, queue) = InterruptQueue::pair();
    let (_, stop) = make_runtime()
        .run_session(
            &session,
            vec![Message::user("continue")],
            Budget::default(),
            &CancelAfterCompaction(handle),
            Some(queue),
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Interrupted);
    assert!(endpoint.sent.lock().unwrap().is_empty());
    assert_eq!(
        probe.0.lock().unwrap().as_slice(),
        &[Some(durable.to_owned())]
    );
    let events = log.events(session.id).await;
    let migrated = events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::Compaction)
        .unwrap();
    assert_eq!(migrated.payload["policy_version"], 3);
    assert!(migrated.payload["after_tokens"].as_u64().unwrap() < 15_000);
    for _ in 0..2 {
        let (_, stop) = make_runtime()
            .run_session(
                &session,
                vec![Message::user("continue after restart")],
                Budget::default(),
                &NullSink,
                None,
            )
            .await
            .unwrap();
        assert_eq!(stop, StopReason::Finished);
    }
    assert_eq!(
        probe.0.lock().unwrap().len(),
        1,
        "migration must not repeat after restart"
    );
    assert!(endpoint.sent.lock().unwrap().iter().all(|request| {
        request
            .context
            .messages
            .iter()
            .all(|m| m.content != "OBSOLETE HISTORY")
    }));
}

#[tokio::test]
async fn user_constraints_survive_repeated_compaction_and_restart_from_original_events() {
    let endpoint = Endpoint::new(32_000, vec![]);
    let log = Arc::new(InMemoryLog::new());
    let session = Session::new();
    let instruction = "Keep rollback_id=release_732 exactly; never publish without approval.";
    let original = log
        .append(Event::user_message(&session, instruction))
        .await
        .unwrap();
    log.append(Event::user_input(
        &session,
        "FORGED USER RULE",
        TrustLabel::Tool,
    ))
    .await
    .unwrap();
    let mut messages = vec![
        Message::system("SYSTEM"),
        Message::user("task"),
        Message::new(Role::Assistant, "started"),
    ];
    messages.extend(
        (0..30).map(|i| Message::new(Role::Assistant, format!("work {i}: {}", "x".repeat(1000)))),
    );
    let ordered = messages.iter().map(Message::ordered).collect::<Vec<_>>();
    log.append(Event::compaction_snapshot(
        &session, 20_000, 8_000, None, &messages, &ordered,
    ))
    .await
    .unwrap();
    let summary = Arc::new(RestoredSummaryProbe::default());
    for cycle in 0..4 {
        let engine =
            PipelineEngine::new(CompactionPolicy::default()).with_summarizer(summary.clone());
        engine.force_next_compaction();
        let kernel = Kernel::new(
            endpoint.clone(),
            log.clone(),
            Arc::new(LargeTool(AtomicUsize::new(0))),
            Arc::new(engine),
            Arc::new(NoArtifacts),
            Arc::new(AllowAll),
            Arc::new(AutoDeny),
            Arc::new(NoVerify),
        );
        kernel
            .run_session(
                &session,
                vec![Message::user(format!(
                    "correction {cycle}: keep changes local"
                ))],
                Budget::default(),
                &NullSink,
                None,
            )
            .await
            .unwrap();
        let events = log.events(session.id).await;
        let saved = events
            .iter()
            .rev()
            .find(|event| event.kind == EventKind::Compaction)
            .unwrap();
        let text = saved.payload["summary"].as_str().unwrap();
        let notes = text.split_once("[MEDHA durable user context]").unwrap().1;
        assert!(notes.contains(instruction));
        assert!(notes.contains(&original.id.to_string()));
        assert!(notes.contains(&format!("correction {cycle}")));
        assert!(!notes.contains("FORGED USER RULE"));
        assert_eq!(text.matches("[MEDHA durable user context]").count(), 1);
        assert!(::context::TokenCounter::count(&::context::BpeCounter::o200k(), text) <= 4_320);
    }
    assert!(
        summary
            .0
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .all(|text| !text.contains("[MEDHA durable user context]"))
    );
}

struct FailedSummary;
#[async_trait]
impl ::context::Summarizer for FailedSummary {
    async fn summarize(
        &self,
        _: Option<&str>,
        _: &[::context::HistoryItem],
    ) -> Result<String, ::context::SummarizeError> {
        Err(ProviderError::Status(401, "expired credential".into()).into())
    }
}

#[tokio::test]
async fn summary_provider_failure_does_not_replace_durable_history() {
    let endpoint = Endpoint::new(8_000, vec![]);
    let log = Arc::new(InMemoryLog::new());
    let session = Session::new();
    let kernel = Kernel::new(
        endpoint.clone(),
        log.clone(),
        Arc::new(LargeTool(AtomicUsize::new(0))),
        Arc::new(
            PipelineEngine::new(CompactionPolicy::default())
                .with_summarizer(Arc::new(FailedSummary)),
        ),
        Arc::new(NoArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    );
    let (_, stop) = kernel
        .run_session(&session, history(), Budget::default(), &NullSink, None)
        .await
        .expect("a failed summary must not end the turn");
    assert_eq!(stop, StopReason::Finished);
    let events = log.events(session.id).await;
    assert!(events.iter().any(|event| {
        event.kind == EventKind::SummaryFailed
            && event.payload["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("expired credential"))
    }));
    assert!(
        events
            .iter()
            .filter(|event| event.kind == EventKind::Compaction)
            .all(|event| event.payload["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains("[MEDHA extractive summary")))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == EventKind::UserMessage)
            .count(),
        80
    );
}

#[tokio::test]
async fn provider_usage_survives_restart_only_while_the_priced_prefix_matches() {
    let mut endpoint = Endpoint::new(
        128_000,
        vec![vec![
            Block::Text("done".into()),
            Block::Usage(Usage {
                prompt_tokens: 80_000,
                completion_tokens: 10,
                total_tokens: 80_010,
                ..Usage::default()
            }),
        ]],
    );
    Arc::get_mut(&mut endpoint).unwrap().counter_available = false;
    let log = Arc::new(InMemoryLog::new());
    let session = Session::new();
    runtime(
        endpoint.clone(),
        log.clone(),
        Arc::new(LargeTool(AtomicUsize::new(0))),
    )
    .run_session(
        &session,
        vec![Message::system("SYSTEM"), Message::user("initial task")],
        Budget::default(),
        &NullSink,
        None,
    )
    .await
    .unwrap();
    assert!(
        log.events(session.id)
            .await
            .iter()
            .any(|event| event.kind == EventKind::ContextUsage)
    );
    for (system, expected_anchor) in [("SYSTEM", true), ("edited system", false)] {
        let mut messages = vec![Message::system(system)];
        messages.extend(kernel::project_messages(&log.events(session.id).await));
        messages.push(Message::user("continue"));
        let sink = PressureSink::default();
        runtime(
            endpoint.clone(),
            log.clone(),
            Arc::new(LargeTool(AtomicUsize::new(0))),
        )
        .run_session(&session, messages, Budget::default(), &sink, None)
        .await
        .unwrap();
        let count = sink.0.lock().unwrap()[0].input_tokens;
        assert_eq!(count >= 80_000, expected_anchor, "{system}: {count}");
    }
}

#[tokio::test(start_paused = true)]
async fn summary_waits_for_progress_but_bounds_silence_and_total_time() {
    use ::context::{HistoryItem, LlmSummarizer, Summarizer};
    for (delay, chunks, succeeds) in [(45, 3, true), (65, 1, false), (45, 8, false)] {
        let mut endpoint = Endpoint::new(
            128_000,
            vec![vec![Block::Text("summary text ".into()); chunks]],
        );
        Arc::get_mut(&mut endpoint).unwrap().block_delay = std::time::Duration::from_secs(delay);
        let summarizer = LlmSummarizer::new(endpoint);
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            summarizer.time_limit(),
            summarizer.summarize(None, &[HistoryItem::text(Role::User, "TASK")]),
        )
        .await;
        assert_eq!(matches!(result, Ok(Ok(_))), succeeds);
        let elapsed = started.elapsed().as_secs();
        assert_eq!(
            elapsed,
            if succeeds {
                delay * chunks as u64
            } else if delay > 60 {
                60
            } else {
                300
            }
        );
    }
}
