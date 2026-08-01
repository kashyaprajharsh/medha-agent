//! Regression coverage for mutation commit ordering (AUD-003).
//!
//! The event log is the replay authority. If two same-turn mutations execute
//! concurrently, the slower first call can commit last while observations are
//! still logged in model-request order. These tests force that reversed
//! completion shape and assert that live state and durable replay agree.

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::{
    AllowAll, AutoDeny, BlastRadius, Block, Budget, CompileResult, CompiledContext, ContextEngine,
    EventKind, EventLog, Executor, InMemoryLog, Kernel, Message, NoVerify, Observation, Provider,
    ProviderCaps, ProviderError, Role, Session, StopReason, ToolCallStrategy, ToolIntent, ToolSpec,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct ScriptedProvider {
    caps: ProviderCaps,
    turns: Mutex<VecDeque<Vec<Block>>>,
}

impl ScriptedProvider {
    fn new(turns: Vec<Vec<Block>>) -> Self {
        Self {
            caps: ProviderCaps {
                vision: false,
                caching: false,
                max_ctx: None,
                tool_calls: ToolCallStrategy::Native,
            },
            turns: Mutex::new(turns.into()),
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    async fn stream(
        &self,
        _ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        let blocks = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![Block::Text("done".into())]);
        Ok(stream::iter(blocks.into_iter().map(Ok)).boxed())
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

fn intent(id: &str, tool: &str, args: Value) -> Block {
    Block::ToolIntent(ToolIntent {
        id: id.into(),
        tool: tool.into(),
        args,
    })
}

async fn run<E: Executor + 'static>(executor: Arc<E>) -> (Arc<InMemoryLog>, Session, Vec<Message>) {
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            intent("first", "state.write", json!({ "value": "first" })),
            intent("second", "state.write", json!({ "value": "second" })),
        ],
        vec![Block::Text("done".into())],
    ]));
    let log = Arc::new(InMemoryLog::new());
    let kernel = Kernel::new(
        provider,
        log.clone(),
        executor,
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    );
    let session = Session::new();
    let (messages, reason) = kernel
        .run_session(
            &session,
            vec![Message::user("mutate")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(reason, StopReason::Finished);
    (log, session, messages)
}

struct FileLikeExecutor {
    state: Mutex<Option<String>>,
}

#[async_trait]
impl Executor for FileLikeExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn blast_radius(&self, tool: &str) -> Option<BlastRadius> {
        (tool == "state.write").then_some(BlastRadius::ReversibleLocal)
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        // If these calls overlap, "second" commits first and the slower
        // request-order predecessor incorrectly wins last.
        if intent.id == "first" {
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        let mut state = self.state.lock().unwrap();
        let snapshot = state.clone();
        let value = intent.args["value"].as_str().unwrap().to_string();
        *state = Some(value);
        Observation::ok(
            &intent.id,
            json!({
                "path": "same-state",
                "snapshot": snapshot,
            }),
        )
    }
}

#[tokio::test]
async fn conflicting_mutations_commit_and_log_in_request_order() {
    let executor = Arc::new(FileLikeExecutor {
        state: Mutex::new(None),
    });
    let (log, session, _messages) = run(executor.clone()).await;

    assert_eq!(
        executor.state.lock().unwrap().as_deref(),
        Some("second"),
        "live state must reflect request/log order"
    );

    let events = log.events(session.id).await;
    let observations: Vec<_> = events
        .iter()
        .filter(|event| event.kind == EventKind::ToolObs)
        .collect();
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].payload["intent_id"], "first");
    assert!(
        observations[0].payload["payload"]["snapshot"].is_null(),
        "the first logged mutation must also be the first commit"
    );
    assert_eq!(observations[1].payload["intent_id"], "second");
    assert_eq!(
        observations[1].payload["payload"]["snapshot"],
        json!("first")
    );
}

#[derive(Default)]
struct MemoryState {
    version: u64,
    value: String,
}

struct MemoryLikeExecutor {
    state: Mutex<MemoryState>,
}

#[async_trait]
impl Executor for MemoryLikeExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn blast_radius(&self, tool: &str) -> Option<BlastRadius> {
        (tool == "memory.update").then_some(BlastRadius::Read)
    }

    fn mutation_key(&self, intent: &ToolIntent) -> Option<String> {
        Some(format!(
            "memory:{}:{}",
            intent.args["scope"].as_str().unwrap_or("project"),
            intent.args["name"].as_str().unwrap_or("*")
        ))
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        if intent.id == "first" {
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        let mut state = self.state.lock().unwrap();
        state.version += 1;
        state.value = intent.args["value"].as_str().unwrap().to_string();
        Observation::ok(
            &intent.id,
            json!({
                "applied": {
                    "op": "update",
                    "name": "shared",
                    "version": state.version,
                    "value": state.value,
                }
            }),
        )
    }
}

