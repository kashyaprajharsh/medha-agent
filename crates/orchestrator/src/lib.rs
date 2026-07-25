//! Sub-agent runtime (Stage 3, slice O1): a child agent is an independently
//! managed session, not a prompt trick.
//!
//! A child is built ad hoc from an objective — there are no preset agent files.
//! It gets a fresh session id, so Medha's event log already gives it a durable,
//! resumable, independently addressable transcript; the parent receives only a
//! bounded structured result. Capability narrowing is enforced in
//! [`NarrowedExecutor`], capacity is reserved before a spawn is published, and
//! cancellation cascades from the parent's token.
//!
//! O1 children are read-only and depth-limited: writer isolation (§6.4) does not
//! exist yet, so two children must not be able to touch one workspace.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kernel::{Executor, TrustLabel};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

mod narrow;
pub use narrow::NarrowedExecutor;

/// Ceiling on children alive at once. Each child burns tokens independently, so
/// this is a spend bound as much as a concurrency one.
pub const DEFAULT_MAX_ACTIVE: usize = 3;
/// How deep delegation may nest. O1 is flat: a child cannot spawn a child.
pub const DEFAULT_MAX_DEPTH: usize = 1;
/// Summary text handed back to the parent before the rest spills to an artifact.
pub const MAX_SUMMARY_CHARS: usize = 16_000;

#[derive(Debug, Error)]
pub enum Error {
    #[error("sub-agents are not available in this session")]
    Unavailable,
    #[error("agent objective is empty")]
    NoObjective,
    #[error("delegation depth {depth} exceeds the limit of {max}")]
    TooDeep { depth: usize, max: usize },
    #[error("too many agents already running (limit {0}); wait for one to finish")]
    AtCapacity(usize),
    #[error("agent failed: {0}")]
    Failed(String),
}

/// What the parent asked a child to do. Built at spawn time — the objective and
/// the output contract *are* the definition, so no agent has to be registered
/// before it can be used.
#[derive(Debug, Clone, Default)]
pub struct AgentSpec {
    /// Short human-readable label, for the UI and for addressing the agent.
    pub name: String,
    pub objective: String,
    /// What the result must contain. Carried into the child's system prompt so
    /// the summary the parent gets back is shaped, not free-form.
    pub contract: Option<String>,
    /// Tools to narrow to. `None` inherits the parent's set — which is still
    /// only the parent's set.
    pub tools: Option<Vec<String>>,
    /// Turn ceiling for this child, clamped to the parent's remaining budget.
    pub max_turns: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Completed,
    /// Stopped on a budget ceiling; partial work is still reported.
    Exhausted,
    Failed,
    Cancelled,
}

/// What the parent sees. Never the child's transcript — that stays in the event
/// log under the child's own session id, durable and resumable.
///
/// Round-trips through the log: a background report is written when the child
/// finishes and read back when its owner next runs, possibly in another process.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct AgentResult {
    pub agent: String,
    pub session: String,
    pub status: AgentStatus,
    pub summary: String,
    /// Set when the summary was too long for context; the whole thing is here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    pub turns: u32,
    pub tool_calls: u32,
    pub duration_ms: u64,
    /// The least-trusted content this child touched. Medha labels every
    /// observation, so a child that read a web page hands back a `web`-labelled
    /// result and the kernel's existing trust-flow escalation still applies —
    /// delegation cannot launder taint into a trusted-looking summary.
    pub trust: TrustLabel,
}

/// One live child.
#[derive(Debug, Clone, Serialize)]
pub struct AgentHandle {
    pub agent: String,
    pub session: String,
    pub objective: String,
    pub started_ms: u64,
}

/// How the runtime actually runs a child. Implemented outside this crate by
/// whoever owns a `Kernel`, which keeps the orchestrator free of the kernel's
/// provider/log type parameters — and free of a dependency cycle with the tool
/// registry that hosts `agent.spawn`.
#[async_trait::async_trait]
pub trait ChildRunner: Send + Sync {
    async fn run(&self, run: ChildRun) -> Result<ChildOutcome, String>;
}

