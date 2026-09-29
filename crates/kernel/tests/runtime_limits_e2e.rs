use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::{
    AllowAll, AutoDeny, BlastRadius, Block, Budget, BudgetStop, CompileResult, CompiledContext,
    ContextEngine, EventKind, EventLog, Executor, HookAudit, HookBatch, HookDecision,
    HookDirective, HookPoint, HookRequest, HookRunner, HookStatus, InMemoryLog, InputTokenCount,
    InterruptQueue, Kernel, KernelError, Message, NoVerify, Observation, PreparedModelRequest,
    Provider, ProviderCaps, ProviderError, Session, StopReason, TokenCountError, TokenCountQuality,
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
    requests: Mutex<Vec<CompiledContext>>,
}

impl LimitProvider {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            caps: ProviderCaps {
                images: kernel::ImageSupport::Unsupported,
                caching: false,
                max_ctx: Some(32_000),
                tool_calls: ToolCallStrategy::Native,
            },
            turns: Mutex::new(turns.into()),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
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
        ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        self.requests.lock().unwrap().push(ctx.clone());
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
        vec![ToolSpec {
            name: "test.read".into(),
            description: String::new(),
            schema: json!({}),
            blast_radius: self.blast_radius("test.read").unwrap_or(BlastRadius::Read),
            category: kernel::ToolCategory::Other,
            icon: String::new(),
        }]
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

#[derive(Clone, Copy)]
enum HookMode {
    Continue,
    Deny,
    RequestApproval,
}

struct ScriptedHooks {
    mode: HookMode,
    requests: Mutex<Vec<HookRequest>>,
}

impl ScriptedHooks {
    fn new(mode: HookMode) -> Self {
        Self {
            mode,
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl HookRunner for ScriptedHooks {
    async fn invoke(
        &self,
        request: &HookRequest,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> HookBatch {
        self.requests.lock().unwrap().push(request.clone());
        let (decision, directive, reason) = match (request.point, self.mode) {
            (HookPoint::PreTool, HookMode::Deny) => (
                HookDecision::Deny,
                HookDirective::Deny("blocked by test hook".into()),
                Some("blocked by test hook".into()),
            ),
            (HookPoint::PreTool, HookMode::RequestApproval) => (
                HookDecision::RequestApproval,
                HookDirective::RequestApproval("review required".into()),
                Some("review required".into()),
            ),
            _ => (HookDecision::Continue, HookDirective::Continue, None),
        };
        HookBatch {
            directive,
            audits: vec![HookAudit {
                event_id: request.event_id.clone(),
                plugin_id: "dev.medha.test".into(),
                component_id: "guard".into(),
                point: request.point,
                status: HookStatus::Completed,
                decision: Some(decision),
                reason,
                duration_ms: 1,
            }],
            contexts: Vec::new(),
            notices: Vec::new(),
        }
    }
}

struct HangingVerifier;

#[async_trait]
impl Verifier for HangingVerifier {
    fn required(&self) -> bool {
        true
    }

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

#[derive(Default)]
struct ReviewedCalls(Mutex<Vec<String>>);

impl kernel::Policy for ReviewedCalls {
    fn authorize(
        &self,
        _autonomy: kernel::AutonomyLevel,
        intent: &ToolIntent,
        _radius: Option<BlastRadius>,
    ) -> kernel::Decision {
        self.0.lock().unwrap().push(intent.tool.clone());
        kernel::Decision::Human
    }
}

#[async_trait]
impl kernel::HumanGate for ReviewedCalls {
    async fn confirm(
        &self,
        action: &str,
        _detail: Option<&str>,
        _escalated: bool,
    ) -> kernel::Approval {
        self.0.lock().unwrap().push(action.into());
        kernel::Approval::Once
    }
}

struct ChangingCatalog {
    empty: bool,
    catalog_reads: AtomicUsize,
    executions: Mutex<Vec<String>>,
}

#[async_trait]
impl Executor for ChangingCatalog {
    fn specs(&self) -> Vec<ToolSpec> {
        let reads = self.catalog_reads.fetch_add(1, Ordering::SeqCst);
        let mut names = if self.empty {
            vec![]
        } else {
            vec!["test.read", "test_read"]
        };
        // A fresh executor lookup would wrongly admit a tool not offered in
        // the request. Its risk metadata also deliberately looks legitimate.
        if reads > 0 {
            names.push("mcp__mcp-remote__brave_search");
        }
        names
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: String::new(),
                schema: json!({}),
                blast_radius: BlastRadius::Read,
                category: kernel::ToolCategory::Other,
                icon: String::new(),
            })
            .collect()
    }

    fn blast_radius(&self, _tool: &str) -> Option<BlastRadius> {
        Some(BlastRadius::Read)
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        self.executions.lock().unwrap().push(intent.tool.clone());
        Observation::ok(&intent.id, json!({"content": "read successfully"}))
    }
}

#[tokio::test]
async fn unavailable_calls_skip_policy_gate_and_execution_and_feed_back_the_request_catalog() {
    for empty in [false, true] {
        let unknown = "mcp__mcp-remote__brave_search";
        let guesses = (0..3)
            .map(|i| {
                Block::ToolIntent(ToolIntent {
                    id: format!("guess-{i}"),
                    tool: unknown.into(),
                    args: json!({"query": "agents"}),
                })
            })
            .collect();
        let mut turns = vec![Turn::Blocks(guesses)];
        if !empty {
            turns.push(Turn::Blocks(vec![intent(0)]));
        }
        let provider = Arc::new(LimitProvider::new(turns));
        let executor = Arc::new(ChangingCatalog {
            empty,
            catalog_reads: AtomicUsize::new(0),
            executions: Mutex::new(Vec::new()),
        });
        let policy = Arc::new(ReviewedCalls::default());
        let gate = Arc::new(ReviewedCalls::default());
        let log = Arc::new(InMemoryLog::new());
        let runner = Kernel::new(
            provider.clone(),
            log.clone(),
            executor.clone(),
            Arc::new(Passthrough),
            Arc::new(MemArtifacts),
            policy.clone(),
            gate.clone(),
            Arc::new(NoVerify),
        );
        let session = Session::new();
        let (_, stop) = runner
            .run_session(
                &session,
                vec![Message::user("search")],
                Budget::default(),
                &kernel::NullSink,
                None,
            )
            .await
            .unwrap();
        assert_eq!(stop, StopReason::Finished);
        let expected = if empty { vec![] } else { vec!["test.read"] };
        assert_eq!(*policy.0.lock().unwrap(), expected);
        assert_eq!(*executor.executions.lock().unwrap(), expected);
        assert_eq!(gate.0.lock().unwrap().len(), expected.len());
        assert_eq!(executor.catalog_reads.load(Ordering::SeqCst), 1);
        let events = log.events(session.id).await;
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::PolicyDecision)
                .count(),
            expected.len()
        );
        for i in 0..3 {
            let id = format!("guess-{i}");
            let obs = events
                .iter()
                .find(|e| e.kind == EventKind::ToolObs && e.payload["intent_id"] == id)
                .unwrap();
            assert_eq!(obs.payload["status"], "error");
            let payload = &obs.payload["payload"];
            assert_eq!(payload["error_code"], "tool_unavailable");
            assert_eq!(payload["requested_tool"], unknown);
            // Uses the same collision-safe mapping as the provider.
            assert_eq!(
                payload["available_tools"],
                if empty {
                    json!([])
                } else {
                    json!(["test_read", "test_read_"])
                }
            );
            let requests = provider.requests.lock().unwrap();
            let feedback = requests[1]
                .messages
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some(id.as_str()))
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&feedback.content).unwrap(),
                *payload
            );
        }
    }
}

