//! End-to-end tests for kernel interrupts (design: MEDHA_INTERRUPTS_DESIGN.md):
//! graceful cancel (every admitted intent gets an observation), steer at turn
//! boundaries, partial-stream preservation, steers returned on cancel, and the
//! intent → decision → observation log-adjacency invariant.

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::{
    AllowAll, AutoDeny, Block, Budget, CompileResult, CompiledContext, ContextEngine, Event,
    EventKind, EventLog, Executor, InMemoryLog, InterruptQueue, Kernel, Message, NoVerify,
    ObsStatus, Observation, Provider, ProviderCaps, ProviderError, Role, Session, StopReason,
    StreamSink, ToolCallStrategy, ToolIntent, ToolSpec,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ── fixtures ─────────────────────────────────────────────────────────────────

enum Turn {
    /// Emit these blocks, then end the stream.
    Blocks(Vec<Block>),
    /// Emit this text, then hang forever (a stream that never finishes).
    TextThenHang(String),
    /// Hang in `stream()` itself — no first byte ever arrives (models with
    /// long prompt processing look exactly like this from the kernel's side).
    HangBeforeFirstByte,
}

struct ScriptedProvider {
    caps: ProviderCaps,
    turns: Mutex<VecDeque<Turn>>,
}

impl ScriptedProvider {
    fn new(turns: Vec<Turn>) -> Self {
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
        let turn = self.turns.lock().unwrap().pop_front();
        match turn {
            Some(Turn::Blocks(blocks)) => Ok(stream::iter(blocks.into_iter().map(Ok)).boxed()),
            Some(Turn::TextThenHang(t)) => Ok(stream::iter(vec![Ok(Block::Text(t))])
                .chain(stream::pending())
                .boxed()),
            Some(Turn::HangBeforeFirstByte) => futures::future::pending().await,
            None => Ok(stream::iter(vec![Ok(Block::Text("(script exhausted)".into()))]).boxed()),
        }
    }
}

/// Executes every tool by sleeping `delay`, then returning ok.
struct SleepyExecutor {
    delay: Duration,
}

#[async_trait]
impl Executor for SleepyExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    async fn execute(&self, intent: &ToolIntent) -> Observation {
        tokio::time::sleep(self.delay).await;
        Observation {
            intent_id: intent.id.clone(),
            status: ObsStatus::Ok,
            payload: json!({ "ok": true, "tool": intent.tool }),
            relayed_trust: None,
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

/// Sink that records returned steers and applied steers.
#[derive(Clone, Default)]
struct CaptureSink {
    returned: Arc<Mutex<Vec<String>>>,
    steered: Arc<Mutex<Vec<String>>>,
}

impl StreamSink for CaptureSink {
    fn steered(&self, text: &str) {
        self.steered.lock().unwrap().push(text.to_string());
    }
    fn steers_returned(&self, texts: &[String]) {
        self.returned.lock().unwrap().extend(texts.iter().cloned());
    }
}

fn intent_block(id: &str, tool: &str) -> Block {
    Block::ToolIntent(ToolIntent {
        id: id.into(),
        tool: tool.into(),
        args: json!({}),
    })
}

#[allow(clippy::type_complexity)]
fn kernel_with(
    turns: Vec<Turn>,
    tool_delay: Duration,
    grace: Duration,
) -> (Arc<Kernel<ScriptedProvider, InMemoryLog>>, Arc<InMemoryLog>) {
    let log = Arc::new(InMemoryLog::new());
    let k = Kernel::new(
        Arc::new(ScriptedProvider::new(turns)),
        log.clone(),
        Arc::new(SleepyExecutor { delay: tool_delay }),
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    )
    .with_settle_grace(grace);
    (Arc::new(k), log)
}

fn ids_of(events: &[Event], kind: EventKind) -> Vec<Value> {
    events
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload.clone())
        .collect()
}

// ── tests ────────────────────────────────────────────────────────────────────

/// Esc while the model is still "thinking" — i.e. the HTTP stream is open but
/// no first byte has arrived (big models spend minutes in prompt processing).
/// The cancel must abort the connect wait itself, not just an open stream.
#[tokio::test]
async fn cancel_aborts_a_stream_still_connecting() {
    let (kernel, log) = kernel_with(
        vec![Turn::HangBeforeFirstByte],
        Duration::from_secs(60),
        Duration::from_millis(100),
    );
    let session = Session::new();
    let (handle, queue) = InterruptQueue::pair();
    let sink = CaptureSink::default();

    let k = kernel.clone();
    let s = session.clone();
    let task = tokio::spawn(async move {
        k.run_session(
            &s,
            vec![Message::user("go")],
            Budget::default(),
            &sink,
            Some(queue),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(200)).await; // connecting…
    let started = std::time::Instant::now();
    handle.cancel_turn();
    let (_messages, reason) = task.await.unwrap().unwrap();

    assert_eq!(reason, StopReason::Interrupted);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "Esc during connect must stop promptly, took {:?}",
        started.elapsed()
    );
    // Nothing streamed → no dangling intents or observations in the log.
    let events = log.events(session.id).await;
    assert_eq!(ids_of(&events, EventKind::ModelIntent).len(), 0);
    assert_eq!(ids_of(&events, EventKind::ToolObs).len(), 0);
}

#[tokio::test]
async fn cancel_settles_inflight_tool_and_leaves_no_dangling_intent() {
    // Turn 1 calls a tool that sleeps far longer than the grace window.
    let (kernel, log) = kernel_with(
        vec![Turn::Blocks(vec![intent_block("i1", "slow.tool")])],
        Duration::from_secs(60),
        Duration::from_millis(100),
    );
    let session = Session::new();
    let (handle, queue) = InterruptQueue::pair();
    let sink = CaptureSink::default();

    let k = kernel.clone();
    let s = session.clone();
    let task = tokio::spawn(async move {
        k.run_session(
            &s,
            vec![Message::user("go")],
            Budget::default(),
            &sink,
            Some(queue),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(300)).await; // tool is in flight
    let started = std::time::Instant::now();
    handle.cancel_turn();
    let (messages, reason) = task.await.unwrap().unwrap();

    assert_eq!(reason, StopReason::Interrupted);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cancel must return promptly (grace-bounded), took {:?}",
        started.elapsed()
    );
    // The admitted intent got a (synthesized) observation — in the log AND in
    // the returned history, so nothing dangles for resume or for a next send.
    let events = log.events(session.id).await;
    let intents = ids_of(&events, EventKind::ModelIntent);
    let obs = ids_of(&events, EventKind::ToolObs);
    assert_eq!(intents.len(), 1);
    assert_eq!(obs.len(), 1, "every admitted intent gets an observation");
    assert!(
        obs[0].to_string().contains("interrupted"),
        "synthesized: {}",
        obs[0]
    );
    let tool_msgs: Vec<_> = messages.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(tool_msgs.len(), 1);
    assert_eq!(tool_msgs[0].tool_call_id.as_deref(), Some("i1"));
}

#[tokio::test]
async fn steer_lands_at_the_next_turn_boundary() {
    // Turn 1 runs a shortish tool; the steer arrives while it runs; turn 2 ends.
    let (kernel, log) = kernel_with(
        vec![
            Turn::Blocks(vec![intent_block("i1", "fast.tool")]),
            Turn::Blocks(vec![Block::Text("done".into())]),
        ],
        Duration::from_millis(400),
        Duration::from_secs(5),
    );
    let session = Session::new();
    let (handle, queue) = InterruptQueue::pair();
    let sink = CaptureSink::default();
    let steered = sink.steered.clone();

    let k = kernel.clone();
    let s = session.clone();
    let task = tokio::spawn(async move {
        k.run_session(
            &s,
            vec![Message::user("go")],
            Budget::default(),
            &sink,
            Some(queue),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await; // mid turn-1
    handle.steer("also check the tests");
    let (messages, reason) = task.await.unwrap().unwrap();

    assert_eq!(reason, StopReason::Finished);
    assert_eq!(
        *steered.lock().unwrap(),
        vec!["also check the tests".to_string()]
    );
    // The steer is a real user message BETWEEN turn 1's results and turn 2.
    let pos_steer = messages
        .iter()
        .position(|m| m.role == Role::User && m.content == "also check the tests")
        .expect("steer message in history");
    let pos_done = messages.iter().position(|m| m.content == "done").unwrap();
    assert!(
        pos_steer < pos_done,
        "steer precedes the following turn's answer"
    );
    // And it's logged as a plain user.message → resume needs no special casing.
    let events = log.events(session.id).await;
    assert!(
        events.iter().any(|e| e.kind == EventKind::UserMessage
            && e.payload["text"] == json!("also check the tests"))
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::Interrupt && e.payload["kind"] == json!("steer"))
    );
}

#[tokio::test]
async fn cancel_mid_stream_keeps_partial_text_and_drops_intents() {
    let (kernel, log) = kernel_with(
        vec![Turn::TextThenHang("partial answer".into())],
        Duration::from_millis(10),
        Duration::from_millis(100),
    );
    let session = Session::new();
    let (handle, queue) = InterruptQueue::pair();
    let sink = CaptureSink::default();

    let k = kernel.clone();
    let s = session.clone();
    let task = tokio::spawn(async move {
        k.run_session(
            &s,
            vec![Message::user("go")],
            Budget::default(),
            &sink,
            Some(queue),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(200)).await; // text streamed, now hanging
    handle.cancel_turn();
    let (messages, reason) = task.await.unwrap().unwrap();

    assert_eq!(reason, StopReason::Interrupted);
    let last = messages.last().unwrap();
    assert_eq!(last.role, Role::Assistant);
    assert_eq!(last.content, "partial answer", "what streamed is kept");
    assert!(
        last.tool_calls.is_empty(),
        "no dangling tool_calls on the partial turn"
    );
    let events = log.events(session.id).await;
    assert!(events.iter().any(|e| e.kind == EventKind::ModelText));
    assert!(
        !events.iter().any(|e| e.kind == EventKind::ModelIntent),
        "un-admitted intents must not be logged"
    );
}

#[tokio::test]
async fn steer_then_cancel_returns_the_text_instead_of_losing_it() {
    let (kernel, _log) = kernel_with(
        vec![Turn::Blocks(vec![intent_block("i1", "slow.tool")])],
        Duration::from_secs(60),
        Duration::from_millis(100),
    );
    let session = Session::new();
    let (handle, queue) = InterruptQueue::pair();
    let sink = CaptureSink::default();
    let returned = sink.returned.clone();

    let k = kernel.clone();
    let s = session.clone();
    let task = tokio::spawn(async move {
        k.run_session(
            &s,
            vec![Message::user("go")],
            Budget::default(),
            &sink,
            Some(queue),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.steer("wait, do Y instead");
    handle.cancel_turn(); // before any turn boundary could drain it
    let (messages, reason) = task.await.unwrap().unwrap();

    assert_eq!(reason, StopReason::Interrupted);
    assert_eq!(
        *returned.lock().unwrap(),
        vec!["wait, do Y instead".to_string()]
    );
    assert!(
        !messages.iter().any(|m| m.content == "wait, do Y instead"),
        "an unapplied steer must not sit in the history"
    );
}

#[tokio::test]
async fn intent_decision_observation_stay_adjacent_per_id() {
    // Two tools in one turn; verify per-id event order in the log.
    let (kernel, log) = kernel_with(
        vec![
            Turn::Blocks(vec![intent_block("a", "t1"), intent_block("b", "t2")]),
            Turn::Blocks(vec![Block::Text("done".into())]),
        ],
        Duration::from_millis(50),
        Duration::from_secs(5),
    );
    let session = Session::new();
    let sink = CaptureSink::default();
    let (_h, queue) = InterruptQueue::pair();
    let (_msgs, reason) = kernel
        .run_session(
            &session,
            vec![Message::user("go")],
            Budget::default(),
            &sink,
            Some(queue),
        )
        .await
        .unwrap();
    assert_eq!(reason, StopReason::Finished);

    let events = log.events(session.id).await;
    for id in ["a", "b"] {
        let pos = |kind: EventKind, key: &str| {
            events
                .iter()
                .position(|e| e.kind == kind && e.payload[key] == json!(id))
        };
        let i = pos(EventKind::ModelIntent, "id").expect("intent logged");
        let d = pos(EventKind::PolicyDecision, "intent_id").expect("decision logged");
        let o = events
            .iter()
            .position(|e| e.kind == EventKind::ToolObs && e.payload["intent_id"] == json!(id))
            .expect("observation logged");
        assert!(
            i < d && d < o,
            "order for {id}: intent({i}) → decision({d}) → obs({o})"
        );
    }
}

#[tokio::test]
async fn headless_without_a_queue_finishes_exactly_as_before() {
    let (kernel, _log) = kernel_with(
        vec![
            Turn::Blocks(vec![intent_block("i1", "fast.tool")]),
            Turn::Blocks(vec![Block::Text("done".into())]),
        ],
        Duration::from_millis(20),
        Duration::from_secs(5),
    );
    let session = Session::new();
    let (msgs, reason) = kernel
        .run_session(
            &session,
            vec![Message::user("go")],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(reason, StopReason::Finished);
    assert_eq!(msgs.last().unwrap().content, "done");
}