#[tokio::test]
async fn read_radius_memory_mutations_are_serialized_and_replay_identically() {
    let executor = Arc::new(MemoryLikeExecutor {
        state: Mutex::new(MemoryState::default()),
    });
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            intent(
                "first",
                "memory.update",
                json!({ "scope": "project", "name": "shared", "value": "first" }),
            ),
            intent(
                "second",
                "memory.update",
                json!({ "scope": "project", "name": "shared", "value": "second" }),
            ),
        ],
        vec![Block::Text("done".into())],
    ]));
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
    let session = Session::new();
    let (_messages, reason) = kernel
        .run_session(
            &session,
            vec![Message::user("remember twice")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(reason, StopReason::Finished);

    {
        let live = executor.state.lock().unwrap();
        assert_eq!(live.version, 2);
        assert_eq!(live.value, "second");
    }

    let events = log.events(session.id).await;
    let durable: Vec<_> = events
        .iter()
        .filter(|event| event.kind == EventKind::MemoryWrite)
        .map(|event| event.payload.clone())
        .collect();
    assert_eq!(durable.len(), 2);
    assert_eq!(durable[0]["version"], 1);
    assert_eq!(durable[0]["value"], "first");
    assert_eq!(durable[1]["version"], 2);
    assert_eq!(durable[1]["value"], "second");
}

struct ContextDrivenProvider {
    caps: ProviderCaps,
}

impl ContextDrivenProvider {
    fn new() -> Self {
        Self {
            caps: ProviderCaps {
                vision: false,
                caching: false,
                max_ctx: None,
                tool_calls: ToolCallStrategy::Native,
            },
        }
    }
}

#[async_trait]
impl Provider for ContextDrivenProvider {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    async fn stream(
        &self,
        ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        if ctx
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            return Ok(stream::iter(vec![Ok(Block::Text("done".into()))]).boxed());
        }
        let value = ctx
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| message.content.clone())
            .unwrap_or_default();
        Ok(stream::iter(vec![Ok(intent(
            &value,
            "memory.update",
            json!({ "scope": "project", "name": "shared", "value": value }),
        ))])
        .boxed())
    }
}

struct CrossSessionMemoryExecutor {
    state: Mutex<MemoryState>,
    first_started: tokio::sync::Notify,
}

#[async_trait]
impl Executor for CrossSessionMemoryExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn blast_radius(&self, tool: &str) -> Option<BlastRadius> {
        (tool == "memory.update").then_some(BlastRadius::Read)
    }

    fn mutation_key(&self, _intent: &ToolIntent) -> Option<String> {
        Some("memory:project:shared".into())
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        // Deliberately split read from write. Without a kernel-wide mutation
        // guard, both sessions can observe version 0 and publish version 1.
        let observed_version = self.state.lock().unwrap().version;
        if intent.id == "first" {
            self.first_started.notify_one();
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        let mut state = self.state.lock().unwrap();
        state.version = observed_version + 1;
        state.value = intent.args["value"].as_str().unwrap().to_string();
        Observation::ok(
            &intent.id,
            json!({
                "applied": {
                    "op": "update",
                    "name": "shared",
                    "version": state.version,
                    "value": state.value,
                }
            }),
        )
    }
}

#[tokio::test]
async fn competing_sessions_cannot_lose_a_same_key_memory_update() {
    let executor = Arc::new(CrossSessionMemoryExecutor {
        state: Mutex::new(MemoryState::default()),
        first_started: tokio::sync::Notify::new(),
    });
    let log = Arc::new(InMemoryLog::new());
    let kernel = Arc::new(Kernel::new(
        Arc::new(ContextDrivenProvider::new()),
        log.clone(),
        executor.clone(),
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    ));
    let first_session = Session::new();
    let second_session = Session::new();

    let first_kernel = kernel.clone();
    let first = tokio::spawn(async move {
        first_kernel
            .run_session(
                &first_session,
                vec![Message::user("first")],
                Budget::default(),
                &kernel::NullSink,
                None,
            )
            .await
            .unwrap();
        first_session
    });
    executor.first_started.notified().await;
    let second_kernel = kernel.clone();
    let second = tokio::spawn(async move {
        second_kernel
            .run_session(
                &second_session,
                vec![Message::user("second")],
                Budget::default(),
                &kernel::NullSink,
                None,
            )
            .await
            .unwrap();
        second_session
    });
    let (first_session, second_session) = tokio::join!(first, second);
    let first_session = first_session.unwrap();
    let second_session = second_session.unwrap();

    {
        let live = executor.state.lock().unwrap();
        assert_eq!(live.version, 2, "neither update may be lost");
        assert_eq!(live.value, "second");
    }
    let mut versions = Vec::new();
    for session in [first_session, second_session] {
        versions.extend(
            log.events(session.id)
                .await
                .into_iter()
                .filter(|event| event.kind == EventKind::MemoryWrite)
                .map(|event| event.payload["version"].as_u64().unwrap()),
        );
    }
    versions.sort_unstable();
    assert_eq!(versions, vec![1, 2]);
}

