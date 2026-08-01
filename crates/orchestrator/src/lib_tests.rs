use super::*;
use async_trait::async_trait;
use kernel::{BlastRadius, Message, Observation, Role, ToolCategory, ToolIntent, ToolSpec};
use serde_json::json;

struct Tools;

#[async_trait]
impl Executor for Tools {
    fn specs(&self) -> Vec<ToolSpec> {
        ["fs.read", "fs.write"]
            .iter()
            .map(|name| ToolSpec {
                name: (*name).to_string(),
                description: String::new(),
                schema: json!({}),
                blast_radius: if name.ends_with("write") {
                    BlastRadius::ReversibleLocal
                } else {
                    BlastRadius::Read
                },
                category: ToolCategory::Other,
                icon: "•".into(),
            })
            .collect()
    }
    fn blast_radius(&self, tool: &str) -> Option<BlastRadius> {
        self.specs()
            .into_iter()
            .find(|spec| spec.name == tool)
            .map(|spec| spec.blast_radius)
    }
    async fn execute(&self, intent: &ToolIntent) -> Observation {
        Observation::ok(&intent.id, json!({}))
    }
}

/// Records what the runtime handed it, so the tests can assert on the
/// narrowing and budget decisions rather than on model output.
struct Recorder {
    seen: std::sync::Mutex<Vec<(Vec<String>, u32)>>,
}

#[async_trait]
impl ChildRunner for Recorder {
    async fn run(&self, run: ChildRun) -> Result<ChildOutcome, String> {
        let tools = run.executor.specs().into_iter().map(|s| s.name).collect();
        self.seen
            .lock()
            .unwrap()
            .push((tools, run.budget.max_turns.unwrap_or(0)));
        Ok(ChildOutcome {
            status: AgentStatus::Completed,
            summary: "done".into(),
            turns: 1,
            tool_calls: 0,
            trust: TrustLabel::Tool,
        })
    }
}

