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
    async fn count_input_tokens(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<Option<InputTokenCount>, TokenCountError> {
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
        Ok(stream::iter(blocks.into_iter().map(Ok)).boxed())
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
    assert_eq!(endpoint.sent.lock().unwrap()[0].body["max_tokens"], 2048);
}