/// Everything a runner needs to execute one child.
pub struct ChildRun {
    pub session: Ulid,
    pub spec: AgentSpec,
    /// Already narrowed — the runner must use exactly this, never the parent's.
    pub executor: Arc<dyn Executor>,
    pub max_turns: u32,
    pub cancel: CancellationToken,
}

/// Raw result of a child run, before it is bounded and labelled.
pub struct ChildOutcome {
    pub status: AgentStatus,
    pub summary: String,
    pub turns: u32,
    pub tool_calls: u32,
    /// Least-trusted label seen across the child's observations.
    pub trust: TrustLabel,
}

/// A dispatched agent, recorded before any work starts.
///
/// `parent` is captured here and never re-derived. Resolving "the current
/// session" when a child finishes is a known failure mode: after a reset or
/// restart the newest session is a different conversation, and the result lands
/// in someone else's chat.
#[derive(Debug, Clone)]
pub struct Dispatch {
    pub agent: String,
    pub child: Ulid,
    pub parent: Ulid,
    pub objective: String,
}

/// Durable delivery for background agents: `pending → claimed → delivered`.
///
/// A background child outlives the turn that asked for it, so its result has to
/// survive the process. Implemented over Medha's event log, which is append-only
/// and already per-session, so the outbox is a fold rather than new storage.
#[async_trait::async_trait]
pub trait Outbox: Send + Sync {
    /// Persist the dispatch *before* the child starts. A crash between here and
    /// completion leaves a visible orphan rather than silent loss.
    async fn dispatched(&self, dispatch: &Dispatch);
    /// Record a terminal result against its dispatch.
    async fn finished(&self, dispatch: &Dispatch, result: &AgentResult);
    /// Mark a result handed to its owner. Delivery is idempotent: replaying the
    /// log must not inject the same report twice. Takes `parent` because the
    /// record belongs on the owner's chain, not the child's.
    async fn delivered(&self, parent: Ulid, child: Ulid);
    /// Results owned by `parent` that have not been delivered, oldest first.
    async fn undelivered(&self, parent: Ulid) -> Vec<AgentResult>;
}

/// A runner installed after construction. The kernel owns the executor that
/// hosts `agent.spawn`, so the tool must exist before the kernel does; filling
/// this in once afterwards breaks that cycle without a `Weak` dance at every
/// call site.
#[derive(Default)]
pub struct DeferredRunner(std::sync::OnceLock<Arc<dyn ChildRunner>>);

impl DeferredRunner {
    /// Install the real runner. Later calls are ignored, so a second install
    /// cannot swap the runtime out from under a running tree.
    pub fn install(&self, runner: Arc<dyn ChildRunner>) {
        let _ = self.0.set(runner);
    }
}

#[async_trait::async_trait]
impl ChildRunner for DeferredRunner {
    async fn run(&self, run: ChildRun) -> Result<ChildOutcome, String> {
        match self.0.get() {
            Some(runner) => runner.run(run).await,
            None => Err(Error::Unavailable.to_string()),
        }
    }
}

/// Control plane for one session tree. Held by the parent session and shared
/// with its descendants, so caps apply to the tree rather than per-call.
pub struct AgentControl {
    runner: Arc<dyn ChildRunner>,
    /// Capacity is a permit taken *before* the spawn is announced, so an
    /// over-subscribed tree is rejected rather than silently queued.
    capacity: Arc<Semaphore>,
    max_active: usize,
    max_depth: usize,
    depth: usize,
    cancel: CancellationToken,
    active: Roster,
    outbox: Option<Arc<dyn Outbox>>,
    /// Owns every backgrounded child, so shutdown can wait for them instead of
    /// leaving detached tasks running (§2.1: no untracked `tokio::spawn`).
    tasks: tokio_util::task::TaskTracker,
}

/// The live roster. Critical sections are a push, a retain and a clone, none of
/// them across an await, so a plain mutex is both correct and what lets the
/// registration guard clean up from `Drop`.
type Roster = Arc<std::sync::Mutex<Vec<Running>>>;

