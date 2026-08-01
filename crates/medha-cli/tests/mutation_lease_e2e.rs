//! Cross-connection regression for AUD-003.
//!
//! These kernels are independently constructed, as two MEDHA processes would
//! be: they do not share the kernel's in-memory mutation mutex and each owns a
//! separate `SqliteLog` connection. Only the durable SQLite mutation lease can
//! prevent the forced read/modify/write race below.

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::{
    AllowAll, AutoDeny, BlastRadius, Block, Budget, CompileResult, CompiledContext, ContextEngine,
    EventKind, Executor, Kernel, Message, NoVerify, Observation, Provider, ProviderCaps,
    ProviderError, Role, Session, ToolCallStrategy, ToolIntent, ToolSpec,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct OneMutationProvider {
    caps: ProviderCaps,
    value: &'static str,
}

impl OneMutationProvider {
    fn new(value: &'static str) -> Self {
        Self {
            caps: ProviderCaps {
                vision: false,
                caching: false,
                max_ctx: None,
                tool_calls: ToolCallStrategy::Native,
            },
            value,
        }
    }
}

#[async_trait]
impl Provider for OneMutationProvider {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    async fn stream(
        &self,
        ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        let block = if ctx
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            Block::Text("done".into())
        } else {
            Block::ToolIntent(ToolIntent {
                id: self.value.into(),
                tool: "memory.update".into(),
                args: json!({
                    "scope": "project",
                    "name": "shared",
                    "value": self.value,
                }),
            })
        };
        Ok(stream::iter(vec![Ok(block)]).boxed())
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

#[derive(Default)]
struct SharedState {
    version: u64,
    value: String,
}

struct RacyMemoryExecutor {
    state: Arc<Mutex<SharedState>>,
    first_started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Executor for RacyMemoryExecutor {
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
        // Deliberately split read from write. Without the durable lease, the
        // second kernel reads version 0 while the first sleeps, writes v1, and
        // then the first overwrites it with another v1.
        let observed = self.state.lock().unwrap().version;
        if intent.id == "first" {
            self.first_started.notify_one();
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let mut state = self.state.lock().unwrap();
        state.version = observed + 1;
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

fn kernel(
    provider: Arc<OneMutationProvider>,
    log: Arc<store::SqliteLog>,
    executor: Arc<RacyMemoryExecutor>,
) -> Kernel<OneMutationProvider, store::SqliteLog> {
    Kernel::new(
        provider,
        log,
        executor,
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_kernels_and_sqlite_connections_cannot_lose_an_update() {
    let dir = std::env::temp_dir().join(format!("medha-cross-process-{}", ulid::Ulid::new()));
    let db = dir.join("events.db");
    let state = Arc::new(Mutex::new(SharedState::default()));
    let first_started = Arc::new(tokio::sync::Notify::new());
    let executor = Arc::new(RacyMemoryExecutor {
        state: state.clone(),
        first_started: first_started.clone(),
    });

    // Separate log handles and separate kernels: no shared in-memory scheduler.
    let first_log = Arc::new(store::SqliteLog::open(&db).unwrap());
    let second_log = Arc::new(store::SqliteLog::open(&db).unwrap());
    let first_kernel = kernel(
        Arc::new(OneMutationProvider::new("first")),
        first_log.clone(),
        executor.clone(),
    );
    let second_kernel = kernel(
        Arc::new(OneMutationProvider::new("second")),
        second_log,
        executor.clone(),
    );
    let first_session = Session::new();
    let second_session = Session::new();

    let started = first_started.notified();
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
    });
    started.await;
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
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        first.await.unwrap();
        second.await.unwrap();
    })
    .await
    .expect("both independently scheduled kernels must settle");

    {
        let state = state.lock().unwrap();
        assert_eq!(state.version, 2, "one read/modify/write was lost");
        assert_eq!(state.value, "second");
    }
    let versions = first_log
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::MemoryWrite)
        .map(|event| event.payload["version"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(versions, vec![1, 2]);
    first_log.verify().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}