#[tokio::test]
async fn unavailable_calls_keep_their_call_order_slot_among_real_results() {
    let guess = Block::ToolIntent(ToolIntent {
        id: "guess".into(),
        tool: "mcp__mcp-remote__brave_search".into(),
        args: json!({}),
    });
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![
        intent(0),
        guess,
        intent(1),
    ])]));
    let executor = Arc::new(ChangingCatalog {
        empty: false,
        catalog_reads: AtomicUsize::new(0),
        executions: Mutex::new(Vec::new()),
    });
    let log = Arc::new(InMemoryLog::new());
    let runner = Kernel::new(
        provider.clone(),
        log.clone(),
        executor.clone(),
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    );
    let session = Session::new();
    runner
        .run_session(
            &session,
            vec![Message::user("search")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(executor.executions.lock().unwrap().len(), 2);
    let order = ["call-0", "guess", "call-1"];
    let logged: Vec<_> = log
        .events(session.id)
        .await
        .into_iter()
        .filter(|e| e.kind == EventKind::ToolObs)
        .map(|e| e.payload["intent_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(logged, order);
    // Templates that render tool results without ids pair them by position.
    let requests = provider.requests.lock().unwrap();
    let fed_back: Vec<_> = requests[1]
        .messages
        .iter()
        .filter_map(|m| m.tool_call_id.clone())
        .collect();
    assert_eq!(fed_back, order);
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

#[derive(Default)]
struct PlanExecutor {
    executed: Mutex<Vec<String>>,
}

#[async_trait]
impl Executor for PlanExecutor {
    fn mutation_key(&self, intent: &ToolIntent) -> Option<String> {
        (intent.args["op"] == "write").then(|| "memory:project:fixture".to_owned())
    }
    fn specs(&self) -> Vec<ToolSpec> {
        ["fs.read", "edit", "shell.exec", "agent.spawn"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: name.into(),
                schema: json!({}),
                // Deliberately misleading schema metadata must not bypass the
                // executor's actual permission metadata.
                blast_radius: BlastRadius::Read,
                category: kernel::ToolCategory::Other,
                icon: String::new(),
            })
            .collect()
    }
    fn blast_radius(&self, name: &str) -> Option<BlastRadius> {
        match name {
            "fs.read" => Some(BlastRadius::Read),
            "edit" => Some(BlastRadius::ReversibleLocal),
            "shell.exec" => Some(BlastRadius::IrreversibleLocal),
            "agent.spawn" => Some(BlastRadius::External),
            _ => None,
        }
    }
    async fn execute(&self, intent: &ToolIntent) -> Observation {
        self.executed.lock().unwrap().push(intent.tool.clone());
        Observation::ok(&intent.id, json!({"content": "fixture"}))
    }
}

#[tokio::test]
async fn plan_rejects_a_mutating_operation_on_a_read_radius_tool() {
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![
        Block::ToolIntent(ToolIntent {
            id: "write".into(),
            tool: "fs.read".into(),
            args: json!({"op": "write"}),
        }),
        Block::ToolIntent(ToolIntent {
            id: "search".into(),
            tool: "fs.read".into(),
            args: json!({"op": "search"}),
        }),
    ])]));
    let executor = Arc::new(PlanExecutor::default());
    let log = Arc::new(InMemoryLog::new());
    let kernel = Kernel::new(
        provider,
        log.clone(),
        executor.clone(),
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    );
    let mut session = Session::new();
    session.autonomy = kernel::AutonomyLevel::Plan;
    kernel
        .run_session(
            &session,
            vec![Message::user("inspect")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(*executor.executed.lock().unwrap(), ["fs.read"]);
    assert!(
        !log.events(session.id)
            .await
            .iter()
            .any(|event| event.kind == EventKind::ToolEffectPrepared)
    );
}

#[tokio::test]
async fn plan_blocks_writes_shell_delegation_and_unknown_tools_even_with_allow_all() {
    let attempted = ["fs.read", "edit", "shell.exec", "agent.spawn", "unknown"];
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(
        attempted
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                Block::ToolIntent(ToolIntent {
                    id: format!("attempt-{i}"),
                    tool: name.into(),
                    args: json!({}),
                })
            })
            .collect(),
    )]));
    let executor = Arc::new(PlanExecutor::default());
    let log = Arc::new(InMemoryLog::new());
    let kernel = Kernel::new(
        provider.clone(),
        log.clone(),
        executor.clone(),
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    );
    let mut session = Session::new();
    session.autonomy = kernel::AutonomyLevel::Plan;
    let (history, stop) = kernel
        .run_session(
            &session,
            vec![Message::user("please implement everything")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Finished);
    assert_eq!(*executor.executed.lock().unwrap(), ["fs.read"]);
    let events = log.events(session.id).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::ToolObs
                && e.payload["payload"]["error_code"] == "tool_unavailable")
            .count(),
        4
    );
    for request in provider.requests.lock().unwrap().iter() {
        assert_eq!(
            request
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["fs.read"]
        );
        assert!(
            request
                .messages
                .iter()
                .any(|m| m.content.contains("Plan mode is active"))
        );
        assert!(request.ordered_messages().iter().any(|m| {
            serde_json::to_string(m)
                .unwrap()
                .contains("Plan mode is active")
        }));
    }
    assert!(
        !history
            .iter()
            .any(|m| m.content.contains("Plan mode is active"))
    );
    session.autonomy = kernel::AutonomyLevel::Normal;
    kernel
        .run_session(
            &session,
            history,
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    let last = requests.last().unwrap();
    assert_eq!(last.tools.len(), 4);
    assert!(
        !last
            .messages
            .iter()
            .any(|m| m.content.contains("Plan mode is active"))
    );
}

