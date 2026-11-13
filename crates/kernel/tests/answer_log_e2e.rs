//! An approval's answer is logged before what it allows runs, so the call never waits for the log behind its own answer.

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::{
    Approval, Block, Budget, CompileResult, CompiledContext, ContextEngine, Decision, Event,
    EventKind, EventLog, Executor, HumanGate, InMemoryLog, Kernel, KernelError, Message, NoVerify,
    Observation, Policy, Provider, ProviderCaps, ProviderError, RecordedGate, Session, StopReason,
    ToolCallStrategy, ToolIntent, ToolSpec,
};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

/// Appends one at a time through a single permit handed out first come, first served, as the SQLite log does.
struct OneWriterLog {
    inner: InMemoryLog,
    writer: Arc<Semaphore>,
}

#[async_trait]
impl EventLog for OneWriterLog {
    async fn append(&self, event: Event) -> Result<Event, KernelError> {
        let _permit = self.writer.clone().acquire_owned().await.unwrap();
        self.inner.append(event).await
    }
    async fn events(&self, session: ulid::Ulid) -> Vec<Event> {
        self.inner.events(session).await
    }
}

struct Scripted(Mutex<VecDeque<Vec<Block>>>);

#[async_trait]
impl Provider for Scripted {
    fn capabilities(&self) -> &ProviderCaps {
        static CAPS: ProviderCaps = ProviderCaps {
            images: kernel::ImageSupport::Unsupported,
            caching: false,
            max_ctx: None,
            tool_calls: ToolCallStrategy::Native,
        };
        &CAPS
    }
    async fn stream(
        &self,
        _ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        let blocks = self
            .0
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

struct NoArtifacts;

impl kernel::ArtifactStore for NoArtifacts {
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

/// Every call asks the person.
struct AlwaysAsk;

impl Policy for AlwaysAsk {
    fn authorize(
        &self,
        _autonomy: kernel::AutonomyLevel,
        _intent: &ToolIntent,
        _blast_radius: Option<kernel::BlastRadius>,
    ) -> Decision {
        Decision::Human
    }
}

struct Approves;

#[async_trait]
impl HumanGate for Approves {
    async fn confirm(&self, _action: &str, _detail: Option<&str>, _escalated: bool) -> Approval {
        Approval::Once
    }
}

/// Like a sub-agent dispatch: writes to the log while another writer, a running child, briefly holds it.
struct WritesWhileBusy {
    log: Arc<OneWriterLog>,
    session: Session,
}

#[async_trait]
impl Executor for WritesWhileBusy {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "agent.spawn".into(),
            description: String::new(),
            schema: json!({}),
            blast_radius: kernel::BlastRadius::ReversibleLocal,
            category: kernel::ToolCategory::Other,
            icon: String::new(),
        }]
    }
    fn blast_radius(&self, _tool: &str) -> Option<kernel::BlastRadius> {
        Some(kernel::BlastRadius::ReversibleLocal)
    }
    async fn execute(&self, intent: &ToolIntent) -> Observation {
        let other = self.log.writer.clone().try_acquire_owned().unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(other);
        });
        self.log
            .append(Event::model_text(&self.session, "dispatched"))
            .await
            .unwrap();
        Observation::ok(&intent.id, json!({"started": true}))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approved_call_that_writes_to_a_busy_log_still_finishes() {
    let log = Arc::new(OneWriterLog {
        inner: InMemoryLog::new(),
        writer: Arc::new(Semaphore::new(1)),
    });
    let session = Session::new();
    let provider = Arc::new(Scripted(Mutex::new(
        vec![
            vec![Block::ToolIntent(ToolIntent {
                id: "spawn".into(),
                tool: "agent.spawn".into(),
                args: json!({}),
            })],
            vec![Block::Text("done".into())],
        ]
        .into(),
    )));
    let kernel = Kernel::new(
        provider,
        log.clone(),
        Arc::new(WritesWhileBusy {
            log: log.clone(),
            session: session.clone(),
        }),
        Arc::new(Passthrough),
        Arc::new(NoArtifacts),
        Arc::new(AlwaysAsk),
        Arc::new(RecordedGate::new(Arc::new(Approves))),
        Arc::new(NoVerify),
    );
    let ran = tokio::time::timeout(
        Duration::from_secs(10),
        kernel.run_session(
            &session,
            vec![Message::user("start an agent")],
            Budget::default(),
            &kernel::NullSink,
            None,
        ),
    )
    .await
    .expect("the call waited forever for the log behind its own approval answer");
    assert_eq!(ran.unwrap().1, StopReason::Finished);
    let events = log.events(session.id).await;
    let answer = events
        .iter()
        .position(|event| event.kind == EventKind::ApprovalDecision);
    let write = events
        .iter()
        .position(|event| event.payload["text"] == "dispatched");
    assert!(
        answer.is_some() && answer < write,
        "the answer was not logged before what it allowed ran: {:?}",
        events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>()
    );
}