/// A child currently executing, with the handle to stop it. Cancelling one agent
/// must not touch its siblings, so the token is per-child rather than the tree's.
struct Running {
    handle: AgentHandle,
    cancel: CancellationToken,
}

impl AgentControl {
    pub fn new(runner: Arc<dyn ChildRunner>, cancel: CancellationToken) -> Self {
        Self {
            runner,
            capacity: Arc::new(Semaphore::new(DEFAULT_MAX_ACTIVE)),
            max_active: DEFAULT_MAX_ACTIVE,
            max_depth: DEFAULT_MAX_DEPTH,
            depth: 0,
            cancel,
            active: Arc::new(std::sync::Mutex::new(Vec::new())),
            outbox: None,
            tasks: tokio_util::task::TaskTracker::new(),
        }
    }

    pub fn with_limits(mut self, max_active: usize, max_depth: usize) -> Self {
        self.max_active = max_active.max(1);
        self.max_depth = max_depth;
        self.capacity = Arc::new(Semaphore::new(self.max_active));
        self
    }

    pub fn max_depth(&self) -> usize {
        self.max_depth
    }

    /// Attach durable delivery. Without it background spawn is refused, because
    /// a result that cannot outlive the process is not a background result.
    pub fn with_outbox(mut self, outbox: Arc<dyn Outbox>) -> Self {
        self.outbox = Some(outbox);
        self
    }

    /// Currently running children, for the UI and `agent.list`.
    pub fn active(&self) -> Vec<AgentHandle> {
        self.active
            .lock()
            .map(|active| active.iter().map(|run| run.handle.clone()).collect())
            .unwrap_or_default()
    }

    /// Stop one agent by name or session id, leaving its siblings running.
    /// Returns the agents actually signalled — a name can be ambiguous only if
    /// the roster is stale, since live display names are deduplicated.
    pub fn cancel(&self, id: &str) -> Vec<String> {
        let Ok(active) = self.active.lock() else {
            return Vec::new();
        };
        active
            .iter()
            .filter(|run| run.handle.agent == id || run.handle.session == id)
            .map(|run| {
                run.cancel.cancel();
                run.handle.agent.clone()
            })
            .collect()
    }

    /// Results finished but not yet handed to `parent`, marked delivered as they
    /// are taken. Idempotent by construction: a delivered result is never
    /// returned twice, so a replayed log cannot duplicate a report.
    pub async fn collect(&self, parent: Ulid) -> Vec<AgentResult> {
        let Some(outbox) = &self.outbox else {
            return Vec::new();
        };
        let ready = outbox.undelivered(parent).await;
        for result in &ready {
            if let Ok(child) = result.session.parse() {
                outbox.delivered(parent, child).await;
            }
        }
        ready
    }

    /// Reserve capacity before anything is published. Returns `AtCapacity`
    /// rather than queueing: an unbounded backlog of children is worse than a
    /// refusal the model can react to.
    fn reserve(&self) -> Result<OwnedSemaphorePermit, Error> {
        Arc::clone(&self.capacity)
            .try_acquire_owned()
            .map_err(|_| Error::AtCapacity(self.max_active))
    }