struct RequiredVerifier {
    results: Mutex<VecDeque<Option<bool>>>,
    calls: AtomicUsize,
    required: bool,
}
impl RequiredVerifier {
    fn new(results: Vec<Option<bool>>) -> Self {
        Self {
            results: Mutex::new(results.into()),
            calls: AtomicUsize::new(0),
            required: true,
        }
    }
}
#[async_trait]
impl Verifier for RequiredVerifier {
    fn required(&self) -> bool {
        self.required
    }
    async fn check(
        &self,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<kernel::VerifyReport> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .flatten()
            .map(|ok| kernel::VerifyReport {
                ok,
                summary: "fixture tests".into(),
                output: "deterministic check".into(),
            })
    }
}

#[tokio::test]
async fn required_checks_block_false_success_and_missing_results() {
    for final_result in [Some(false), None] {
        let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![intent(1)])]));
        let executor = Arc::new(CountingExecutor {
            mutates: true,
            ..Default::default()
        });
        // A post-edit pass is insufficient: changes can occur before completion.
        let verifier = Arc::new(RequiredVerifier::new(vec![Some(true), final_result]));
        let kernel = kernel_with(provider, executor).with_verifier(verifier.clone());
        let session = Session::new();
        let (history, stop) = kernel
            .run_session(
                &session,
                vec![Message::user("fix it")],
                Budget::default(),
                &kernel::NullSink,
                None,
            )
            .await
            .unwrap();
        assert_eq!(stop, StopReason::VerificationFailed);
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 2);
        assert!(history.last().unwrap().content.contains("[verifier] FAIL"));
        assert_eq!(
            history.last().unwrap().trust,
            Some(kernel::TrustLabel::Tool)
        );
        let logged = kernel.log.events(session.id).await;
        let result = logged
            .iter()
            .rev()
            .find_map(|event| event.payload.get("verifier"))
            .expect("the verifier result is recorded as itself, not only as text");
        assert_eq!(result["ok"], false);
    }
}

