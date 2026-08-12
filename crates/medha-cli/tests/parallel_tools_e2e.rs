//! Does a batch of parallel read tools settle on its own?
//!
//! A live session showed five intents, three policy decisions and no
//! observations, followed by minutes of silence. That is the signature of a
//! wedged batch — and equally the signature of a force-abort, which drops the
//! in-flight futures before they can be observed. The event log cannot tell the
//! two apart, so this runs the batch with nobody interrupting it.

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::{
    AllowAll, AutoDeny, BlastRadius, Block, Budget, CompileResult, CompiledContext, ContextEngine,
    EventKind, EventLog, Executor, Kernel, Message, NoVerify, NullSink, Observation, Provider,
    ProviderCaps, ProviderError, Role, Session, ToolCallStrategy, ToolIntent, ToolSpec,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const FANOUT: usize = 20;

struct Batch {
    caps: ProviderCaps,
}

impl Batch {
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
impl Provider for Batch {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    async fn stream(
        &self,
        ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        // Once the results are in, stop — one batch is the whole experiment.
        if ctx.messages.iter().any(|m| m.role == Role::Tool) {
            return Ok(stream::iter(vec![Ok(Block::Text("done".into()))]).boxed());
        }
        let blocks: Vec<Result<Block, ProviderError>> = (0..FANOUT)
            .map(|n| {
                Ok(Block::ToolIntent(ToolIntent {
                    id: format!("read-{n}"),
                    tool: "fs.read".into(),
                    args: json!({ "path": format!("file-{n}.txt") }),
                }))
            })
            .collect();
        Ok(stream::iter(blocks).boxed())
    }
}

struct Reads;

#[async_trait]
impl Executor for Reads {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: String::new(),
            schema: json!({}),
            blast_radius: BlastRadius::Read,
            category: kernel::ToolCategory::Read,
            icon: "◇".into(),
        }]
    }

    fn blast_radius(&self, tool: &str) -> Option<BlastRadius> {
        (tool == "fs.read").then_some(BlastRadius::Read)
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        Observation::ok(&intent.id, json!({ "content": "hello" }))
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
        Ok("h".into())
    }
    fn get(&self, _hash: &str, _offset: usize, _len: Option<usize>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
    fn size(&self, _hash: &str) -> Result<usize, String> {
        Ok(0)
    }
}

/// Every admitted intent must reach an observation without anyone stopping it.
///
/// The store serializes each append behind a single permit, so a batch this wide
/// is also the cheapest check that the permit is always released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wide_batch_of_reads_settles_without_being_interrupted() {
    let dir = std::env::temp_dir().join(format!("medha-parallel-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = Arc::new(store::SqliteLog::open(dir.join("events.db")).unwrap());
    let kernel = Kernel::new(
        Arc::new(Batch::new()),
        log.clone(),
        Arc::new(Reads),
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    );
    let session = Session::new();

    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        kernel.run_session(
            &session,
            vec![Message::user("read them all")],
            Budget::turns(2),
            &NullSink,
            None,
        ),
    )
    .await;

    let events = log.events(session.id).await;
    let count = |kind: EventKind| events.iter().filter(|e| e.kind == kind).count();
    assert!(
        outcome.is_ok(),
        "the turn never finished: {} intents, {} decisions, {} observations",
        count(EventKind::ModelIntent),
        count(EventKind::PolicyDecision),
        count(EventKind::ToolObs),
    );
    outcome.unwrap().expect("the session itself failed");

    assert_eq!(count(EventKind::ModelIntent), FANOUT);
    assert_eq!(
        count(EventKind::PolicyDecision),
        FANOUT,
        "a decision short of the batch means the append lane stopped granting"
    );
    assert_eq!(
        count(EventKind::ToolObs),
        FANOUT,
        "every admitted intent owes an observation"
    );
}