    /// Everything both spawn paths need before any work starts: the checks, a
    /// capacity permit, the narrowed executor and the roster entry.
    fn admit(
        &self,
        spec: &mut AgentSpec,
        parent_executor: Arc<dyn Executor>,
        remaining_turns: u32,
    ) -> Result<Admitted, Error> {
        if spec.objective.trim().is_empty() {
            return Err(Error::NoObjective);
        }
        if self.depth >= self.max_depth {
            return Err(Error::TooDeep {
                depth: self.depth,
                max: self.max_depth,
            });
        }
        let permit = self.reserve()?;

        if spec.name.trim().is_empty() {
            spec.name = default_name(&spec.objective);
        }
        // Read-only: writer isolation does not exist yet (§6.4), so a child that
        // could mutate the workspace could corrupt its parent's.
        let executor: Arc<dyn Executor> =
            Arc::new(NarrowedExecutor::new(parent_executor, spec.tools.as_deref()).read_only());
        // A child's budget is a slice of what the parent has left, never more.
        let max_turns = spec
            .max_turns
            .unwrap_or(remaining_turns)
            .min(remaining_turns)
            .max(1);

        let session = Ulid::new();
        // Per-child token, so cancelling one agent leaves its siblings alone;
        // it is a child of the tree's, so cancelling the tree still settles all.
        let cancel = self.cancel.child_token();
        {
            // The session id is the identity; the name is display only. Two
            // children with the same objective must still be distinguishable,
            // so a collision gets a suffix rather than silently aliasing (§6.2).
            let mut active = self.active.lock().map_err(|_| Error::Unavailable)?;
            if active.iter().any(|run| run.handle.agent == spec.name) {
                spec.name = format!("{}-{}", spec.name, &session.to_string()[..4].to_lowercase());
            }
            active.push(Running {
                handle: AgentHandle {
                    agent: spec.name.clone(),
                    session: session.to_string(),
                    objective: spec.objective.clone(),
                    started_ms: epoch_ms(),
                },
                cancel: cancel.clone(),
            });
        }
        Ok(Admitted {
            session,
            cancel,
            executor,
            max_turns,
            permit,
            registration: Registration {
                active: Arc::clone(&self.active),
                session: session.to_string(),
            },
        })
    }

    /// Run one child to completion and return its bounded result. Foreground
    /// spawn waits (§6.2).
    pub async fn spawn(
        &self,
        mut spec: AgentSpec,
        parent_executor: Arc<dyn Executor>,
        remaining_turns: u32,
    ) -> Result<AgentResult, Error> {
        let admitted = self.admit(&mut spec, parent_executor, remaining_turns)?;
        Ok(execute(Arc::clone(&self.runner), spec, admitted).await)
    }

    /// Dispatch a child that outlives this turn and return its handle at once.
    ///
    /// The dispatch is persisted *before* the child starts and carries `parent`
    /// as given, so the report reaches the session that asked for it even across
    /// a restart — never "whichever session is current when it finishes".
    pub async fn spawn_background(
        &self,
        mut spec: AgentSpec,
        parent_executor: Arc<dyn Executor>,
        remaining_turns: u32,
        parent: Ulid,
    ) -> Result<AgentHandle, Error> {
        // No durable delivery means no background: a result that cannot outlive
        // the process is not a background result, it is a lost one.
        let outbox = Arc::clone(self.outbox.as_ref().ok_or(Error::Unavailable)?);
        let admitted = self.admit(&mut spec, parent_executor, remaining_turns)?;
        let handle = AgentHandle {
            agent: spec.name.clone(),
            session: admitted.session.to_string(),
            objective: spec.objective.clone(),
            started_ms: epoch_ms(),
        };
        let dispatch = Dispatch {
            agent: spec.name.clone(),
            child: admitted.session,
            parent,
            objective: spec.objective.clone(),
        };
        outbox.dispatched(&dispatch).await;

        let runner = Arc::clone(&self.runner);
        self.tasks.spawn(async move {
            let result = execute(runner, spec, admitted).await;
            outbox.finished(&dispatch, &result).await;
        });
        Ok(handle)
    }

    /// Stop accepting work and let backgrounded children finish. Use when the
    /// caller wants their reports — session exit should prefer [`Self::shutdown`].
    pub async fn drain(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }

    /// Cancel every child and wait for them to settle. Their partial results are
    /// still persisted, so what a child had found is delivered on the next run
    /// rather than lost with it.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        self.drain().await;
    }

    /// A control for a child of this one: one deeper, sharing the tree's
    /// capacity, roster and cancellation. Caps are tree-scoped (§2.1), so a
    /// descendant cannot escape them by holding its own control.
    pub fn for_child(&self) -> Self {
        Self {
            runner: Arc::clone(&self.runner),
            capacity: Arc::clone(&self.capacity),
            max_active: self.max_active,
            max_depth: self.max_depth,
            depth: self.depth + 1,
            cancel: self.cancel.child_token(),
            active: Arc::clone(&self.active),
            outbox: self.outbox.clone(),
            tasks: self.tasks.clone(),
        }
    }
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A child cleared to run: capacity held, executor narrowed, roster entry made.
struct Admitted {
    session: Ulid,
    cancel: CancellationToken,
    executor: Arc<dyn Executor>,
    max_turns: u32,
    /// Held for the child's lifetime; dropping it frees a slot in the tree.
    permit: OwnedSemaphorePermit,
    registration: Registration,
}

