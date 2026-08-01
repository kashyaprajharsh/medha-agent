//! Fail-closed policy logging and mutation outbox regressions (AUD-011).

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::{
    AllowAll, AutoDeny, Block, Budget, CompileResult, CompiledContext, ContextEngine, Event,
    EventKind, EventLog, Executor, InMemoryLog, Kernel, KernelError, Message, MutationLease,
    NoVerify, Observation, Provider, ProviderCaps, ProviderError, Session, ToolCallStrategy,
    ToolIntent, ToolSpec,
};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use ulid::Ulid;

struct ScriptedProvider {
    caps: ProviderCaps,
    turns: Mutex<VecDeque<Vec<Block>>>,
}

impl ScriptedProvider {
    fn new(tool: &str) -> Self {
        Self {
            caps: ProviderCaps {
                vision: false,
                caching: false,
                max_ctx: None,
                tool_calls: ToolCallStrategy::Native,
            },
            turns: Mutex::new(
                vec![
                    vec![Block::ToolIntent(ToolIntent {
                        id: "effect-1".into(),
                        tool: tool.into(),
                        args: json!({
                            "name": "durable-test",
                            "scope": "project",
                            "value": "new",
                        }),
                    })],
                    vec![Block::Text("done".into())],
                ]
                .into(),
            ),
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
        let blocks = self.turns.lock().unwrap().pop_front().unwrap_or_default();
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

struct Artifacts;

impl kernel::ArtifactStore for Artifacts {
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

struct CountingMutation {
    executions: AtomicUsize,
    memory: bool,
}

#[async_trait]
impl Executor for CountingMutation {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn mutation_key(&self, _intent: &ToolIntent) -> Option<String> {
        Some(if self.memory {
            "memory:project:durable-test".into()
        } else {
            "state:*".into()
        })
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        self.executions.fetch_add(1, Ordering::SeqCst);
        if self.memory {
            Observation::ok(
                &intent.id,
                json!({
                    "applied": {
                        "op": "update",
                        "entry": {
                            "name": "durable-test",
                            "value": "new",
                        },
                    },
                    "name": "durable-test",
                }),
            )
        } else {
            Observation::ok(&intent.id, json!({ "changed": true }))
        }
    }
}

struct FailOnceLog {
    inner: InMemoryLog,
    kind: EventKind,
    failed: AtomicBool,
}

impl FailOnceLog {
    fn new(kind: EventKind) -> Self {
        Self {
            inner: InMemoryLog::new(),
            kind,
            failed: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl EventLog for FailOnceLog {
    async fn append(&self, event: Event) -> Result<Event, KernelError> {
        if event.kind == self.kind && !self.failed.swap(true, Ordering::SeqCst) {
            return Err(KernelError::Log(format!(
                "injected {} append failure",
                event.kind.as_str()
            )));
        }
        self.inner.append(event).await
    }

    async fn events(&self, session: Ulid) -> Vec<Event> {
        self.inner.events(session).await
    }

    async fn acquire_mutation_lease(
        &self,
        _mutation_key: &str,
    ) -> Result<MutationLease, KernelError> {
        Ok(MutationLease::in_process())
    }
}

async fn run_case(
    fail_kind: EventKind,
    memory: bool,
) -> (
    Arc<FailOnceLog>,
    Arc<CountingMutation>,
    Session,
    Result<(), KernelError>,
) {
    let tool = if memory {
        "memory.update"
    } else {
        "state.write"
    };
    let log = Arc::new(FailOnceLog::new(fail_kind));
    let executor = Arc::new(CountingMutation {
        executions: AtomicUsize::new(0),
        memory,
    });
    let kernel = Kernel::new(
        Arc::new(ScriptedProvider::new(tool)),
        log.clone(),
        executor.clone(),
        Arc::new(Passthrough),
        Arc::new(Artifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    );
    let session = Session::new();
    let result = kernel
        .run_session(
            &session,
            vec![Message::user("perform one mutation")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .map(|_| ());
    (log, executor, session, result)
}

#[tokio::test]
async fn policy_append_failure_fails_closed_before_execution() {
    let (log, executor, session, result) = run_case(EventKind::PolicyDecision, false).await;
    assert!(
        result.is_ok(),
        "the denied execution is a settled tool result"
    );
    assert_eq!(executor.executions.load(Ordering::SeqCst), 0);
    let events = log.events(session.id).await;
    assert!(
        !events
            .iter()
            .any(|event| event.kind == EventKind::ToolEffectPrepared)
    );
}

#[tokio::test]
async fn outbox_append_failure_fails_closed_before_execution() {
    let (log, executor, session, result) = run_case(EventKind::ToolEffectPrepared, false).await;
    assert!(
        result.is_ok(),
        "the denied execution is a settled tool result"
    );
    assert_eq!(executor.executions.load(Ordering::SeqCst), 0);
    let events = log.events(session.id).await;
    assert!(
        events
            .iter()
            .any(|event| event.kind == EventKind::PolicyDecision)
    );
    assert!(
        !events
            .iter()
            .any(|event| event.kind == EventKind::ToolEffectPrepared)
    );
}

#[tokio::test]
async fn observation_append_failure_leaves_a_durable_uncertain_effect_record() {
    let (log, executor, session, result) = run_case(EventKind::ToolObs, false).await;
    assert!(
        result.is_err(),
        "a missing final observation must fail the turn"
    );
    assert_eq!(executor.executions.load(Ordering::SeqCst), 1);
    let events = log.events(session.id).await;
    let prepared = events
        .iter()
        .find(|event| event.kind == EventKind::ToolEffectPrepared)
        .expect("write-ahead effect marker");
    assert_eq!(prepared.payload["intent_id"], "effect-1");
    assert_eq!(prepared.payload["tool"], "state.write");
    assert!(!events.iter().any(|event| event.kind == EventKind::ToolObs));
}

#[tokio::test]
async fn memory_observation_failure_keeps_the_authoritative_projection_event() {
    let (log, executor, session, result) = run_case(EventKind::ToolObs, true).await;
    assert!(
        result.is_err(),
        "a missing final observation must fail the turn"
    );
    assert_eq!(executor.executions.load(Ordering::SeqCst), 1);
    let events = log.events(session.id).await;
    let memory = events
        .iter()
        .find(|event| event.kind == EventKind::MemoryWrite)
        .expect("the exact memory op must precede the fallible observation append");
    assert_eq!(memory.payload["entry"]["name"], "durable-test");
    assert!(!events.iter().any(|event| event.kind == EventKind::ToolObs));
}

#[tokio::test]
async fn memory_write_append_failure_keeps_the_pre_effect_outbox_and_no_false_success() {
    let (log, executor, session, result) = run_case(EventKind::MemoryWrite, true).await;
    assert!(
        result.is_err(),
        "a missing projection event must fail the turn"
    );
    assert_eq!(executor.executions.load(Ordering::SeqCst), 1);
    let events = log.events(session.id).await;
    let prepared = events
        .iter()
        .find(|event| event.kind == EventKind::ToolEffectPrepared)
        .expect("write-ahead effect marker");
    assert_eq!(prepared.payload["args"]["name"], "durable-test");
    assert!(
        !events
            .iter()
            .any(|event| event.kind == EventKind::MemoryWrite)
    );
    assert!(
        !events.iter().any(|event| event.kind == EventKind::ToolObs),
        "do not durably claim success when the authoritative memory event failed"
    );
}