/// A provider that makes the root mutate and then wait in one emitted batch,
/// while the derived child must mutate before that wait can finish.
struct ParentWaitProvider {
    caps: ProviderCaps,
}

impl ParentWaitProvider {
    fn new() -> Self {
        Self {
            caps: ProviderCaps {
                vision: false,
                caching: false,
                max_ctx: None,
                tool_calls: ToolCallStrategy::Native,
            },
        }
    }
}

#[async_trait]
impl Provider for ParentWaitProvider {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    async fn stream(
        &self,
        ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        if ctx
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            return Ok(stream::iter(vec![Ok(Block::Text("done".into()))]).boxed());
        }
        let caller = ctx
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| message.content.as_str())
            .unwrap_or_default();
        let calls = match caller {
            "parent" => vec![
                intent("parent-write", "parent.mutate", json!({})),
                intent("wait-child", "child.wait", json!({})),
            ],
            "child" => vec![intent("child-write", "child.mutate", json!({}))],
            _ => vec![Block::Text("done".into())],
        };
        Ok(stream::iter(calls.into_iter().map(Ok)).boxed())
    }
}

struct ParentWaitExecutor {
    parent_started: tokio::sync::Notify,
    child_finished: tokio::sync::Semaphore,
}

#[async_trait]
impl Executor for ParentWaitExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn blast_radius(&self, tool: &str) -> Option<BlastRadius> {
        match tool {
            "parent.mutate" | "child.mutate" => Some(BlastRadius::ReversibleLocal),
            "child.wait" => Some(BlastRadius::Read),
            _ => None,
        }
    }

    fn mutation_key(&self, intent: &ToolIntent) -> Option<String> {
        match intent.tool.as_str() {
            "parent.mutate" | "child.mutate" => Some("state:*".into()),
            _ => None,
        }
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        match intent.tool.as_str() {
            "parent.mutate" => {
                // Let the test launch the child only after the parent has
                // definitely acquired and entered its mutation.
                self.parent_started.notify_one();
                Observation::ok(&intent.id, json!({ "mutated": "parent" }))
            }
            "child.mutate" => {
                self.child_finished.add_permits(1);
                Observation::ok(&intent.id, json!({ "mutated": "child" }))
            }
            "child.wait" => {
                let permit = self
                    .child_finished
                    .acquire()
                    .await
                    .expect("test semaphore stays open");
                permit.forget();
                Observation::ok(&intent.id, json!({ "child": "finished" }))
            }
            _ => Observation::error(&intent.id, "unknown test tool"),
        }
    }
}

#[tokio::test]
async fn parent_wait_does_not_hold_the_mutation_lane_needed_by_its_child() {
    let executor = Arc::new(ParentWaitExecutor {
        parent_started: tokio::sync::Notify::new(),
        child_finished: tokio::sync::Semaphore::new(0),
    });
    let log = Arc::new(InMemoryLog::new());
    let root = Arc::new(Kernel::new(
        Arc::new(ParentWaitProvider::new()),
        log.clone(),
        executor.clone(),
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    ));
    let child = root.derive(executor.clone(), Arc::new(Passthrough), Arc::new(AutoDeny));
    let parent_session = Session::new();
    let child_session = Session::new();

    // Register the notification before spawning so the fast parent mutation
    // cannot signal between task creation and the await.
    let parent_started = executor.parent_started.notified();
    let parent_kernel = root.clone();
    let parent = tokio::spawn(async move {
        parent_kernel
            .run_session(
                &parent_session,
                vec![Message::user("parent")],
                Budget::default(),
                &kernel::NullSink,
                None,
            )
            .await
            .unwrap();
        parent_session
    });
    parent_started.await;
    let child = tokio::spawn(async move {
        child
            .run_session(
                &child_session,
                vec![Message::user("child")],
                Budget::default(),
                &kernel::NullSink,
                None,
            )
            .await
            .unwrap();
        child_session
    });

    let (parent_session, child_session) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(parent, child)
    })
    .await
    .expect("parent wait deadlocked with the child's mutation");
    let parent_session = parent_session.unwrap();
    let child_session = child_session.unwrap();

    let parent_observations = log
        .events(parent_session.id)
        .await
        .into_iter()
        .filter(|event| event.kind == EventKind::ToolObs)
        .count();
    let child_observations = log
        .events(child_session.id)
        .await
        .into_iter()
        .filter(|event| event.kind == EventKind::ToolObs)
        .count();
    assert_eq!(parent_observations, 2);
    assert_eq!(child_observations, 1);
}