/// The one execution path, shared by foreground and background spawn so they
/// cannot drift in how they cancel, time or bound a child.
async fn execute(runner: Arc<dyn ChildRunner>, spec: AgentSpec, admitted: Admitted) -> AgentResult {
    let Admitted {
        session,
        cancel,
        executor,
        max_turns,
        permit,
        registration,
    } = admitted;
    let started = Instant::now();
    let run = runner.run(ChildRun {
        session,
        spec: spec.clone(),
        executor,
        max_turns,
        cancel: cancel.clone(),
    });
    // Race the run against the token. A cooperative runner honours it itself,
    // but one that ignores it must not outlive a cancellation, so dropping the
    // future here is the backstop.
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(None),
        outcome = run => outcome.map_err(Some),
    };
    // Both guards outlive the run deliberately: the slot and the roster entry
    // are released together, after the child is genuinely finished.
    drop(registration);
    drop(permit);

    let elapsed = started.elapsed();
    match outcome {
        Ok(outcome) => bound(spec.name, session, outcome, elapsed),
        // Partial-result preservation (§6.5): a cancelled or failed child still
        // reports, so the parent learns what happened rather than nothing.
        Err(reason) => bound(
            spec.name,
            session,
            ChildOutcome {
                status: match &reason {
                    Some(_) => AgentStatus::Failed,
                    None => AgentStatus::Cancelled,
                },
                summary: reason.unwrap_or_else(|| "cancelled before reporting".into()),
                turns: 0,
                tool_calls: 0,
                trust: TrustLabel::Tool,
            },
            elapsed,
        ),
    }
}

/// Keeps a child in the visible roster for exactly as long as it runs. A guard
/// rather than a pair of calls because the run future can be dropped mid-flight
/// on cancellation, which would otherwise strand the entry forever.
struct Registration {
    active: Roster,
    session: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        // A plain mutex, not an async one: `Drop` cannot await, and a `try_lock`
        // that loses a race would strand the row permanently. The roster is
        // never held across an await, so locking here cannot block meaningfully.
        if let Ok(mut active) = self.active.lock() {
            active.retain(|run| run.handle.session != self.session);
        }
    }
}

/// Assemble the parent-facing record. The summary is returned **whole**: the
/// caller owns the artifact store, so it caps for context and spills the
/// remainder there. Truncating here would discard the tail before anyone could
/// persist it.
fn bound(name: String, session: Ulid, outcome: ChildOutcome, elapsed: Duration) -> AgentResult {
    AgentResult {
        agent: name,
        session: session.to_string(),
        status: outcome.status,
        summary: outcome.summary,
        artifact: None,
        turns: outcome.turns,
        tool_calls: outcome.tool_calls,
        duration_ms: elapsed.as_millis() as u64,
        trust: outcome.trust,
    }
}