#[tokio::test]
async fn failed_checks_feed_back_then_allow_a_verified_repair() {
    let provider = Arc::new(LimitProvider::new(vec![
        Turn::Blocks(vec![intent(1)]),
        Turn::Blocks(vec![intent(2)]),
    ]));
    let verifier = Arc::new(RequiredVerifier::new(vec![
        Some(false),
        Some(true),
        Some(true),
    ]));
    let kernel = kernel_with(
        provider.clone(),
        Arc::new(CountingExecutor {
            mutates: true,
            ..Default::default()
        }),
    )
    .with_verifier(verifier.clone());
    let (_, stop) = kernel
        .run_session(
            &Session::new(),
            vec![Message::user("fix it")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Finished);
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 3);
    assert!(
        provider.requests.lock().unwrap()[1]
            .messages
            .iter()
            .any(|m| m.content.contains("[verifier] FAIL"))
    );
}

#[tokio::test]
async fn required_checks_recheck_text_only_and_resumed_completion() {
    let provider = Arc::new(LimitProvider::new(vec![]));
    let verifier = Arc::new(RequiredVerifier::new(vec![Some(true), Some(false)]));
    let kernel = kernel_with(provider, Arc::new(CountingExecutor::default()))
        .with_verifier(verifier.clone());
    let session = Session::new();
    let (history, stop) = kernel
        .run_session(
            &session,
            vec![Message::user("done?")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Finished);
    let (_, stop) = kernel
        .run_session(
            &session,
            history,
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::VerificationFailed);
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn plan_never_launches_verification_even_after_a_denied_mutation() {
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![intent(1)])]));
    let verifier = Arc::new(RequiredVerifier::new(vec![Some(false)]));
    let executor = Arc::new(CountingExecutor {
        mutates: true,
        ..Default::default()
    });
    let kernel = kernel_with(provider, executor.clone()).with_verifier(verifier.clone());
    let mut session = Session::new();
    session.autonomy = kernel::AutonomyLevel::Plan;
    let (_, stop) = kernel
        .run_session(
            &session,
            vec![Message::user("plan it")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Finished);
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn optional_verification_preserves_advisory_behavior() {
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![intent(1)])]));
    let mut verifier = RequiredVerifier::new(vec![Some(false)]);
    verifier.required = false;
    let kernel = kernel_with(
        provider,
        Arc::new(CountingExecutor {
            mutates: true,
            ..Default::default()
        }),
    )
    .with_verifier(Arc::new(verifier));
    let (_, stop) = kernel
        .run_session(
            &Session::new(),
            vec![Message::user("fix it")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Finished);
}