fn control() -> (Arc<Recorder>, AgentControl) {
    let recorder = Arc::new(Recorder {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let control = deliverable(recorder.clone(), CancellationToken::new());
    (recorder, control)
}

/// A control with somewhere to deliver to. Every spawn is asynchronous now, so
/// a control without an outbox refuses every one of them.
fn deliverable(runner: Arc<dyn ChildRunner>, cancel: CancellationToken) -> AgentControl {
    AgentControl::new(runner, cancel).with_outbox(Arc::new(MemoryOutbox::default()))
}

/// Wait for `ready`, failing the test rather than hanging if it never comes.
async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..20_000 {
        if ready() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for {what}")
}

/// Start a child and wait for its report — what production does across two
/// turns, collapsed into one call so a test can assert on the outcome.
async fn run(control: &AgentControl, spec: AgentSpec, turns: u32) -> Result<AgentResult, Error> {
    let owner = control.owner().unwrap_or_default();
    run_as(control, &Caller::root(owner), spec, turns).await
}

/// The same, from an agent's own address rather than the root's.
async fn run_as(
    control: &AgentControl,
    caller: &Caller,
    spec: AgentSpec,
    turns: u32,
) -> Result<AgentResult, Error> {
    // Wait on the signal, not a poll loop. `notify_one` leaves a permit if it
    // fires before the await, so a fast child cannot be missed.
    let ready = Arc::new(tokio::sync::Notify::new());
    if let Ok(mut slot) = control.notifier_handle().lock() {
        let wake = Arc::clone(&ready);
        *slot = Some(Arc::new(move || wake.notify_one()));
    }
    control
        .spawn_background(spec, caller, Arc::new(Tools), kernel::Budget::turns(turns))
        .await?;
    ready.notified().await;
    // Collect then settle, as a surface does: reading no longer acknowledges,
    // so without the settle the next call would hand back this report again.
    let reports = control.collect(caller.session).await;
    control.settle(caller.session, &reports).await;
    Ok(reports
        .into_iter()
        .next()
        .expect("the signal fires only once the report is collectable"))
}

fn at(path: &str) -> Caller {
    Caller {
        path: AgentPath::parse(path).unwrap(),
        session: Ulid::new(),
    }
}

fn spec(objective: &str) -> AgentSpec {
    AgentSpec {
        objective: objective.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_child_runs_read_only_within_the_parents_budget() {
    let (recorder, control) = control();
    let result = run(&control, spec("summarise the crate"), 5).await.unwrap();
    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.agent, "summarise-the-crate");

    let seen = recorder.seen.lock().unwrap();
    // fs.write is gone: O1 children cannot mutate a shared workspace.
    assert_eq!(seen[0].0, vec!["fs.read"]);
    assert_eq!(seen[0].1, 5);
}

#[tokio::test]
async fn a_child_budget_is_clamped_to_what_the_parent_has_left() {
    let (recorder, control) = control();
    let mut wants_more = spec("do a lot");
    wants_more.max_turns = Some(500);
    run(&control, wants_more, 4).await.unwrap();
    assert_eq!(recorder.seen.lock().unwrap()[0].1, 4);
}

#[tokio::test]
async fn capacity_is_refused_rather_than_queued() {
    let (_, control) = control();
    let control = control.with_limits(1, 1);
    // Hold the only permit, then prove a second spawn is refused outright.
    let held = control.reserve().unwrap();
    assert!(matches!(
        run(&control, spec("second"), 5).await,
        Err(Error::AtCapacity(1))
    ));
    drop(held);
    assert!(run(&control, spec("second"), 5).await.is_ok());
}

#[tokio::test]
async fn a_child_cannot_nest_past_the_depth_limit() {
    let (_, control) = control();
    // Depth is the *caller's*, so an agent one level down reads its own — a
    // counter on the shared control reads the root's and never fires.
    assert!(matches!(
        run_as(&control, &at("/parent"), spec("nested"), 5).await,
        Err(Error::TooDeep { depth: 2, max: 1 })
    ));
}

#[tokio::test]
async fn a_grandchild_hangs_off_its_own_parent_not_the_root() {
    let (_, control) = control();
    let control = control.with_limits(4, 3);
    run_as(&control, &at("/survey"), spec("parse"), 5)
        .await
        .unwrap();
    let paths: Vec<String> = control
        .agents()
        .into_iter()
        .map(|agent| agent.path.to_string())
        .collect();
    // A flat tree is the symptom of a child holding the root's address: it
    // would land at `/parse`, a sibling of the agent that asked for it.
    assert_eq!(paths, vec!["/survey/parse".to_string()]);
}

#[tokio::test]
async fn an_empty_objective_is_rejected() {
    let (_, control) = control();
    assert!(matches!(
        run(&control, spec("   "), 5).await,
        Err(Error::NoObjective)
    ));
}

/// Settles itself when its queue's token trips, as the real runner does now,
/// and records that it got to return rather than being dropped mid-run.
struct Cooperative {
    settled: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ChildRunner for Cooperative {
    async fn run(&self, run: ChildRun) -> Result<ChildOutcome, String> {
        run.cancel.cancelled().await;
        // A dropped future never reaches this line.
        self.settled
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(ChildOutcome {
            status: AgentStatus::Cancelled,
            summary: "settled its own work before returning".into(),
            turns: 1,
            tool_calls: 0,
            trust: TrustLabel::Tool,
        })
    }
}

/// A cancelled child must be allowed to finish writing and answer its own
/// in-flight tool calls. Racing its future against the token dropped it
/// mid-tool, which is how a half-written file and an unanswered call happen on
/// the one path where the work still has to survive.
#[tokio::test]
async fn a_cancelled_child_settles_itself_rather_than_being_dropped() {
    let settled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let control = Arc::new(deliverable(
        Arc::new(Cooperative {
            settled: Arc::clone(&settled),
        }),
        CancellationToken::new(),
    ));
    let parent = Ulid::new();
    control
        .spawn_background(
            spec("will be cancelled"),
            &Caller::root(parent),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    until("the child to start", || !control.active().is_empty()).await;
    control
        .cancel(&AgentPath::root(), "/will-be-cancelled")
        .unwrap();
    control.drain().await;

    assert!(
        settled.load(std::sync::atomic::Ordering::Acquire),
        "the run future was dropped instead of being allowed to settle"
    );
    let reported = control.collect(parent).await;
    assert_eq!(reported.len(), 1);
    assert_eq!(
        reported[0].summary, "settled its own work before returning",
        "the child's own account of the cancellation was discarded"
    );
}

/// Blocks until cancelled, so the roster/cancellation behaviour is testable
/// without depending on a runner that cooperates.
struct Hangs;

#[async_trait]
impl ChildRunner for Hangs {
    async fn run(&self, run: ChildRun) -> Result<ChildOutcome, String> {
        run.cancel.cancelled().await;
        futures::future::pending::<()>().await;
        unreachable!()
    }
}

#[tokio::test]
async fn a_cancelled_agent_leaves_no_phantom_in_the_roster() {
    let cancel = CancellationToken::new();
    let control = deliverable(Arc::new(Hangs), cancel.clone());
    cancel.cancel();
    let result = run(&control, spec("will be cancelled"), 5).await.unwrap();
    assert_eq!(result.status, AgentStatus::Cancelled);
    // The run future was dropped mid-flight; the roster must still be clean.
    assert!(control.active().is_empty());
}

#[tokio::test]
async fn capacity_is_one_budget_for_the_whole_tree() {
    let (_, control) = control();
    let control = control.with_limits(2, 2);
    let held = control.reserve().unwrap();
    let _also_held = control.reserve().unwrap();
    // A nested agent draws on the same permits, so depth cannot be used to buy
    // more concurrency than the operator allowed.
    assert!(matches!(
        run_as(&control, &at("/parent"), spec("nested"), 5).await,
        Err(Error::AtCapacity(2))
    ));
    drop(held);
    assert!(
        run_as(&control, &at("/parent"), spec("nested"), 5)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn nesting_is_allowed_when_the_limit_permits_it() {
    let (_, control) = control();
    let control = control.with_limits(4, 2);
    // Recursion is a capability, not an accident: at depth 2 of 2 a grandchild
    // is admitted, and the tools to do it are no longer stripped.
    assert!(
        run_as(&control, &at("/parent"), spec("nested"), 5)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn two_agents_never_answer_to_one_name() {
    let (_, control) = control();
    run(&control, spec("audit the crate"), 5).await.unwrap();
    let second = run(&control, spec("audit the crate"), 5).await.unwrap();
    // The second gets its own address rather than aliasing the first, so a
    // later reference cannot resolve to whichever happened to be found.
    assert_eq!(second.agent, "audit-the-crate-2");
    let root = AgentPath::root();
    assert!(control.reach(&root, "audit-the-crate").is_ok());
    assert!(control.reach(&root, "audit-the-crate-2").is_ok());
}

#[tokio::test]
async fn an_agent_cannot_reach_outside_its_own_subtree() {
    let (_, control) = control();
    let control = control.with_limits(4, 3);
    run_as(&control, &at("/survey"), spec("parse"), 5)
        .await
        .unwrap();
    run(&control, spec("unrelated"), 5).await.unwrap();
    let stranger = AgentPath::parse("/other").unwrap();
    // The delegation tools are shared with every descendant, so reach is the
    // only thing standing between a child and its siblings' cancel button.
    assert!(matches!(
        control.reach(&stranger, "/survey/parse"),
        Err(Error::OutOfReach(_))
    ));
    assert!(matches!(
        control.cancel(&stranger, "/unrelated"),
        Err(Error::OutOfReach(_))
    ));
    assert!(
        control
            .reach(&AgentPath::parse("/survey").unwrap(), "parse")
            .is_ok()
    );
}

#[tokio::test]
async fn a_wait_outside_the_operators_bounds_is_refused_not_clamped() {
    let bounds = WaitBounds {
        min: Duration::from_secs(5),
        default: Duration::from_secs(30),
        max: Duration::from_secs(60),
    };
    assert_eq!(bounds.resolve(None).unwrap(), Duration::from_secs(30));
    assert_eq!(
        bounds.resolve(Some(Duration::from_secs(10))).unwrap(),
        Duration::from_secs(10)
    );
    // Silently shortening a wait returns "nothing finished", which reads as an
    // answer rather than as a request that was never honoured.
    assert!(matches!(
        bounds.resolve(Some(Duration::from_secs(1))),
        Err(Error::WaitOutOfRange { min: 5, max: 60 })
    ));
    assert!(matches!(
        bounds.resolve(Some(Duration::from_secs(600))),
        Err(Error::WaitOutOfRange { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_child_settling_mid_wait_wakes_the_waiter() {
    let control = Arc::new(deliverable(Arc::new(Hangs), CancellationToken::new()));
    let root = AgentPath::root();
    control
        .spawn_background(
            spec("slow"),
            &Caller::root(Ulid::new()),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    let stopper = Arc::clone(&control);
    tokio::spawn(async move {
        until("the child to start", || !stopper.active().is_empty()).await;
        let _ = stopper.cancel(&AgentPath::root(), "/slow");
    });
    let waited = control.wait(&root, Duration::from_secs(10)).await;
    assert!(
        matches!(waited, Waited::Settled(ref paths) if paths[0].as_str() == "/slow"),
        "expected the settled child, got {waited:?}"
    );
    control.shutdown().await;
}

#[tokio::test]
async fn a_nested_childs_report_reaches_its_own_parent() {
    let control = Arc::new(
        deliverable(
            Arc::new(Recorder {
                seen: std::sync::Mutex::new(Vec::new()),
            }),
            CancellationToken::new(),
        )
        .with_limits(4, 3),
    );
    // The parent has to be live to receive anything, so it is registered by
    // hand here — production has it running its own session.
    let (parent_path, reservation) = control
        .registry
        .claim(&AgentPath::root(), "survey")
        .unwrap();
    let (handle, mut queue) = kernel::InterruptQueue::pair();
    reservation.commit(
        registry::Agent {
            path: parent_path.clone(),
            session: Ulid::new().to_string(),
            objective: "survey".into(),
            started_ms: epoch_ms(),
            state: registry::State::Running,
            write: false,
            tools: None,
        },
        registry::Live {
            cancel: CancellationToken::new(),
            steer: handle,
            budget: kernel::Budget::turns(5),
        },
    );

    let caller = Caller {
        path: parent_path,
        session: Ulid::new(),
    };
    run_as(&control, &caller, spec("parse"), 5).await.unwrap();

    // A nested parent is a running session, not a surface: it has no outbox
    // pass, so a report written only to its chain is one nobody ever reads.
    let delivered = queue.drain_steers();
    // Tagged, not prose: it arrives on the queue the parent is told to treat as
    // authoritative, so an untagged report reads as an order to do what the
    // report describes.
    let Some((text, trust)) = delivered.first() else {
        panic!("the parent never received its child's report");
    };
    assert!(text.contains("AGENT_REPORT"), "untagged report: {text}");
    assert!(text.contains("/survey/parse"), "unattributed: {text}");
    assert!(text.contains("COMPLETED"), "no outcome: {text}");
    // Labelled with what the child touched, so a nested parent acting on the
    // report escalates exactly as it would on a direct observation.
    assert_eq!(*trust, TrustLabel::Tool, "report arrived unlabelled");
}

/// Hands back whatever conversation it was given, so a test can assert on what
/// a child actually inherited.
struct Echoes {
    inherited: std::sync::Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl ChildRunner for Echoes {
    async fn run(&self, run: ChildRun) -> Result<ChildOutcome, String> {
        self.inherited.lock().unwrap().push(
            run.history
                .iter()
                .map(|message| message.content.clone())
                .collect(),
        );
        Ok(ChildOutcome {
            status: AgentStatus::Completed,
            summary: "done".into(),
            turns: 1,
            tool_calls: 0,
            trust: TrustLabel::Tool,
        })
    }
}

struct Said(Vec<Message>);

#[async_trait]
impl Transcripts for Said {
    async fn history(&self, _session: Ulid) -> Vec<Message> {
        self.0.clone()
    }
}

fn forking() -> (Arc<Echoes>, AgentControl) {
    let runner = Arc::new(Echoes {
        inherited: std::sync::Mutex::new(Vec::new()),
    });
    let control = deliverable(runner.clone(), CancellationToken::new()).with_transcripts(Arc::new(
        Said(vec![
            Message::new(Role::User, "what does the parser do?"),
            Message::new(Role::Assistant, "it builds an AST"),
        ]),
    ));
    (runner, control)
}

#[tokio::test]
async fn a_forked_child_starts_with_the_callers_conversation() {
    let (runner, control) = forking();
    let mut asked = spec("check the lexer");
    asked.fork = Fork::All;
    run(&control, asked, 5).await.unwrap();
    // Starting cold is what made a delegated writer redo the reasoning its
    // caller had already paid for.
    assert_eq!(
        runner.inherited.lock().unwrap()[0],
        ["what does the parser do?", "it builds an AST"]
    );
}

#[tokio::test]
async fn a_child_asked_to_start_cold_inherits_nothing() {
    let (runner, control) = forking();
    let mut asked = spec("self-contained job");
    asked.fork = Fork::None;
    run(&control, asked, 5).await.unwrap();
    assert!(runner.inherited.lock().unwrap()[0].is_empty());
}

#[tokio::test]
async fn a_message_reaches_sideways_where_control_does_not() {
    let (_, control) = control();
    let control = control.with_limits(4, 3);
    run_as(&control, &at("/survey"), spec("parse"), 5)
        .await
        .unwrap();
    let stranger = AgentPath::parse("/other").unwrap();
    // Talking costs the recipient a turn; cancelling destroys its work. Only
    // the second needs containment, and a tree whose agents cannot talk to each
    // other is a tree for nothing.
    assert!(control.address(&stranger, "/survey/parse").is_ok());
    assert!(matches!(
        control.reach(&stranger, "/survey/parse"),
        Err(Error::OutOfReach(_))
    ));
}

/// Register a live agent by hand, keeping its queue so a test can read what was
/// delivered to it. Production has a running session on the other end.
fn listening(control: &AgentControl, path: &str) -> (AgentPath, kernel::InterruptQueue) {
    let path = AgentPath::parse(path).unwrap();
    let reservation = control
        .registry
        .claim(&path.parent().unwrap(), path.name())
        .unwrap()
        .1;
    let (handle, queue) = kernel::InterruptQueue::pair();
    reservation.commit(
        registry::Agent {
            path: path.clone(),
            session: Ulid::new().to_string(),
            objective: "work".into(),
            started_ms: epoch_ms(),
            state: registry::State::Running,
            write: false,
            tools: None,
        },
        registry::Live {
            cancel: CancellationToken::new(),
            steer: handle,
            budget: kernel::Budget::turns(5),
        },
    );
    (path, queue)
}

#[tokio::test]
async fn a_peers_message_is_not_dressed_up_as_an_order() {
    let (_, control) = control();
    let (path, mut queue) = listening(&control, "/worker");

    control
        .message(
            &AgentPath::parse("/peer").unwrap(),
            path.as_str(),
            "the lexer is generated, do not read it",
        )
        .unwrap();

    // Same queue as the recipient's own operator, and an agent is told to treat
    // what arrives there as authoritative and superseding. A peer is not its
    // operator, and an untagged note would be obeyed as though it were.
    let delivered = queue.drain_steers();
    let (text, _) = delivered.first().expect("the message reached the agent");
    assert!(text.contains("AGENT_MESSAGE"), "untagged: {text}");
    assert!(text.contains("/peer"), "unattributed: {text}");
    assert!(
        text.contains("the lexer is generated, do not read it"),
        "the message itself must survive tagging: {text}"
    );
}

#[tokio::test]
async fn a_message_to_a_finished_agent_says_so_rather_than_vanishing() {
    let (_, control) = control();
    run(&control, spec("quick job"), 5).await.unwrap();
    // The agent settled the moment it reported. Accepting text for it would
    // leave the sender believing it had passed something along.
    assert!(matches!(
        control.message(&AgentPath::root(), "/quick-job", "one more thing"),
        Err(Error::Settled(_))
    ));
}

#[tokio::test]
async fn a_followup_continues_the_same_agent_rather_than_cloning_it() {
    let (runner, control) = forking();
    let owner = Ulid::new();
    let caller = Caller::root(owner);
    let first = control
        .spawn_background(
            spec("survey the crate"),
            &caller,
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    control.drain().await;

    let again = control
        .followup(
            &caller,
            "/survey-the-crate",
            "now check the lexer",
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    control.drain().await;

    // Same address and same session: two halves of one agent's work under one
    // id, not a near-namesake that has to rediscover everything.
    assert_eq!(again.path, first.path);
    assert_eq!(again.session, first.session);
    assert_eq!(runner.inherited.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_followup_is_refused_for_an_agent_that_is_not_the_callers() {
    let (_, control) = control();
    let control = control.with_limits(4, 3);
    run(&control, spec("mine"), 5).await.unwrap();
    let stranger = Caller {
        path: AgentPath::parse("/elsewhere").unwrap(),
        session: Ulid::new(),
    };
    // A follow-up spends money on someone else's worker and restarts it.
    assert!(matches!(
        control
            .followup(
                &stranger,
                "/mine",
                "more work",
                Arc::new(Tools),
                kernel::Budget::turns(5)
            )
            .await,
        Err(Error::OutOfReach(_))
    ));
}

#[tokio::test]
async fn a_wait_ends_when_the_operator_speaks() {
    let control = Arc::new(deliverable(Arc::new(Hangs), CancellationToken::new()));
    let (interrupt, _queue) = kernel::InterruptQueue::pair();
    control.attend(interrupt.clone());
    control
        .spawn_background(
            spec("long job"),
            &Caller::root(Ulid::new()),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    until("the child to start", || !control.active().is_empty()).await;

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        interrupt.steer("stop waiting, do this instead");
    });
    // Holding the turn to its deadline after the user has already said
    // something is obeying instructions known to be superseded.
    let waited = control
        .wait(&AgentPath::root(), Duration::from_secs(30))
        .await;
    assert_eq!(waited, Waited::Interrupted);
    control.shutdown().await;
}

#[tokio::test]
async fn a_wait_ignores_what_was_said_before_it_started() {
    let control = Arc::new(deliverable(Arc::new(Hangs), CancellationToken::new()));
    let (interrupt, _queue) = kernel::InterruptQueue::pair();
    control.attend(interrupt.clone());
    // Already read by the turn that is now waiting; treating it as fresh would
    // make every wait after a steer return instantly.
    interrupt.steer("earlier instruction");
    control
        .spawn_background(
            spec("long job"),
            &Caller::root(Ulid::new()),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    until("the child to start", || !control.active().is_empty()).await;

    let waited = control
        .wait(&AgentPath::root(), Duration::from_millis(80))
        .await;
    assert_eq!(waited, Waited::TimedOut);
    control.shutdown().await;
}

#[tokio::test]
async fn waiting_returns_only_for_the_callers_own_children() {
    let (_, control) = control();
    let control = control.with_limits(4, 3);
    run(&control, spec("mine"), 5).await.unwrap();
    // Someone else's finished child is not this caller's answer, so a wait that
    // reported it would return an agent the caller cannot even address.
    let stranger = AgentPath::parse("/elsewhere").unwrap();
    assert_eq!(
        control.wait(&stranger, Duration::from_millis(50)).await,
        Waited::Settled(Vec::new())
    );
}

/// Reports a summary far past the context cap.
struct Verbose;

#[async_trait]
impl ChildRunner for Verbose {
    async fn run(&self, _run: ChildRun) -> Result<ChildOutcome, String> {
        Ok(ChildOutcome {
            status: AgentStatus::Completed,
            summary: "x".repeat(MAX_SUMMARY_CHARS * 2),
            turns: 1,
            tool_calls: 0,
            trust: TrustLabel::Tool,
        })
    }
}

#[tokio::test]
async fn a_long_report_reaches_the_caller_whole() {
    let control = deliverable(Arc::new(Verbose), CancellationToken::new());
    let result = run(&control, spec("write at length"), 5).await.unwrap();
    // The runtime must not truncate: the caller owns the artifact store, so
    // trimming here would discard the tail before anything could persist it.
    assert_eq!(result.summary.len(), MAX_SUMMARY_CHARS * 2);
}

/// In-memory stand-in for the event-log outbox, with the same state machine.
#[derive(Default)]
struct MemoryOutbox {
    rows: std::sync::Mutex<Vec<(Dispatch, Option<AgentResult>, bool)>>,
    /// The durable patch record, with the same `recorded → applied` fold
    /// the log implements.
    patches: std::sync::Mutex<Vec<Recorded>>,
    /// Stand-in for "newest event on the child's chain".
    activity: std::sync::Mutex<std::collections::HashMap<Ulid, f64>>,
}

struct Recorded {
    parent: Ulid,
    agent: String,
    child: String,
    dispatch: String,
    patch: Patch,
    applied: bool,
}

#[async_trait]
impl Outbox for MemoryOutbox {
    async fn dispatched(&self, dispatch: &Dispatch) -> bool {
        self.rows
            .lock()
            .unwrap()
            .push((dispatch.clone(), None, false));
        true
    }
    async fn finished(&self, dispatch: &Dispatch, result: &AgentResult) -> bool {
        if let Some(row) = self
            .rows
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(d, _, _)| d.id == dispatch.id)
        {
            row.1 = Some(result.clone());
        }
        true
    }
    async fn delivered(&self, _parent: Ulid, dispatch: Ulid) {
        if let Some(row) = self
            .rows
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(d, _, _)| d.id == dispatch)
        {
            row.2 = true;
        }
    }
    async fn transcript(&self, child: Ulid) -> Vec<String> {
        vec![format!("transcript:{child}")]
    }
    async fn recorded(
        &self,
        parent: Ulid,
        dispatch: Ulid,
        agent: &str,
        child: Ulid,
        patch: &Patch,
    ) -> bool {
        self.patches.lock().unwrap().push(Recorded {
            parent,
            agent: agent.to_string(),
            child: child.to_string(),
            dispatch: dispatch.to_string(),
            patch: patch.clone(),
            applied: false,
        });
        true
    }
    async fn applied(&self, _parent: Ulid, dispatch: Ulid) {
        if let Some(row) = self
            .patches
            .lock()
            .unwrap()
            .iter_mut()
            .find(|row| row.dispatch == dispatch.to_string())
        {
            row.applied = true;
        }
    }
    async fn unapplied(&self, parent: Ulid) -> Vec<Pending> {
        self.patches
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.parent == parent && !row.applied)
            .map(|row| Pending {
                agent: row.agent.clone(),
                session: row.child.clone(),
                dispatch: row.dispatch.clone(),
                patch: row.patch.clone(),
            })
            .collect()
    }
    async fn last_activity(&self, child: Ulid) -> Option<f64> {
        self.activity.lock().unwrap().get(&child).copied()
    }
    async fn reap_abandoned(&self, parent: Ulid) -> usize {
        // A row dispatched but never finished is the in-memory shape of the
        // same abandonment the log implementation detects by instance.
        let mut rows = self.rows.lock().unwrap();
        let mut reaped = 0;
        for (dispatch, result, _) in rows.iter_mut() {
            if dispatch.parent == parent && result.is_none() {
                *result = Some(AgentResult {
                    agent: dispatch.agent.clone(),
                    session: dispatch.child.to_string(),
                    dispatch: dispatch.id.to_string(),
                    status: AgentStatus::Failed,
                    summary: "[outcome unknown — abandoned]".into(),
                    artifact: None,
                    turns: 0,
                    tool_calls: 0,
                    duration_ms: 0,
                    trust: TrustLabel::Tool,
                    patch: None,
                });
                reaped += 1;
            }
        }
        reaped
    }
    async fn undelivered(&self, parent: Ulid) -> Vec<AgentResult> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .filter(|(d, result, delivered)| d.parent == parent && result.is_some() && !delivered)
            .filter_map(|(_, result, _)| result.clone())
            .collect()
    }
}

fn background_control() -> (Arc<MemoryOutbox>, AgentControl) {
    let outbox = Arc::new(MemoryOutbox::default());
    let control = deliverable(
        Arc::new(Recorder {
            seen: std::sync::Mutex::new(Vec::new()),
        }),
        CancellationToken::new(),
    )
    .with_outbox(outbox.clone());
    (outbox, control)
}

#[tokio::test]
async fn a_background_result_reaches_the_session_that_asked_for_it() {
    let (_outbox, control) = background_control();
    let asked = Ulid::new();
    let someone_else = Ulid::new();

    control
        .spawn_background(
            spec("survey"),
            &Caller::root(asked),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    control.drain().await;

    // The owner is captured at dispatch. Resolving "the current session" on
    // completion is what lands a report in a different chat after a restart.
    assert!(control.collect(someone_else).await.is_empty());
    let mine = control.collect(asked).await;
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].status, AgentStatus::Completed);
}

/// The signal must arrive only once the report can actually be read.
///
/// A child leaves the roster inside its own execution, *before* its report
/// is persisted, so anything keyed on the roster emptying races the record
/// it is trying to read — and the loser is a turn spent on nothing.
#[tokio::test]
async fn the_ready_signal_never_arrives_before_the_report_is_collectable() {
    let (_outbox, control) = background_control();
    let parent = Ulid::new();
    // Captures what `collect` would have returned at the instant of the
    // signal, from inside the callback itself.
    let seen: Arc<std::sync::Mutex<Vec<usize>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let probe = Arc::clone(&seen);
    let reader = Arc::new(control);
    let watched = Arc::clone(&reader);
    if let Ok(mut slot) = reader.notifier_handle().lock() {
        *slot = Some(Arc::new(move || {
            let ready = futures::executor::block_on(watched.collect(parent));
            probe.lock().unwrap().push(ready.len());
        }));
    }

    reader
        .spawn_background(
            spec("survey"),
            &Caller::root(parent),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    reader.drain().await;

    let observed = seen.lock().unwrap();
    assert_eq!(observed.len(), 1, "the signal fires once per child");
    assert_eq!(
        observed[0], 1,
        "the report must be collectable at the moment the signal fires"
    );
}

#[tokio::test]
async fn a_delivered_result_is_never_handed_over_twice() {
    let (_outbox, control) = background_control();
    let parent = Ulid::new();
    control
        .spawn_background(
            spec("survey"),
            &Caller::root(parent),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    control.drain().await;

    let reports = control.collect(parent).await;
    assert_eq!(reports.len(), 1);
    // Reading is not acknowledging: until the caller settles, a failed turn
    // must be able to see the report again.
    assert_eq!(control.collect(parent).await.len(), 1);
    control.settle(parent, &reports).await;
    // Once settled, replaying must not inject the same report again.
    assert!(control.collect(parent).await.is_empty());
}

#[tokio::test]
async fn a_spawn_is_refused_without_durable_delivery() {
    // Deliberately no outbox: the result could not survive the process, so
    // accepting the spawn would promise a delivery that cannot happen.
    let control = AgentControl::new(
        Arc::new(Recorder {
            seen: std::sync::Mutex::new(Vec::new()),
        }),
        CancellationToken::new(),
    );
    assert!(matches!(
        control
            .spawn_background(
                spec("survey"),
                &Caller::root(Ulid::new()),
                Arc::new(Tools),
                kernel::Budget::turns(5)
            )
            .await,
        Err(Error::Unavailable)
    ));
}

#[tokio::test]
async fn a_cancelled_background_agent_still_reports() {
    let control = deliverable(Arc::new(Hangs), CancellationToken::new())
        .with_outbox(Arc::new(MemoryOutbox::default()));
    let parent = Ulid::new();
    control
        .spawn_background(
            spec("never finishes"),
            &Caller::root(parent),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    control.shutdown().await;

    // Partial-result preservation (§6.5): killing a child must not lose the
    // fact that it ran, or the parent is left waiting on nothing.
    let reported = control.collect(parent).await;
    assert_eq!(reported.len(), 1);
    assert_eq!(reported[0].status, AgentStatus::Cancelled);
}

/// Drains its steer queue and reports what it was told, so steering is
/// asserted on what the *session loop* received — not on the roster having
/// a handle. A queue the runner drops would pass the second check and fail
/// this one, and that is exactly the bug worth catching: text accepted and
/// silently lost is worse than text refused.
struct Listens;

#[async_trait]
impl ChildRunner for Listens {
    async fn run(&self, mut run: ChildRun) -> Result<ChildOutcome, String> {
        let mut heard: Vec<String> = Vec::new();
        // Wait for the parent's steer, then report it back as the summary.
        while heard.is_empty() {
            if run.cancel.is_cancelled() {
                break;
            }
            heard.extend(
                run.interrupts
                    .drain_steers()
                    .into_iter()
                    .map(|(text, _)| text),
            );
            tokio::task::yield_now().await;
        }
        Ok(ChildOutcome {
            status: AgentStatus::Completed,
            summary: heard.join("|"),
            turns: 1,
            tool_calls: 0,
            trust: TrustLabel::Tool,
        })
    }
}

#[tokio::test]
async fn steering_reaches_the_running_child() {
    let control = Arc::new(deliverable(Arc::new(Listens), CancellationToken::new()));
    let steering = Arc::clone(&control);
    // Steer from outside while the child runs, which is the only way this is
    // ever used.
    tokio::spawn(async move {
        until("the child to start", || !steering.active().is_empty()).await;
        let path = steering.active()[0].path.clone();
        let _ = steering.steer(
            &AgentPath::root(),
            path.as_str(),
            "only the parser, skip the lexer",
        );
    });

    let result = run(&control, spec("survey the compiler"), 5).await.unwrap();
    assert_eq!(result.summary, "only the parser, skip the lexer");
}

#[tokio::test]
async fn a_stalled_child_is_distinguishable_from_a_working_one() {
    let outbox = Arc::new(MemoryOutbox::default());
    let control = deliverable(Arc::new(Hangs), CancellationToken::new())
        .with_outbox(outbox.clone())
        .with_limits(4, 1);
    let parent = Ulid::new();
    let busy = control
        .spawn_background(
            spec("busy worker"),
            &Caller::root(parent),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    let stalled = control
        .spawn_background(
            spec("stalled worker"),
            &Caller::root(parent),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();

    let now = epoch_ms() as f64 / 1000.0;
    {
        let mut activity = outbox.activity.lock().unwrap();
        activity.insert(busy.session.parse().unwrap(), now - 1.0);
        activity.insert(stalled.session.parse().unwrap(), now - 300.0);
    }

    let idle = control.idle_times().await;
    // Both started at the same moment, so elapsed-since-start says they are
    // identical. Only last-activity separates them.
    assert!(idle[&busy.session].unwrap() < 5_000);
    assert!(idle[&stalled.session].unwrap() > 250_000);
    control.shutdown().await;
}

#[tokio::test]
async fn a_child_that_has_recorded_nothing_reads_as_unknown_not_stalled() {
    let control = deliverable(Arc::new(Hangs), CancellationToken::new())
        .with_outbox(Arc::new(MemoryOutbox::default()));
    let parent = Ulid::new();
    let starting = control
        .spawn_background(
            spec("just started"),
            &Caller::root(parent),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    // Starting up is not stalling. Reporting a huge idle time for a child
    // that simply has not written its first event yet would flag every
    // healthy agent as stuck in its opening moments.
    assert_eq!(control.idle_times().await[&starting.session], None);
    control.shutdown().await;
}

#[tokio::test]
async fn steering_an_agent_that_is_not_running_reaches_nobody() {
    let (_, control) = control();
    // Must be visible to the caller: silently accepting text for a finished
    // agent would let the model believe it had corrected a run.
    assert!(
        control
            .steer(&AgentPath::root(), "no-such-agent", "hello")
            .is_err()
    );
}

#[tokio::test]
async fn empty_steer_text_is_refused_before_it_reaches_anyone() {
    let control = Arc::new(deliverable(Arc::new(Hangs), CancellationToken::new()));
    let steering = Arc::clone(&control);
    tokio::spawn(async move {
        let _ = run(&steering, spec("worker task"), 5).await;
    });
    until("the child to start", || !control.active().is_empty()).await;
    // Whitespace would arrive as an empty user message and cost the child a
    // turn to read nothing.
    let root = AgentPath::root();
    assert!(control.steer(&root, "worker-task", "   ").is_err());
    assert!(control.steer(&root, "worker-task", "narrow it").is_ok());
    control.shutdown().await;
}

#[tokio::test]
async fn cancelling_one_agent_leaves_its_siblings_running() {
    let control = deliverable(Arc::new(Hangs), CancellationToken::new())
        .with_outbox(Arc::new(MemoryOutbox::default()))
        .with_limits(4, 1);
    let parent = Ulid::new();
    let first = control
        .spawn_background(
            spec("first task"),
            &Caller::root(parent),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    control
        .spawn_background(
            spec("second task"),
            &Caller::root(parent),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();

    assert_eq!(
        control.cancel(&AgentPath::root(), &first.session).unwrap(),
        first.path
    );
    // The sibling is untouched — a per-child token, not the tree's.
    let still_running = control.active();
    assert!(still_running.iter().any(|run| run.path != first.path));
    control.shutdown().await;
}

// ── writer isolation (O3) ────────────────────────────────────────────────

/// A repository with one commit, plus a pool cutting worktrees outside it.
async fn writable() -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path().canonicalize().unwrap();
    let run = |args: Vec<&str>| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
    };
    run(vec!["init", "--initial-branch=main"]);
    run(vec!["config", "user.email", "t@example.com"]);
    run(vec!["config", "user.name", "t"]);
    // The assertions below are about worktree and patch behavior, not Git's
    // host-specific text checkout policy. Keep LF fixtures stable on Windows.
    run(vec!["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("a.txt"), "original\n").unwrap();
    run(vec!["add", "."]);
    run(vec!["commit", "-m", "base"]);
    let state = tempfile::tempdir().unwrap();
    (repo, state, root)
}

/// Git for Windows may check out text files with CRLF according to the
/// machine's `core.autocrlf` setting. These tests assert logical file content,
/// not the platform's newline convention.
fn read_text(path: impl AsRef<std::path::Path>) -> String {
    std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

/// Isolation over a real repository. The executor is deliberately *not*
/// rebased here — this crate cannot build one — so the tests assert on
/// worktree lifecycle, patch return and the refusal path, and the rebasing
/// itself is covered where it lives, in the tool registry.
struct Isolation {
    pool: WorktreePool,
    repo: std::path::PathBuf,
    verification: Option<Verification>,
}

struct BlockingVerification {
    inner: Arc<Isolation>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Workspaces for BlockingVerification {
    async fn checkout(&self, session: Ulid) -> Result<Workspace, String> {
        self.inner.checkout(session).await
    }

    async fn verify(&self, _root: &Path, _cancel: &CancellationToken) -> Option<Verification> {
        self.entered.notify_one();
        self.release.notified().await;
        Some(Verification {
            command: "blocked verifier".into(),
            passed: true,
            output: "released".into(),
        })
    }

    fn repo(&self) -> std::path::PathBuf {
        self.inner.repo()
    }
}

#[async_trait]
impl Workspaces for Isolation {
    async fn checkout(&self, session: Ulid) -> Result<Workspace, String> {
        let worktree = self
            .pool
            .checkout(session)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Workspace {
            worktree,
            executor: Arc::new(Tools),
        })
    }
    async fn verify(&self, _root: &Path, _cancel: &CancellationToken) -> Option<Verification> {
        self.verification.clone()
    }
    fn repo(&self) -> std::path::PathBuf {
        self.repo.clone()
    }
}

fn isolation(root: &Path, state: &tempfile::TempDir) -> Arc<Isolation> {
    Arc::new(Isolation {
        pool: WorktreePool::new(root, state.path().join("worktrees")),
        repo: root.to_path_buf(),
        verification: None,
    })
}

fn writer(objective: &str) -> AgentSpec {
    AgentSpec {
        objective: objective.into(),
        write: true,
        ..Default::default()
    }
}

/// Edits a file inside whatever workspace it was given, so the tests can
/// assert on where a writer's changes actually land.
struct Edits;

#[async_trait]
impl ChildRunner for Edits {
    async fn run(&self, run: ChildRun) -> Result<ChildOutcome, String> {
        let root = run.workspace.ok_or("no workspace")?;
        std::fs::write(root.join("a.txt"), "rewritten by the child\n")
            .map_err(|e| e.to_string())?;
        std::fs::write(root.join("added.txt"), "new file\n").map_err(|e| e.to_string())?;
        Ok(ChildOutcome {
            status: AgentStatus::Completed,
            summary: "edited".into(),
            turns: 1,
            tool_calls: 2,
            trust: TrustLabel::Tool,
        })
    }
}

#[tokio::test]
async fn a_writer_without_isolation_is_refused_not_downgraded() {
    let (_, control) = control();
    // Running it read-only would fail the objective; running it un-isolated
    // would corrupt the parent's tree. Neither is a safe default, so the
    // spawn is refused and the model is told to make the edits itself.
    assert!(matches!(
        run(&control, writer("fix the bug"), 5).await,
        Err(Error::NoIsolation(_))
    ));
    assert!(!control.can_write());
}

#[tokio::test]
async fn a_writer_edits_its_own_checkout_and_never_the_parents() {
    let (_repo, state, root) = writable().await;
    let control = deliverable(Arc::new(Edits), CancellationToken::new())
        .with_workspaces(isolation(&root, &state));

    let result = run(&control, writer("rewrite a.txt"), 5).await.unwrap();
    assert_eq!(result.status, AgentStatus::Completed);
    // The parent's working tree is exactly as it was.
    assert_eq!(read_text(root.join("a.txt")), "original\n");
    assert!(!root.join("added.txt").exists());
}

#[tokio::test]
async fn a_writer_returns_a_patch_rather_than_an_account_of_its_edits() {
    let (_repo, state, root) = writable().await;
    let control = deliverable(Arc::new(Edits), CancellationToken::new())
        .with_workspaces(isolation(&root, &state));

    let result = run(&control, writer("rewrite a.txt"), 5).await.unwrap();
    let patch = result.patch.expect("a writer reports a patch");
    assert!(patch.diff.contains("rewritten by the child"));
    assert!(patch.files.contains(&"a.txt".to_string()));
    assert!(patch.files.contains(&"added.txt".to_string()));
    // Which is reviewable *and* applicable — the point of a diff over prose.
    assert_eq!(control.check(&patch).await, Some(MergeCheck::Clean));
    control.merge(&patch, false).await.unwrap();
    assert_eq!(read_text(root.join("a.txt")), "rewritten by the child\n");
}

#[tokio::test]
async fn nothing_is_merged_until_someone_asks_for_it() {
    let (_repo, state, root) = writable().await;
    let control = deliverable(Arc::new(Edits), CancellationToken::new())
        .with_workspaces(isolation(&root, &state));

    let result = run(&control, writer("rewrite a.txt"), 5).await.unwrap();
    // Merging is a consequential action on the user's files. Finishing a
    // child must never be what triggers it.
    assert!(result.patch.is_some());
    assert_eq!(read_text(root.join("a.txt")), "original\n");
}

#[tokio::test]
async fn a_patch_carries_the_verification_the_runtime_ran_itself() {
    let (_repo, state, root) = writable().await;
    let workspaces = Arc::new(Isolation {
        pool: WorktreePool::new(&root, state.path().join("worktrees")),
        repo: root.clone(),
        verification: Some(Verification {
            command: "cargo test".into(),
            passed: false,
            output: "2 failed".into(),
        }),
    });
    let control =
        deliverable(Arc::new(Edits), CancellationToken::new()).with_workspaces(workspaces);

    let result = run(&control, writer("rewrite a.txt"), 5).await.unwrap();
    // The child said it succeeded; the build says otherwise, and the build
    // is what the merge gate reads.
    assert_eq!(result.status, AgentStatus::Completed);
    let patch = result.patch.unwrap();
    assert!(!patch.verified());
    assert_eq!(patch.verification.unwrap().output, "2 failed");
}

#[tokio::test]
async fn capacity_is_held_through_verification_and_terminal_settlement() {
    let (_repo, state, root) = writable().await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let workspaces = Arc::new(BlockingVerification {
        inner: isolation(&root, &state),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let control = AgentControl::new(Arc::new(Edits), CancellationToken::new())
        .with_outbox(Arc::new(MemoryOutbox::default()))
        .with_workspaces(workspaces)
        .with_limits(1, 1);
    let caller = Caller::root(Ulid::new());

    control
        .spawn_background(
            writer("first writer"),
            &caller,
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("the first writer never reached verification");

    let second = control
        .spawn_background(
            spec("second child"),
            &caller,
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await;
    release.notify_one();
    control.drain().await;

    assert!(
        matches!(second, Err(Error::AtCapacity(1))),
        "post-run verification escaped the configured lifecycle capacity"
    );
}

#[tokio::test]
async fn a_cancelled_writer_still_hands_back_what_it_changed() {
    let (_repo, state, root) = writable().await;
    let control = deliverable(Arc::new(Hangs), CancellationToken::new())
        .with_outbox(Arc::new(MemoryOutbox::default()))
        .with_workspaces(isolation(&root, &state));
    let parent = Ulid::new();
    control
        .spawn_background(
            writer("never finishes"),
            &Caller::root(parent),
            Arc::new(Tools),
            kernel::Budget::turns(5),
        )
        .await
        .unwrap();
    control.shutdown().await;

    let reported = control.collect(parent).await;
    assert_eq!(reported.len(), 1);
    assert_eq!(reported[0].status, AgentStatus::Cancelled);
    // Empty here because `Hangs` never writes — but the patch was still
    // taken, which is what keeps a killed writer's real work from being
    // thrown away with its checkout.
    assert!(reported[0].patch.as_ref().is_some_and(Patch::is_empty));
}

#[tokio::test]
async fn a_writers_checkout_does_not_outlive_it() {
    let (_repo, state, root) = writable().await;
    let worktrees = state.path().join("worktrees");
    let control = deliverable(Arc::new(Edits), CancellationToken::new()).with_workspaces(Arc::new(
        Isolation {
            pool: WorktreePool::new(&root, worktrees.clone()),
            repo: root.clone(),
            verification: None,
        },
    ));

    run(&control, writer("rewrite a.txt"), 5).await.unwrap();
    // An orphaned worktree wedges the next `git worktree add` on that path,
    // so this is a correctness property, not tidiness.
    let left: Vec<_> = std::fs::read_dir(&worktrees)
        .map(|entries| entries.filter_map(Result::ok).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "a checkout was left behind");
}

/// Isolation whose verifier reports whatever the test needs it to.
fn judged(
    root: &Path,
    state: &tempfile::TempDir,
    verification: Option<Verification>,
) -> Arc<Isolation> {
    Arc::new(Isolation {
        pool: WorktreePool::new(root, state.path().join("worktrees")),
        repo: root.to_path_buf(),
        verification,
    })
}

fn failed_build() -> Option<Verification> {
    Some(Verification {
        command: "cargo test".into(),
        passed: false,
        output: "error[E0308]: mismatched types".into(),
    })
}

#[tokio::test]
async fn a_patch_that_does_not_build_does_not_merge() {
    let (_repo, state, root) = writable().await;
    let control = deliverable(Arc::new(Edits), CancellationToken::new()).with_workspaces(judged(
        &root,
        &state,
        failed_build(),
    ));
    let result = run(&control, writer("rewrite a.txt"), 5).await.unwrap();
    let patch = result.patch.unwrap();

    // §6.4. The child said it succeeded and the diff applies cleanly — the
    // only thing standing between a broken change and the user's tree is
    // this refusal.
    assert_eq!(control.check(&patch).await, Some(MergeCheck::Clean));
    assert!(matches!(
        control.merge(&patch, false).await,
        Err(Error::Unverified(_))
    ));
    assert_eq!(read_text(root.join("a.txt")), "original\n");
}

#[tokio::test]
async fn a_failing_patch_can_still_be_forced_deliberately() {
    let (_repo, state, root) = writable().await;
    let control = deliverable(Arc::new(Edits), CancellationToken::new()).with_workspaces(judged(
        &root,
        &state,
        failed_build(),
    ));
    let patch = run(&control, writer("rewrite a.txt"), 5)
        .await
        .unwrap()
        .patch
        .unwrap();

    // A pre-existing unrelated build failure must not make delegation
    // permanently unusable — but getting past it has to be an explicit act.
    control.merge(&patch, true).await.unwrap();
    assert_eq!(read_text(root.join("a.txt")), "rewritten by the child\n");
}

#[tokio::test]
async fn a_project_with_no_verifier_is_not_blocked_by_the_gate() {
    let (_repo, state, root) = writable().await;
    let control = deliverable(Arc::new(Edits), CancellationToken::new())
        .with_workspaces(judged(&root, &state, None));
    let patch = run(&control, writer("rewrite a.txt"), 5)
        .await
        .unwrap()
        .patch
        .unwrap();

    // No verification is not failed verification. Refusing on evidence the
    // project cannot produce would make writers useless rather than safe.
    assert!(!patch.verified());
    control.merge(&patch, false).await.unwrap();
}

/// A control sharing one outbox and owner with another — what a restart
/// looks like from the patch registry's point of view.
fn restarted(
    outbox: Arc<MemoryOutbox>,
    owner: OwnerHandle,
    workspaces: Arc<Isolation>,
) -> AgentControl {
    // The *same* outbox across both controls — that shared record is what a
    // restart reads back.
    AgentControl::new(Arc::new(Edits), CancellationToken::new())
        .with_outbox(outbox)
        .with_owner(owner)
        .with_workspaces(workspaces)
}

#[tokio::test]
async fn a_patch_outlives_the_process_that_produced_it() {
    let (_repo, state, root) = writable().await;
    let outbox = Arc::new(MemoryOutbox::default());
    let owner: OwnerHandle = Arc::new(std::sync::Mutex::new(Some(Ulid::new())));
    let control = restarted(
        outbox.clone(),
        Arc::clone(&owner),
        judged(&root, &state, None),
    );
    let session = run(&control, writer("rewrite a.txt"), 5)
        .await
        .unwrap()
        .session;

    // A fresh control plane: same log, no in-memory registry. The worktree
    // is long gone, so if the patch is not in the record it is not anywhere.
    let after = restarted(
        outbox.clone(),
        Arc::clone(&owner),
        judged(&root, &state, None),
    );
    assert!(after.cached_patch(&session).is_none());
    let recovered = after.pending(&session).await.expect("patch survived");
    assert!(recovered.patch.diff.contains("rewritten by the child"));
    assert_eq!(after.outstanding().await.len(), 1);

    after.merge(&recovered.patch, false).await.unwrap();
    // Closed by handout: the session may hold several once followed up.
    after.forget(&recovered.dispatch).await;
    // And once applied it stays applied — a later run must not offer it
    // again, or the same patch lands twice.
    let later = restarted(outbox, owner, judged(&root, &state, None));
    assert!(later.outstanding().await.is_empty());
}

#[tokio::test]
async fn finished_children_stay_visible_after_they_leave_the_roster() {
    let (_repo, state, root) = writable().await;
    let control = deliverable(Arc::new(Edits), CancellationToken::new())
        .with_workspaces(judged(&root, &state, None));
    run(&control, writer("rewrite a.txt"), 5).await.unwrap();

    // "What is running" and "what happened" are different questions; a settled
    // child leaves the first and stays answerable to the second.
    assert!(control.active().is_empty());
    let agents = control.agents();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].state, State::Settled(AgentStatus::Completed));
    assert_eq!(agents[0].objective, "rewrite a.txt");
}

#[tokio::test]
async fn durable_session_ids_keep_transcripts_addressable_after_eviction_and_restart() {
    let outbox = Arc::new(MemoryOutbox::default());
    let runner = Arc::new(Recorder {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let owner = Ulid::new();
    let control =
        AgentControl::new(runner.clone(), CancellationToken::new()).with_outbox(outbox.clone());
    control.adopt(owner);

    let mut oldest = None;
    for index in 0..100 {
        let result = run(&control, spec(&format!("agent number {index}")), 2)
            .await
            .unwrap();
        oldest.get_or_insert(result.session);
    }
    let oldest = oldest.unwrap();
    assert!(
        control.agents().iter().all(|agent| agent.session != oldest),
        "the test did not exceed the settled-roster cache"
    );
    assert_eq!(
        control
            .transcript(&AgentPath::root(), &oldest)
            .await
            .unwrap(),
        [format!("transcript:{oldest}")]
    );

    let restarted = AgentControl::new(runner, CancellationToken::new()).with_outbox(outbox.clone());
    assert!(restarted.agents().is_empty());
    assert_eq!(
        restarted
            .transcript(&AgentPath::root(), &oldest)
            .await
            .unwrap(),
        [format!("transcript:{oldest}")]
    );
    assert!(
        restarted
            .transcript(&AgentPath::parse("/another-agent").unwrap(), &oldest)
            .await
            .is_err(),
        "a descendant must not bypass reachability with a learned session id"
    );
}

#[tokio::test]
async fn a_failed_child_is_recorded_not_forgotten() {
    let (_, control) = control();
    let control = deliverable(Arc::new(Hangs), control.cancel.clone());
    control.cancel.cancel();
    run(&control, spec("will be cancelled"), 5).await.unwrap();
    let agents = control.agents();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].state, State::Settled(AgentStatus::Cancelled));
}

#[test]
fn the_weakest_trust_a_child_touched_is_what_it_reports() {
    // A child that read the web hands back a web-labelled result, so
    // delegation cannot launder taint into a trusted-looking summary.
    assert_eq!(
        least_trusted([TrustLabel::User, TrustLabel::Web, TrustLabel::Tool]),
        TrustLabel::Web
    );
    assert_eq!(
        least_trusted([TrustLabel::User, TrustLabel::System]),
        TrustLabel::System
    );
    assert_eq!(least_trusted([]), TrustLabel::Tool);
}