/// A readable fallback name from the objective, so every agent is addressable
/// even when the model does not name it.
fn default_name(objective: &str) -> String {
    let slug: String = objective
        .split_whitespace()
        .take(4)
        .map(|word| {
            word.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    if slug.is_empty() {
        "agent".to_string()
    } else {
        slug
    }
}

/// Rank trust from most to least trusted, so a child's result can report the
/// weakest thing it touched.
pub fn least_trusted(labels: impl IntoIterator<Item = TrustLabel>) -> TrustLabel {
    fn rank(label: TrustLabel) -> u8 {
        match label {
            TrustLabel::User => 0,
            TrustLabel::System => 1,
            TrustLabel::Skill => 2,
            TrustLabel::Workspace => 3,
            TrustLabel::Memory => 4,
            TrustLabel::Tool => 5,
            TrustLabel::Web => 6,
        }
    }
    labels
        .into_iter()
        .max_by_key(|label| rank(*label))
        .unwrap_or(TrustLabel::Tool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kernel::{BlastRadius, Observation, ToolCategory, ToolIntent, ToolSpec};
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
            self.seen.lock().unwrap().push((tools, run.max_turns));
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
        let control = AgentControl::new(recorder.clone(), CancellationToken::new());
        (recorder, control)
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
        let result = control
            .spawn(spec("summarise the crate"), Arc::new(Tools), 5)
            .await
            .unwrap();
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
        control.spawn(wants_more, Arc::new(Tools), 4).await.unwrap();
        assert_eq!(recorder.seen.lock().unwrap()[0].1, 4);
    }

    #[tokio::test]
    async fn capacity_is_refused_rather_than_queued() {
        let (_, control) = control();
        let control = control.with_limits(1, 1);
        // Hold the only permit, then prove a second spawn is refused outright.
        let held = control.reserve().unwrap();
        assert!(matches!(
            control.spawn(spec("second"), Arc::new(Tools), 5).await,
            Err(Error::AtCapacity(1))
        ));
        drop(held);
        assert!(
            control
                .spawn(spec("second"), Arc::new(Tools), 5)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn delegation_is_flat_until_writer_isolation_exists() {
        let (_, mut control) = control();
        control.depth = 1;
        assert!(matches!(
            control.spawn(spec("nested"), Arc::new(Tools), 5).await,
            Err(Error::TooDeep { .. })
        ));
    }

    #[tokio::test]
    async fn an_empty_objective_is_rejected() {
        let (_, control) = control();
        assert!(matches!(
            control.spawn(spec("   "), Arc::new(Tools), 5).await,
            Err(Error::NoObjective)
        ));
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
        let control = AgentControl::new(Arc::new(Hangs), cancel.clone());
        cancel.cancel();
        let result = control
            .spawn(spec("will be cancelled"), Arc::new(Tools), 5)
            .await
            .unwrap();
        assert_eq!(result.status, AgentStatus::Cancelled);
        // The run future was dropped mid-flight; the roster must still be clean.
        assert!(control.active().is_empty());
    }

    #[tokio::test]
    async fn a_child_control_is_deeper_and_shares_the_trees_capacity() {
        let (_, control) = control();
        let control = control.with_limits(2, 1);
        let child = control.for_child();
        // Depth is exhausted one level down, so O1 delegation stays flat.
        assert!(matches!(
            child.spawn(spec("nested"), Arc::new(Tools), 5).await,
            Err(Error::TooDeep { .. })
        ));
        // Capacity is one semaphore for the whole tree, not one per control.
        let held = control.reserve().unwrap();
        let _also_held = control.reserve().unwrap();
        assert!(matches!(child.reserve(), Err(Error::AtCapacity(2))));
        drop(held);
        assert!(child.reserve().is_ok());
    }

    #[tokio::test]
    async fn concurrent_agents_never_share_a_display_name() {
        let (_, control) = control();
        // Occupy the roster with the name the next spawn would pick.
        control.active.lock().unwrap().push(Running {
            handle: AgentHandle {
                agent: "audit-the-crate".into(),
                session: Ulid::new().to_string(),
                objective: "audit the crate".into(),
                started_ms: epoch_ms(),
            },
            cancel: CancellationToken::new(),
        });
        let result = control
            .spawn(spec("audit the crate"), Arc::new(Tools), 5)
            .await
            .unwrap();
        assert_ne!(result.agent, "audit-the-crate");
        assert!(result.agent.starts_with("audit-the-crate-"));
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
        let control = AgentControl::new(Arc::new(Verbose), CancellationToken::new());
        let result = control
            .spawn(spec("write at length"), Arc::new(Tools), 5)
            .await
            .unwrap();
        // The runtime must not truncate: the caller owns the artifact store, so
        // trimming here would discard the tail before anything could persist it.
        assert_eq!(result.summary.len(), MAX_SUMMARY_CHARS * 2);
    }

    /// In-memory stand-in for the event-log outbox, with the same state machine.
    #[derive(Default)]
    struct MemoryOutbox {
        rows: std::sync::Mutex<Vec<(Dispatch, Option<AgentResult>, bool)>>,
    }

    #[async_trait]
    impl Outbox for MemoryOutbox {
        async fn dispatched(&self, dispatch: &Dispatch) {
            self.rows
                .lock()
                .unwrap()
                .push((dispatch.clone(), None, false));
        }
        async fn finished(&self, dispatch: &Dispatch, result: &AgentResult) {
            if let Some(row) = self
                .rows
                .lock()
                .unwrap()
                .iter_mut()
                .find(|(d, _, _)| d.child == dispatch.child)
            {
                row.1 = Some(result.clone());
            }
        }
        async fn delivered(&self, _parent: Ulid, child: Ulid) {
            if let Some(row) = self
                .rows
                .lock()
                .unwrap()
                .iter_mut()
                .find(|(d, _, _)| d.child == child)
            {
                row.2 = true;
            }
        }
        async fn undelivered(&self, parent: Ulid) -> Vec<AgentResult> {
            self.rows
                .lock()
                .unwrap()
                .iter()
                .filter(|(d, result, delivered)| {
                    d.parent == parent && result.is_some() && !delivered
                })
                .filter_map(|(_, result, _)| result.clone())
                .collect()
        }
    }

    fn background_control() -> (Arc<MemoryOutbox>, AgentControl) {
        let outbox = Arc::new(MemoryOutbox::default());
        let control = AgentControl::new(
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
            .spawn_background(spec("survey"), Arc::new(Tools), 5, asked)
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

    #[tokio::test]
    async fn a_delivered_result_is_never_handed_over_twice() {
        let (_outbox, control) = background_control();
        let parent = Ulid::new();
        control
            .spawn_background(spec("survey"), Arc::new(Tools), 5, parent)
            .await
            .unwrap();
        control.drain().await;

        assert_eq!(control.collect(parent).await.len(), 1);
        // Replaying must not inject the same report again.
        assert!(control.collect(parent).await.is_empty());
    }

    #[tokio::test]
    async fn background_is_refused_without_durable_delivery() {
        let (_, control) = control();
        // No outbox: the result could not survive the process, so accepting the
        // spawn would be promising a delivery that cannot happen.
        assert!(matches!(
            control
                .spawn_background(spec("survey"), Arc::new(Tools), 5, Ulid::new())
                .await,
            Err(Error::Unavailable)
        ));
    }

    #[tokio::test]
    async fn a_cancelled_background_agent_still_reports() {
        let control = AgentControl::new(Arc::new(Hangs), CancellationToken::new())
            .with_outbox(Arc::new(MemoryOutbox::default()));
        let parent = Ulid::new();
        control
            .spawn_background(spec("never finishes"), Arc::new(Tools), 5, parent)
            .await
            .unwrap();
        control.shutdown().await;

        // Partial-result preservation (§6.5): killing a child must not lose the
        // fact that it ran, or the parent is left waiting on nothing.
        let reported = control.collect(parent).await;
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].status, AgentStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancelling_one_agent_leaves_its_siblings_running() {
        let control = AgentControl::new(Arc::new(Hangs), CancellationToken::new())
            .with_outbox(Arc::new(MemoryOutbox::default()))
            .with_limits(4, 1);
        let parent = Ulid::new();
        let first = control
            .spawn_background(spec("first task"), Arc::new(Tools), 5, parent)
            .await
            .unwrap();
        control
            .spawn_background(spec("second task"), Arc::new(Tools), 5, parent)
            .await
            .unwrap();

        let stopped = control.cancel(&first.session);
        assert_eq!(stopped, vec![first.agent.clone()]);
        // The sibling is untouched — a per-child token, not the tree's.
        let still_running = control.active();
        assert!(still_running.iter().any(|run| run.agent != first.agent));
        control.shutdown().await;
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
}