#[tokio::test]
async fn completion_check_obeys_the_task_wall_deadline() {
    let kernel = kernel_with(
        Arc::new(LimitProvider::new(vec![])),
        Arc::new(CountingExecutor::default()),
    )
    .with_verifier(Arc::new(HangingVerifier));
    let (_, stop) = tokio::time::timeout(
        Duration::from_secs(2),
        kernel.run_session(
            &Session::new(),
            vec![Message::user("done?")],
            one_second_wall_budget(),
            &kernel::NullSink,
            None,
        ),
    )
    .await
    .expect("completion must not hang on verification")
    .unwrap();
    assert_eq!(stop, StopReason::Budget(BudgetStop::Wall));
}

#[tokio::test]
async fn cumulative_usage_blocks_settle_once_at_the_end_of_an_attempt() {
    #[derive(Default)]
    struct UsageSink(Mutex<Vec<kernel::Usage>>);
    impl kernel::StreamSink for UsageSink {
        fn usage(&self, usage: &kernel::Usage) {
            self.0.lock().unwrap().push(*usage);
        }
    }
    let partial = kernel::Usage {
        prompt_tokens: 100,
        cached_prompt_tokens: Some(75),
        ..kernel::Usage::default()
    };
    let final_usage = kernel::Usage {
        completion_tokens: 5,
        total_tokens: 105,
        ..partial
    };
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![
        Block::Usage(partial),
        Block::Text("done".into()),
        Block::Usage(final_usage),
        Block::Usage(final_usage),
    ])]));
    let kernel = kernel_with(provider, Arc::new(CountingExecutor::default()));
    let sink = UsageSink::default();
    kernel
        .run_session(
            &Session::new(),
            vec![Message::user("go")],
            Budget::default(),
            &sink,
            None,
        )
        .await
        .unwrap();
    let recorded = sink.0.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].completion_tokens, 5);
    assert_eq!(recorded[0].cached_prompt_tokens, Some(75));
}

