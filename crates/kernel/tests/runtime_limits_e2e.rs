use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::{
    AllowAll, AutoDeny, BlastRadius, Block, Budget, BudgetStop, CompileResult, CompiledContext,
    ContextEngine, EventKind, EventLog, Executor, InMemoryLog, InputTokenCount, InterruptQueue,
    Kernel, KernelError, Message, NoVerify, Observation, PreparedModelRequest, Provider,
    ProviderCaps, ProviderError, Session, StopReason, TokenCountError, TokenCountQuality,
    ToolCallStrategy, ToolIntent, ToolSpec, Verifier,
};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

enum Turn {
    Blocks(Vec<Block>),
    InfiniteEmptyBlocks,
    HangBeforeFirstByte,
}

struct LimitProvider {
    caps: ProviderCaps,
    turns: Mutex<VecDeque<Turn>>,
    calls: AtomicUsize,
}

impl LimitProvider {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            caps: ProviderCaps {
                vision: false,
                caching: false,
                max_ctx: Some(32_000),
                tool_calls: ToolCallStrategy::Native,
            },
            turns: Mutex::new(turns.into()),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Provider for LimitProvider {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    fn requested_output_tokens(&self) -> Option<u64> {
        Some(40)
    }

    async fn count_input_tokens(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<Option<InputTokenCount>, TokenCountError> {
        Ok(Some(InputTokenCount {
            tokens: 10,
            quality: TokenCountQuality::Authoritative,
            request_fingerprint: request.request_fingerprint.clone(),
        }))
    }

    async fn stream(
        &self,
        _ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let turn = self.turns.lock().unwrap().pop_front();
        match turn {
            Some(Turn::Blocks(blocks)) => Ok(stream::iter(blocks.into_iter().map(Ok)).boxed()),
            Some(Turn::InfiniteEmptyBlocks) => {
                Ok(
                    stream::unfold((), |_| async { Some((Ok(Block::Text(String::new())), ())) })
                        .boxed(),
                )
            }
            Some(Turn::HangBeforeFirstByte) => std::future::pending().await,
            None => Ok(stream::iter([Ok(Block::Text("done".into()))]).boxed()),
        }
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

#[derive(Default)]
struct CountingExecutor {
    starts: AtomicUsize,
    started: tokio::sync::Notify,
    hang: bool,
    mutates: bool,
}

#[async_trait]
impl Executor for CountingExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn blast_radius(&self, _tool: &str) -> Option<BlastRadius> {
        self.mutates.then_some(BlastRadius::ReversibleLocal)
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        if self.hang {
            std::future::pending::<()>().await;
        }
        Observation::ok(intent.id.clone(), json!({"ok": true}))
    }
}

struct HangingVerifier;

#[async_trait]
impl Verifier for HangingVerifier {
    async fn check(
        &self,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<kernel::VerifyReport> {
        std::future::pending().await
    }
}

struct MemArtifacts;

impl kernel::ArtifactStore for MemArtifacts {
    fn put(&self, _bytes: &[u8]) -> Result<String, String> {
        Ok("sha256:test".into())
    }

    fn get(&self, _hash: &str, _offset: usize, _len: Option<usize>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }

    fn size(&self, _hash: &str) -> Result<usize, String> {
        Ok(0)
    }
}

fn kernel_with(
    provider: Arc<LimitProvider>,
    executor: Arc<CountingExecutor>,
) -> Kernel<LimitProvider, InMemoryLog> {
    Kernel::new(
        provider,
        Arc::new(InMemoryLog::new()),
        executor,
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    )
}

fn intent(index: usize) -> Block {
    Block::ToolIntent(ToolIntent {
        id: format!("call-{index}"),
        tool: "test.read".into(),
        args: json!({}),
    })
}

fn one_second_wall_budget() -> Budget {
    Budget {
        max_turns: None,
        max_tokens: None,
        max_cost_usd: None,
        max_wall_s: Some(1),
        pooled: None,
    }
}

#[tokio::test]
async fn a_hung_provider_connection_stops_at_the_task_wall_deadline() {
    let provider = Arc::new(LimitProvider::new(vec![Turn::HangBeforeFirstByte]));
    let kernel = kernel_with(provider, Arc::new(CountingExecutor::default()));
    let started = Instant::now();
    let (_, stop) = tokio::time::timeout(
        Duration::from_secs(2),
        kernel.run_session(
            &Session::new(),
            vec![Message::user("go")],
            one_second_wall_budget(),
            &kernel::NullSink,
            None,
        ),
    )
    .await
    .expect("wall deadline must terminate the connection wait")
    .unwrap();
    assert_eq!(stop, StopReason::Budget(BudgetStop::Wall));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_hung_tool_is_dropped_at_the_task_wall_deadline() {
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![intent(0)])]));
    let executor = Arc::new(CountingExecutor {
        hang: true,
        ..CountingExecutor::default()
    });
    let kernel = kernel_with(provider, executor.clone());
    let (_, stop) = tokio::time::timeout(
        Duration::from_secs(2),
        kernel.run_session(
            &Session::new(),
            vec![Message::user("go")],
            one_second_wall_budget(),
            &kernel::NullSink,
            None,
        ),
    )
    .await
    .expect("wall deadline must terminate a hung tool")
    .unwrap();
    assert_eq!(stop, StopReason::Budget(BudgetStop::Wall));
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_hung_verifier_is_dropped_at_the_task_wall_deadline() {
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![intent(0)])]));
    let executor = Arc::new(CountingExecutor {
        mutates: true,
        ..CountingExecutor::default()
    });
    let kernel = Kernel::new(
        provider,
        Arc::new(InMemoryLog::new()),
        executor,
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(HangingVerifier),
    );
    let (_, stop) = tokio::time::timeout(
        Duration::from_secs(2),
        kernel.run_session(
            &Session::new(),
            vec![Message::user("go")],
            one_second_wall_budget(),
            &kernel::NullSink,
            None,
        ),
    )
    .await
    .expect("wall deadline must terminate a hung verifier")
    .unwrap();
    assert_eq!(stop, StopReason::Budget(BudgetStop::Wall));
}

