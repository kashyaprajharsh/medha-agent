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
                images: kernel::ImageSupport::Unsupported,
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
            media: Vec::new(),
            relayed_trust: None,
            net_denied: false,
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
    let (messages, reason) = task.await.unwrap().unwrap();

    assert_eq!(reason, StopReason::Interrupted);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "Esc during connect must stop promptly, took {:?}",
        started.elapsed()
    );
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, Role::User);
    let events = log.events(session.id).await;
    assert_eq!(ids_of(&events, EventKind::ModelMessage).len(), 0);
    assert_eq!(ids_of(&events, EventKind::ModelText).len(), 0);
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

/// Everything needs a decision, so every dispatched intent reaches the gate.
struct AlwaysHuman;
impl kernel::Policy for AlwaysHuman {
    fn authorize(
        &self,
        _autonomy: kernel::AutonomyLevel,
        _intent: &ToolIntent,
        _blast_radius: Option<kernel::BlastRadius>,
    ) -> kernel::Decision {
        kernel::Decision::Human
    }
}

/// Records the highest number of cards that were ever open at once. One surface
/// can only ask one question, so anything above 1 is a card racing another.
#[derive(Default)]
struct CountingGate {
    open: Mutex<usize>,
    most: Mutex<usize>,
    calls: Mutex<usize>,
}

#[async_trait]
impl kernel::HumanGate for CountingGate {
    async fn confirm(
        &self,
        _action: &str,
        _detail: Option<&str>,
        _escalated: bool,
    ) -> kernel::Approval {
        {
            let mut open = self.open.lock().unwrap();
            *open += 1;
            *self.calls.lock().unwrap() += 1;
            let mut most = self.most.lock().unwrap();
            *most = (*most).max(*open);
        }
        // Long enough that a second card would have to overlap this one.
        tokio::time::sleep(Duration::from_millis(60)).await;
        *self.open.lock().unwrap() -= 1;
        kernel::Approval::Once
    }
}

#[tokio::test]
async fn a_derived_kernel_queues_behind_the_parents_approval_card() {
    let gate = Arc::new(CountingGate::default());
    // `derive` shares the provider, so one script serves both sessions: a turn
    // each, one card each. Queue two or the child's turn finds it exhausted and
    // never reaches the gate at all.
    let parent = Kernel::new(
        Arc::new(ScriptedProvider::new(vec![
            Turn::Blocks(vec![intent_block("a", "fs.read")]),
            Turn::Blocks(vec![intent_block("b", "fs.read")]),
        ])),
        Arc::new(InMemoryLog::new()),
        Arc::new(SleepyExecutor {
            delay: Duration::ZERO,
        }),
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AlwaysHuman),
        gate.clone(),
        Arc::new(NoVerify),
    );
    // A sub-agent's kernel: its own executor and gate wrapper, everything else
    // inherited — exactly how a spawned child is built.
    let child = parent.derive(
        Arc::new(SleepyExecutor {
            delay: Duration::ZERO,
        }),
        Arc::new(Passthrough),
        gate.clone(),
    );

    let run_one = |k: Kernel<ScriptedProvider, InMemoryLog>| async move {
        let session = Session::new();
        let _ = k
            .run_session(
                &session,
                vec![Message::user("go")],
                Budget::turns(1),
                &CaptureSink::default(),
                Some(InterruptQueue::pair().1),
            )
            .await;
    };
    tokio::join!(run_one(parent), run_one(child));

    assert_eq!(
        *gate.calls.lock().unwrap(),
        2,
        "both sessions must actually reach the gate, or this proves nothing"
    );
    assert_eq!(
        *gate.most.lock().unwrap(),
        1,
        "a parent and its child must share one approval lane: two cards at one \
         surface means whichever the operator answers, the other is answered for them"
    );
}