#[tokio::test]
async fn each_model_call_records_its_tokens_and_cost_in_the_log() {
    let usage = kernel::Usage {
        prompt_tokens: 1_000_000,
        completion_tokens: 500_000,
        total_tokens: 1_500_000,
        cached_prompt_tokens: Some(400_000),
    };
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![
        Block::Text("done".into()),
        Block::Usage(usage),
    ])]));
    let pricing = kernel::Pricing {
        input_per_mtok: 2.0,
        output_per_mtok: 8.0,
        cached_input_per_mtok: Some(0.5),
        indicative: false,
    };
    let kernel =
        kernel_with(provider, Arc::new(CountingExecutor::default())).with_pricing(Some(pricing));
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
    let events = kernel.log.events(session.id).await;
    let record = events
        .iter()
        .find(|event| event.kind == kernel::EventKind::ContextUsage)
        .expect("the model call is recorded");
    assert_eq!(record.payload["prompt_tokens"], 1_000_000);
    assert_eq!(record.payload["completion_tokens"], 500_000);
    assert_eq!(record.payload["cached_prompt_tokens"], 400_000);
    // 600k fresh input at $2, 400k cached at $0.50, 500k output at $8.
    assert_eq!(record.payload["cost_usd"], 5.4);
}

#[tokio::test]
async fn pre_tool_hooks_can_only_narrow_or_escalate_execution() {
    for mode in [HookMode::Deny, HookMode::RequestApproval] {
        let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![intent(0)])]));
        let executor = Arc::new(CountingExecutor::default());
        let log = Arc::new(InMemoryLog::new());
        let hooks = Arc::new(ScriptedHooks::new(mode));
        let kernel = Kernel::new(
            provider,
            log.clone(),
            executor.clone(),
            Arc::new(Passthrough),
            Arc::new(MemArtifacts),
            Arc::new(AllowAll),
            Arc::new(AutoDeny),
            Arc::new(NoVerify),
        )
        .with_hooks(hooks);
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
        assert_eq!(executor.starts.load(Ordering::SeqCst), 0);
        let events = log.events(session.id).await;
        assert!(
            events
                .iter()
                .any(|event| event.kind == EventKind::HookDecision)
        );
        let policy = events
            .iter()
            .find(|event| event.kind == EventKind::PolicyDecision)
            .unwrap();
        let expected = match mode {
            HookMode::Deny => "deny",
            HookMode::RequestApproval => "human",
            HookMode::Continue => unreachable!(),
        };
        assert_eq!(policy.payload["decision"], expected);
    }
}

#[tokio::test]
async fn hook_payloads_are_redacted_and_audited_around_real_execution() {
    let call = Block::ToolIntent(ToolIntent {
        id: "call-secret".into(),
        tool: "test.read".into(),
        args: json!({"path": "visible.txt", "api_key": "must-not-leak"}),
    });
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![call])]));
    let executor = Arc::new(CountingExecutor::default());
    let log = Arc::new(InMemoryLog::new());
    let hooks = Arc::new(ScriptedHooks::new(HookMode::Continue));
    let kernel = Kernel::new(
        provider,
        log.clone(),
        executor.clone(),
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    )
    .with_hooks(hooks.clone());
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
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
    {
        let requests = hooks.requests.lock().unwrap();
        let tool_points: Vec<HookPoint> = requests
            .iter()
            .filter(|request| request.point.is_tool_point())
            .map(|request| request.point)
            .collect();
        assert_eq!(tool_points, [HookPoint::PreTool, HookPoint::PostTool]);
        let pre_tool = requests
            .iter()
            .find(|request| request.point == HookPoint::PreTool)
            .unwrap();
        assert_eq!(pre_tool.payload["args"]["path"], "visible.txt");
        assert_eq!(pre_tool.payload["args"]["api_key"], "<redacted>");
    }

    let events = log.events(session.id).await;
    let tool_point = |event: &kernel::Event| {
        event.kind != EventKind::HookDecision
            || matches!(
                event.payload["point"].as_str(),
                Some("pre_tool" | "post_tool")
            )
    };
    let positions = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            (matches!(
                &event.kind,
                EventKind::HookDecision | EventKind::PolicyDecision | EventKind::ToolObs
            ) && tool_point(event))
            .then_some((index, event.kind.as_str()))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        positions.iter().map(|(_, kind)| *kind).collect::<Vec<_>>(),
        vec![
            "hook.decision",
            "policy.decision",
            "hook.decision",
            "tool.observation"
        ]
    );
}