#[tokio::test]
async fn missing_usage_consumes_the_reserved_ceiling_before_another_turn() {
    let provider = Arc::new(LimitProvider::new(vec![
        Turn::Blocks(vec![intent(0)]),
        Turn::Blocks(vec![Block::Text("must not be sent".into())]),
    ]));
    let kernel = kernel_with(provider.clone(), Arc::new(CountingExecutor::default()));
    let (_, stop) = kernel
        .run_session(
            &Session::new(),
            vec![Message::user("go")],
            Budget {
                max_turns: None,
                max_tokens: Some(50),
                max_cost_usd: None,
                max_wall_s: None,
                pooled: None,
            },
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Budget(BudgetStop::Tokens));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn excessive_tool_fanout_is_rejected_before_any_intent_is_admitted() {
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(
        (0..65).map(intent).collect(),
    )]));
    let executor = Arc::new(CountingExecutor::default());
    let kernel = kernel_with(provider, executor.clone());
    let error = kernel
        .run_session(
            &Session::new(),
            vec![Message::user("go")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .expect_err("fanout above the hard cap must fail");
    assert!(matches!(error, KernelError::Provider(_)));
    assert_eq!(executor.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_infinite_provider_stream_hits_the_block_cap() {
    let provider = Arc::new(LimitProvider::new(vec![Turn::InfiniteEmptyBlocks]));
    let kernel = kernel_with(provider, Arc::new(CountingExecutor::default()));
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        kernel.run_session(
            &Session::new(),
            vec![Message::user("go")],
            Budget::default(),
            &kernel::NullSink,
            None,
        ),
    )
    .await
    .expect("an infinite stream must terminate at the block cap");
    assert!(matches!(result, Err(KernelError::Provider(_))));
}

#[tokio::test]
async fn cancellation_waits_one_shared_grace_and_never_starts_queued_tools() {
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(
        (0..20).map(intent).collect(),
    )]));
    let executor = Arc::new(CountingExecutor {
        hang: true,
        ..CountingExecutor::default()
    });
    let kernel = Arc::new(
        kernel_with(provider, executor.clone())
            .with_max_parallel_tools(1)
            .with_settle_grace(Duration::from_millis(80)),
    );
    let session = Session::new();
    let (handle, queue) = InterruptQueue::pair();
    let running = {
        let kernel = Arc::clone(&kernel);
        let session = session.clone();
        tokio::spawn(async move {
            kernel
                .run_session(
                    &session,
                    vec![Message::user("go")],
                    Budget::default(),
                    &kernel::NullSink,
                    Some(queue),
                )
                .await
        })
    };
    executor.started.notified().await;
    let cancelled = Instant::now();
    handle.cancel_turn();
    let (_, stop) = running.await.unwrap().unwrap();
    assert_eq!(stop, StopReason::Interrupted);
    assert!(cancelled.elapsed() < Duration::from_millis(400));
    assert_eq!(
        executor.starts.load(Ordering::SeqCst),
        1,
        "queued calls must receive synthetic observations without dispatch"
    );
    let events = kernel.log.events(session.id).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == EventKind::ToolObs)
            .count(),
        20
    );
}
