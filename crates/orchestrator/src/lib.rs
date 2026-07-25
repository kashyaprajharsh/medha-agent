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
//! Children are read-only by default. A child that must modify code (O3) is
//! given its own git worktree and returns a patch; §6.4 forbids two writers
//! sharing one workspace, so a writer without isolation is refused outright
//! rather than quietly downgraded to editing the parent's tree.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kernel::{Executor, TrustLabel};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

mod narrow;
pub mod worktree;
pub use narrow::NarrowedExecutor;
pub use worktree::{MergeCheck, Patch, Verification, Worktree, WorktreePool};

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
    #[error("no agent '{0}'")]
    UnknownAgent(String),
    #[error("agent failed: {0}")]
    Failed(String),
    #[error(
        "a writing agent needs an isolated worktree, which is unavailable here: {0}. \
         Delegate the investigation read-only and make the edits yourself."
    )]
    NoIsolation(String),
    #[error(
        "this patch does not build — `{0}` failed against it, so it was not applied. \
         Read the failure and fix it, or re-run the agent; apply it unchanged only if \
         you have established the failure is pre-existing and unrelated."
    )]
    Unverified(String),
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
    /// Whether this child may modify code (O3). A writer runs in its own git
    /// worktree and hands back a patch; it never edits the parent's tree, and
    /// if isolation cannot be arranged the spawn is refused.
    pub write: bool,
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
    /// What a writer changed, as a diff against the commit it started from,
    /// with whatever evidence it has that the change works. `None` for a
    /// read-only child. Present but empty when a writer changed nothing —
    /// which is a result, not a failure.
    ///
    /// This is deliberately not a prose account of the edits: a summary of a
    /// change cannot be reviewed, verified or applied, and the merge gate needs
    /// all three.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<Patch>,
}

/// One live child.
#[derive(Debug, Clone, Serialize)]
pub struct AgentHandle {
    pub agent: String,
    pub session: String,
    pub objective: String,
    pub started_ms: u64,
    /// Whether the caller is waiting on this one. A foreground result is
    /// returned inline, so announcing its completion separately would be both
    /// redundant and wrong about where the report went.
    pub background: bool,
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
    /// For a writer it is also already *rooted at the worktree*.
    pub executor: Arc<dyn Executor>,
    pub max_turns: u32,
    pub cancel: CancellationToken,
    /// A writer's isolated checkout. The runner must make this the child's cwd:
    /// the executor is rooted here, and a child whose working directory still
    /// points at the parent would resolve relative paths into the tree the
    /// isolation exists to protect.
    pub workspace: Option<std::path::PathBuf>,
    /// Steer queue for this child. The runner must hand it to the session loop
    /// — text queued against a child whose runner drops this is accepted and
    /// then silently lost, which is worse than refusing it.
    pub interrupts: kernel::InterruptQueue,
}

/// An isolated checkout, together with the tools rooted at it.
///
/// Both halves have to come from one place. Handing a child a worktree but the
/// parent's executor would isolate nothing — the tools would still resolve
/// paths against the parent's root — and that failure is invisible until
/// something is already overwritten.
pub struct Workspace {
    pub worktree: Worktree,
    /// The parent's tools, rebased onto the worktree. Same capabilities, a
    /// different root; never a wider set.
    pub executor: Arc<dyn Executor>,
}

/// Supplies writer isolation. Implemented outside this crate, because building
/// an executor over a new sandbox root needs the tool registry — which depends
/// on this crate.
#[async_trait::async_trait]
pub trait Workspaces: Send + Sync {
    /// Cut an isolated checkout for `session`, with tools rooted at it.
    async fn checkout(&self, session: Ulid) -> Result<Workspace, String>;

    /// Run the project's verification command inside `root`, if one is
    /// configured. The orchestrator runs this itself rather than trusting the
    /// child's account: "I ran the tests and they passed" is exactly the claim
    /// a merge gate cannot afford to take on faith.
    async fn verify(&self, root: &Path) -> Option<Verification>;

    /// Where a patch merges back to — the repository root.
    fn repo(&self) -> std::path::PathBuf;
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

/// Durable delivery for background agents: `dispatched → finished → delivered`.
///
/// A background child outlives the turn that asked for it, so its result has to
/// survive the process. Implemented over Medha's event log, which is append-only
/// and already per-session, so the outbox is a fold rather than new storage.
///
/// Delivery is at-least-once, not exactly-once: [`Self::undelivered`] reads and
/// the caller then marks, so a crash in between replays a report rather than
/// dropping one. That is the safe direction to fail in, and a lease would buy
/// exactly-once only against a second concurrent reader of the same session —
/// which is not a configuration Medha has.
///
/// The state machine has a fourth path that is easy to miss: a dispatch whose
/// owning process died leaves no terminal event at all, and would otherwise sit
/// unresolved forever. [`Self::reap_abandoned`] is what closes it.
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

    /// What a child actually did, newest last. A report is a summary by design,
    /// so when one is thin or wrong the only honest recourse is to look at the
    /// work — without this the caller is left guessing at a transcript that
    /// exists but is unreachable.
    async fn transcript(&self, child: Ulid) -> Vec<String>;

    /// Persist a writer's patch against `parent`.
    ///
    /// The child's worktree is reaped as soon as the diff is taken, so this
    /// record is the only surviving copy of the work. An in-memory registry
    /// alone would mean a background writer finishing, the process exiting, and
    /// the patch being silently lost while its report survived — the worst
    /// shape of failure, because the transcript says the work was done.
    async fn recorded(&self, parent: Ulid, agent: &str, child: Ulid, patch: &Patch);

    /// Mark a patch merged. `pending → applied`, mirroring delivery, so a
    /// restart does not offer already-applied work as outstanding.
    async fn applied(&self, parent: Ulid, child: Ulid);

    /// Patches owned by `parent` that have not been applied, oldest first, as
    /// `(agent, child session, patch)`.
    async fn unapplied(&self, parent: Ulid) -> Vec<(String, String, Patch)>;

    /// When `child` last recorded anything, as epoch seconds.
    ///
    /// A child appends an event per step, so the newest one *is* its heartbeat —
    /// no separate signal to keep in sync with the work. Without it a running
    /// agent that is thinking, one stuck in a retry loop, and one wedged on a
    /// tool that will never return all look identical: a name and a spinner.
    async fn last_activity(&self, child: Ulid) -> Option<f64>;

    /// Resolve children whose owning process died before they could report.
    ///
    /// A dispatch is written before the child starts, so a crash in between
    /// leaves a dispatch with no terminal event. Nothing else in the fold looks
    /// at those: `undelivered` only returns rows that *have* a terminal event,
    /// so without this the parent waits on a child that will never report —
    /// silently, and forever.
    ///
    /// Records a terminal result of unknown outcome, which is the honest one:
    /// the child may have finished its work, may have half-finished it, and
    /// nothing survived to say which. Returns how many it closed. Idempotent —
    /// the terminal event it writes is what stops a second pass re-reaping.
    async fn reap_abandoned(&self, parent: Ulid) -> usize;
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
    /// Writer isolation. `None` means writers are refused: without it a writing
    /// child would edit the parent's tree, which is the one thing §6.4 exists
    /// to prevent.
    workspaces: Option<Arc<dyn Workspaces>>,
    /// Patches from writers that have finished, so a merge can be asked for by
    /// agent id instead of by pasting a diff back through the model — which
    /// would put a whitespace-sensitive artifact through a lossy channel.
    /// A cache over the log, not the record: [`Self::outstanding`] reads both.
    patches: Patches,
    /// The session that owns this tree, for addressing durable records. Filled
    /// by the surface once the session id exists — the same deferred-handle
    /// shape the parent executor uses, for the same reason.
    owner: OwnerHandle,
    /// Signalled when a background report becomes collectable.
    notifier: NotifierHandle,
    /// Children that have finished, newest last and bounded. The live roster
    /// drops an entry the moment its child settles, which is correct for "what
    /// is running" and useless for "what happened" — §6.6 asks the TUI for both.
    history: History,
    /// Owns every backgrounded child, so shutdown can wait for them instead of
    /// leaving detached tasks running (§2.1: no untracked `tokio::spawn`).
    tasks: tokio_util::task::TaskTracker,
}

/// Finished writers' patches, newest last, bounded. Shared with descendants so
/// a patch can be applied from anywhere in the tree.
type Patches = Arc<std::sync::Mutex<Vec<Delivered>>>;

/// How many finished writers stay cached in memory. Not a bound on how many
/// survive: the log is the record, and this is only the fast path.
const MAX_RETAINED_PATCHES: usize = 16;

/// How much finished-child history the panel keeps. Enough to answer "what did
/// that agent do", short enough that the list stays readable.
pub const MAX_HISTORY: usize = 32;

/// Shared slot for the session that owns a tree of children.
pub type OwnerHandle = Arc<std::sync::Mutex<Option<Ulid>>>;

/// Told that a background child's report is durably recorded and collectable.
///
/// Fired *after* the outbox write, never off the roster: a child leaves the
/// roster inside its own execution, before its report is persisted, so anything
/// keyed on the roster emptying would race the record it is trying to read.
pub type Notifier = Arc<dyn Fn() + Send + Sync>;

/// Deferred slot for [`Notifier`] — the surface that wants the signal is built
/// after the control plane that emits it.
pub type NotifierHandle = Arc<std::sync::Mutex<Option<Notifier>>>;

/// Settled children, shared across a tree so the panel sees one record.
type History = Arc<std::sync::Mutex<Vec<Finished>>>;

#[derive(Clone)]
struct Delivered {
    agent: String,
    session: String,
    patch: Patch,
}

/// A child that has settled, for the `/agents` panel.
#[derive(Debug, Clone, Serialize)]
pub struct Finished {
    pub agent: String,
    pub session: String,
    pub objective: String,
    pub status: AgentStatus,
    pub duration_ms: u64,
    /// Whether it left a patch — the difference between "done" and "waiting on
    /// you", which the panel has to be able to show.
    pub patched: bool,
}

/// The live roster. Critical sections are a push, a retain and a clone, none of
/// them across an await, so a plain mutex is both correct and what lets the
/// registration guard clean up from `Drop`.
type Roster = Arc<std::sync::Mutex<Vec<Running>>>;

/// A child currently executing, with the handles to stop or steer it.
/// Cancelling one agent must not touch its siblings, so both are per-child
/// rather than the tree's.
struct Running {
    handle: AgentHandle,
    cancel: CancellationToken,
    /// Queues text for the child to receive at its next turn boundary. A child
    /// cannot ask a question, so without this a run that started on a wrong
    /// assumption can only be killed and paid for again.
    steer: kernel::InterruptHandle,
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
            workspaces: None,
            patches: Arc::new(std::sync::Mutex::new(Vec::new())),
            owner: Arc::new(std::sync::Mutex::new(None)),
            notifier: Arc::new(std::sync::Mutex::new(None)),
            history: Arc::new(std::sync::Mutex::new(Vec::new())),
            tasks: tokio_util::task::TaskTracker::new(),
        }
    }

    /// Share the slot naming the session these children belong to.
    ///
    /// The handle, not its value: the session id does not exist yet when the
    /// control plane is built, and a snapshot taken here would be `None`
    /// forever — patches would then be written to no chain at all.
    pub fn with_owner(mut self, owner: OwnerHandle) -> Self {
        self.owner = owner;
        self
    }

    /// The slot for the collectable-report signal. The surface installs its
    /// side once it exists; until then background reports simply wait, which is
    /// the pre-existing behaviour rather than a failure.
    pub fn notifier_handle(&self) -> NotifierHandle {
        Arc::clone(&self.notifier)
    }

    fn owner(&self) -> Option<Ulid> {
        self.owner.lock().ok().and_then(|slot| *slot)
    }

    /// What a run borrows from this control plane. `owner` overrides the shared
    /// handle for a background child, whose owner is fixed at dispatch.
    fn shared(&self, owner: Option<Ulid>) -> Shared {
        Shared {
            runner: Arc::clone(&self.runner),
            workspaces: self.workspaces.clone(),
            outbox: self.outbox.clone(),
            owner: owner.or_else(|| self.owner()),
            patches: Arc::clone(&self.patches),
            history: Arc::clone(&self.history),
        }
    }

    /// Children that have finished, newest last. The live roster answers "what
    /// is running"; this answers "what happened", which is a different question
    /// and the one a user asks after the fact.
    pub fn history(&self) -> Vec<Finished> {
        self.history
            .lock()
            .map(|history| history.clone())
            .unwrap_or_default()
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

    /// Enable writing children by supplying isolation. Without this every child
    /// stays read-only — a writer is refused, never degraded.
    pub fn with_workspaces(mut self, workspaces: Arc<dyn Workspaces>) -> Self {
        self.workspaces = Some(workspaces);
        self
    }

    /// Whether this session can run writing children at all, so a surface can
    /// say why rather than only that it failed.
    pub fn can_write(&self) -> bool {
        self.workspaces.is_some()
    }

    /// Test a child's patch against the current tree without touching it, for
    /// the merge preview.
    pub async fn check(&self, patch: &Patch) -> Option<MergeCheck> {
        let workspaces = self.workspaces.as_ref()?;
        Some(worktree::check(&workspaces.repo(), patch).await)
    }

    /// Apply a child's patch to the parent's tree.
    ///
    /// Two gates, in order. **Verification** (§6.4: a patch that does not build
    /// does not merge) — a patch whose verification *failed* is refused unless
    /// `force`. A patch with no verification at all is allowed: `None` means the
    /// project configured no verify command, and refusing on evidence the
    /// project cannot produce would make delegation unusable rather than safe.
    /// Then **conflict**, which `worktree::merge` refuses outright — never
    /// last-writer-wins.
    ///
    /// The *caller* is still responsible for the human gate: merging delegated
    /// work is a consequential action on files the user owns.
    pub async fn merge(&self, patch: &Patch, force: bool) -> Result<MergeCheck, Error> {
        let workspaces = self
            .workspaces
            .as_ref()
            .ok_or_else(|| Error::NoIsolation("no repository is configured".into()))?;
        if !force
            && let Some(evidence) = &patch.verification
            && !evidence.passed
        {
            return Err(Error::Unverified(evidence.command.clone()));
        }
        let outcome = worktree::merge(&workspaces.repo(), patch)
            .await
            .map_err(|error| Error::Failed(error.to_string()))?;
        Ok(outcome)
    }

    /// A finished writer's patch, by session id or display name.
    ///
    /// Memory first, then the durable record — so a patch outlives the process
    /// that produced it. Session id wins over name: it is the identity, and a
    /// name can be reused across sessions.
    pub async fn patch(&self, id: &str) -> Option<Patch> {
        if let Some(found) = self.cached_patch(id) {
            return Some(found);
        }
        let (outbox, owner) = (self.outbox.as_ref()?, self.owner()?);
        outbox
            .unapplied(owner)
            .await
            .into_iter()
            .rev()
            .find(|(agent, session, _)| session == id || agent == id)
            .map(|(_, _, patch)| patch)
    }

    /// How many patches this process is holding. The render-pass counterpart to
    /// [`Self::outstanding`]: a status bar redraws constantly and must not read
    /// the log to do it.
    pub fn cached_unmerged(&self) -> usize {
        self.patches
            .lock()
            .map(|patches| patches.iter().filter(|e| !e.patch.is_empty()).count())
            .unwrap_or(0)
    }

    /// Patches this process is holding, for a surface that must render before
    /// it can await. Correct for everything produced in this session — the
    /// durable [`Self::outstanding`] adds what survived a restart.
    pub fn cached_unmerged_patches(&self) -> Vec<(String, String, Patch)> {
        self.patches
            .lock()
            .map(|patches| {
                patches
                    .iter()
                    .filter(|entry| !entry.patch.is_empty())
                    .map(|entry| {
                        (
                            entry.agent.clone(),
                            entry.session.clone(),
                            entry.patch.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The in-memory fast path, for callers that cannot await — the TUI's
    /// render pass among them.
    pub fn cached_patch(&self, id: &str) -> Option<Patch> {
        let patches = self.patches.lock().ok()?;
        patches
            .iter()
            .rev()
            .find(|entry| entry.session == id)
            .or_else(|| patches.iter().rev().find(|entry| entry.agent == id))
            .map(|entry| entry.patch.clone())
    }

    /// Patches still waiting to be applied, oldest first, from the durable
    /// record so a restart does not hide finished work.
    pub async fn outstanding(&self) -> Vec<(String, String, Patch)> {
        let mut found: Vec<(String, String, Patch)> = match (&self.outbox, self.owner()) {
            (Some(outbox), Some(owner)) => outbox.unapplied(owner).await,
            _ => Vec::new(),
        };
        // Union with memory: a *foreground* writer's patch is recorded on the
        // owner's chain too, but a control with no owner yet — or no outbox —
        // still has to report what it holds.
        if let Ok(patches) = self.patches.lock() {
            for entry in patches.iter().filter(|entry| !entry.patch.is_empty()) {
                if !found
                    .iter()
                    .any(|(_, session, _)| *session == entry.session)
                {
                    found.push((
                        entry.agent.clone(),
                        entry.session.clone(),
                        entry.patch.clone(),
                    ));
                }
            }
        }
        found
    }

    /// Close a patch out once it has been applied: dropped from memory and
    /// marked in the log, so neither this process nor the next offers it again.
    pub async fn forget(&self, session: &str) {
        if let Ok(mut patches) = self.patches.lock() {
            patches.retain(|entry| entry.session != session);
        }
        if let (Some(outbox), Some(owner), Ok(child)) =
            (&self.outbox, self.owner(), session.parse())
        {
            outbox.applied(owner, child).await;
        }
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

    /// Close out children abandoned by a process that died mid-run, so their
    /// dispatches resolve instead of hanging. Call once when a session opens;
    /// the results then arrive through [`Self::collect`] like any other.
    pub async fn reap_abandoned(&self, parent: Ulid) -> usize {
        match &self.outbox {
            Some(outbox) => outbox.reap_abandoned(parent).await,
            None => 0,
        }
    }

    /// How long each running child has been silent, in milliseconds, keyed by
    /// session id. Absent means it has recorded nothing at all yet.
    ///
    /// Answers "is this moving?", which elapsed-since-start cannot: a child
    /// ninety seconds in may have worked for all ninety or stalled after one.
    pub async fn idle_times(&self) -> std::collections::HashMap<String, Option<u64>> {
        let Some(outbox) = &self.outbox else {
            return std::collections::HashMap::new();
        };
        let now = epoch_ms();
        let mut idle = std::collections::HashMap::new();
        for run in self.active() {
            let Ok(id) = run.session.parse() else {
                continue;
            };
            let since = outbox.last_activity(id).await.map(|ts| {
                // Saturating: clock adjustment must not read as a child that
                // has been idle since before the epoch.
                now.saturating_sub((ts * 1000.0) as u64)
            });
            idle.insert(run.session, since);
        }
        idle
    }

    /// Send further instruction to a running child, by name or session id.
    ///
    /// The text lands as a user message at the child's next turn boundary — it
    /// never interrupts a tool call mid-flight, and it does not restart the
    /// child or discard what it has already found. Returns the agents actually
    /// reached, empty if none matched.
    ///
    /// This is the alternative to the only other correction available: killing
    /// the child and paying for the whole run again. A child cannot ask a
    /// question, so a run that began on a wrong assumption is otherwise
    /// unrecoverable.
    pub fn steer(&self, id: &str, text: &str) -> Vec<String> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        let Ok(active) = self.active.lock() else {
            return Vec::new();
        };
        active
            .iter()
            .filter(|run| run.handle.agent == id || run.handle.session == id)
            .map(|run| {
                run.steer.steer(text);
                run.handle.agent.clone()
            })
            .collect()
    }

    /// Results finished but not yet handed to `parent`, marked delivered as they
    /// are taken. A delivered result is never returned twice, so a replayed log
    /// cannot duplicate a report.
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

    /// Read back what a child did. Its transcript lives under its own session id
    /// and outlives the run, so this answers for a finished agent as well as a
    /// running one.
    pub async fn transcript(&self, child: &str) -> Result<Vec<String>, Error> {
        let outbox = self.outbox.as_ref().ok_or(Error::Unavailable)?;
        // By name as well as id, like `cancel` and `steer`. The name is what is
        // on screen and in the model's own previous message; requiring the id
        // only here made the one tool you reach for when something looks wrong
        // the one that rejects the identifier you have.
        let id = match child.parse() {
            Ok(id) => id,
            Err(_) => self
                .resolve(child)
                .ok_or_else(|| Error::UnknownAgent(child.to_string()))?,
        };
        Ok(outbox.transcript(id).await)
    }

    /// The session id of a live or recently finished agent, by display name.
    /// History as well as the roster: a name is most often typed *after* the
    /// agent finished, which is exactly when it has left the roster.
    fn resolve(&self, name: &str) -> Option<Ulid> {
        let live = self.active.lock().ok().and_then(|active| {
            active
                .iter()
                .find(|run| run.handle.agent == name)
                .and_then(|run| run.handle.session.parse().ok())
        });
        live.or_else(|| {
            self.history.lock().ok().and_then(|history| {
                history
                    .iter()
                    .rev()
                    .find(|done| done.agent == name)
                    .and_then(|done| done.session.parse().ok())
            })
        })
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
    async fn admit(
        &self,
        spec: &mut AgentSpec,
        parent_executor: Arc<dyn Executor>,
        remaining_turns: u32,
        background: bool,
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
        let session = Ulid::new();
        // The two paths differ only in *where* the child works and whether it
        // keeps its mutating tools. A writer without a worktree is refused
        // (§6.4) rather than degraded: silently running it read-only would fail
        // its objective, and silently running it un-isolated would corrupt the
        // parent's tree.
        let (executor, workspace) = if spec.write {
            let workspaces = self
                .workspaces
                .as_ref()
                .ok_or_else(|| Error::NoIsolation("this session has no git repository".into()))?;
            let Workspace { worktree, executor } = workspaces
                .checkout(session)
                .await
                .map_err(Error::NoIsolation)?;
            // Narrowed against the *parent's* names as well as the rebased
            // executor's. The rebase is a re-registration of the same tools, so
            // the sets already match — but "already match" is an assumption
            // about another crate, and "cannot widen" is not something to hold
            // by assumption.
            let parent_tools: Vec<String> = parent_executor
                .specs()
                .into_iter()
                .map(|spec| spec.name)
                .collect();
            let requested: Vec<String> = match spec.tools.as_deref() {
                Some(asked) => asked
                    .iter()
                    .filter(|name| parent_tools.contains(name))
                    .cloned()
                    .collect(),
                None => parent_tools,
            };
            // No delegation: a writer keeps its mutating tools, so unlike a
            // read-only child it would otherwise retain `agent.spawn` and be
            // able to nest indefinitely.
            let narrowed: Arc<dyn Executor> = Arc::new(
                NarrowedExecutor::new(executor, Some(&requested))
                    .no_delegation()
                    .no_clarifying_questions(),
            );
            (narrowed, Some(worktree))
        } else {
            // Read-only children may share the parent's tree safely; that is
            // precisely what makes them safe to run in parallel.
            // `read_only` already removes `agent.spawn`; saying it outright
            // means the invariant does not depend on a blast-radius accident.
            let narrowed: Arc<dyn Executor> = Arc::new(
                NarrowedExecutor::new(parent_executor, spec.tools.as_deref())
                    .read_only()
                    .no_delegation()
                    .no_clarifying_questions(),
            );
            (narrowed, None)
        };
        // A child's budget is a slice of what the parent has left, never more.
        let max_turns = spec
            .max_turns
            .unwrap_or(remaining_turns)
            .min(remaining_turns)
            .max(1);

        // Per-child token, so cancelling one agent leaves its siblings alone;
        // it is a child of the tree's, so cancelling the tree still settles all.
        let cancel = self.cancel.child_token();
        // Per-child steer queue. The handle stays on the roster so the child
        // stays addressable while it runs; the queue moves into its session.
        let (steer, interrupts) = kernel::InterruptQueue::pair();
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
                    background,
                },
                cancel: cancel.clone(),
                steer: steer.clone(),
            });
        }
        Ok(Admitted {
            session,
            cancel,
            interrupts,
            executor,
            workspace,
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
        let admitted = self
            .admit(&mut spec, parent_executor, remaining_turns, false)
            .await?;
        Ok(execute(self.shared(None), spec, admitted).await)
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
        let admitted = self
            .admit(&mut spec, parent_executor, remaining_turns, true)
            .await?;
        let handle = AgentHandle {
            agent: spec.name.clone(),
            session: admitted.session.to_string(),
            objective: spec.objective.clone(),
            started_ms: epoch_ms(),
            background: true,
        };
        let dispatch = Dispatch {
            agent: spec.name.clone(),
            child: admitted.session,
            parent,
            objective: spec.objective.clone(),
        };
        outbox.dispatched(&dispatch).await;

        // The owner is the dispatching session as given, not the handle's
        // current value: a background child can finish after the surface has
        // moved on, and its patch belongs to whoever asked for it.
        let shared = self.shared(Some(parent));
        let notifier = Arc::clone(&self.notifier);
        self.tasks.spawn(async move {
            let result = execute(shared, spec, admitted).await;
            outbox.finished(&dispatch, &result).await;
            // Only now is the report collectable. Signalling any earlier — off
            // the roster, or before this write — would hand the surface a
            // completion it cannot yet read.
            //
            // Cloned out with the guard dropped before the call: a notifier
            // that re-entered this control plane while the lock was held would
            // deadlock on a non-reentrant mutex, and that is the class of bug
            // that only shows up under timing nobody can reproduce.
            let ready = notifier
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().map(Arc::clone));
            if let Some(ready) = ready {
                ready();
            }
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
            workspaces: self.workspaces.clone(),
            patches: Arc::clone(&self.patches),
            owner: Arc::clone(&self.owner),
            notifier: Arc::clone(&self.notifier),
            history: Arc::clone(&self.history),
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

/// A child cleared to run: capacity held, executor narrowed, roster entry made,
/// and — for a writer — an isolated checkout leased.
struct Admitted {
    session: Ulid,
    cancel: CancellationToken,
    /// The child's steer queue, moved into its run. Not cloneable: exactly one
    /// session loop may own it, which is what makes "who receives this text"
    /// unambiguous.
    interrupts: kernel::InterruptQueue,
    executor: Arc<dyn Executor>,
    /// A writer's checkout, held here so it is reaped on exactly one path
    /// regardless of how the child ends.
    workspace: Option<Worktree>,
    max_turns: u32,
    /// Held for the child's lifetime; dropping it frees a slot in the tree.
    permit: OwnedSemaphorePermit,
    registration: Registration,
}

/// The collaborators a run needs from its control plane. Bundled so foreground
/// and background spawn hand over the same set — a signature they both spell out
/// is one they can drift apart on.
struct Shared {
    runner: Arc<dyn ChildRunner>,
    workspaces: Option<Arc<dyn Workspaces>>,
    outbox: Option<Arc<dyn Outbox>>,
    owner: Option<Ulid>,
    patches: Patches,
    history: History,
}

/// The one execution path, shared by foreground and background spawn so they
/// cannot drift in how they cancel, time or bound a child.
async fn execute(shared: Shared, spec: AgentSpec, admitted: Admitted) -> AgentResult {
    let Shared {
        runner,
        workspaces,
        outbox,
        owner,
        patches,
        history,
    } = shared;
    let objective = spec.objective.clone();
    let Admitted {
        session,
        cancel,
        executor,
        workspace,
        max_turns,
        permit,
        registration,
        interrupts,
    } = admitted;
    let started = Instant::now();
    let run = runner.run(ChildRun {
        session,
        spec: spec.clone(),
        executor,
        max_turns,
        cancel: cancel.clone(),
        interrupts,
        workspace: workspace.as_ref().map(|tree| tree.path().to_path_buf()),
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

    // The patch is taken on *every* exit path, cancellation and failure
    // included. A writer killed halfway has usually still changed something,
    // and throwing that away is the same partial-output loss §6.5 forbids for
    // summaries — only more expensive, because it was real work on real files.
    let patch = match (&workspace, &workspaces) {
        (Some(tree), Some(workspaces)) => {
            let mut patch = tree.patch().await.unwrap_or_default();
            // Verification runs here rather than being reported by the child:
            // "I ran the tests and they passed" is exactly the claim a merge
            // gate cannot take on faith. Skipped for an empty patch — there is
            // nothing to verify, and a build is not free.
            if !patch.is_empty() {
                patch.verification = workspaces.verify(tree.path()).await;
            }
            Some(patch)
        }
        _ => None,
    };
    // Only now: the diff is out, so the checkout has nothing left to protect.
    // Reaping before this would discard the child's work; not reaping at all
    // wedges the next `git worktree add` on that path.
    if let Some(tree) = &workspace {
        tree.reap().await;
    }

    let elapsed = started.elapsed();
    let mut result = match outcome {
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
    };
    // Retained so the merge can be asked for by agent id. Round-tripping a diff
    // back through the model to apply it would put a whitespace-exact artifact
    // through a channel that does not preserve whitespace exactly.
    if let Some(patch) = &patch
        && !patch.is_empty()
    {
        if let Ok(mut patches) = patches.lock() {
            patches.push(Delivered {
                agent: result.agent.clone(),
                session: result.session.clone(),
                patch: patch.clone(),
            });
            let excess = patches.len().saturating_sub(MAX_RETAINED_PATCHES);
            patches.drain(..excess);
        }
        // And durably. The worktree is already gone, so if this is the process
        // that dies next, the log is the only place the work still exists.
        if let (Some(outbox), Some(owner)) = (&outbox, owner) {
            outbox.recorded(owner, &result.agent, session, patch).await;
        }
    }
    if let Ok(mut history) = history.lock() {
        history.push(Finished {
            agent: result.agent.clone(),
            session: result.session.clone(),
            objective,
            status: result.status,
            duration_ms: result.duration_ms,
            patched: patch.as_ref().is_some_and(|patch| !patch.is_empty()),
        });
        let excess = history.len().saturating_sub(MAX_HISTORY);
        history.drain(..excess);
    }
    result.patch = patch;
    result
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
        patch: None,
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
                background: false,
            },
            cancel: CancellationToken::new(),
            steer: kernel::InterruptQueue::pair().0,
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
        patch: Patch,
        applied: bool,
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
        async fn transcript(&self, _child: Ulid) -> Vec<String> {
            Vec::new()
        }
        async fn recorded(&self, parent: Ulid, agent: &str, child: Ulid, patch: &Patch) {
            self.patches.lock().unwrap().push(Recorded {
                parent,
                agent: agent.to_string(),
                child: child.to_string(),
                patch: patch.clone(),
                applied: false,
            });
        }
        async fn applied(&self, _parent: Ulid, child: Ulid) {
            if let Some(row) = self
                .patches
                .lock()
                .unwrap()
                .iter_mut()
                .find(|row| row.child == child.to_string())
            {
                row.applied = true;
            }
        }
        async fn unapplied(&self, parent: Ulid) -> Vec<(String, String, Patch)> {
            self.patches
                .lock()
                .unwrap()
                .iter()
                .filter(|row| row.parent == parent && !row.applied)
                .map(|row| (row.agent.clone(), row.child.clone(), row.patch.clone()))
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
            .spawn_background(spec("survey"), Arc::new(Tools), 5, parent)
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
                heard.extend(run.interrupts.drain_steers());
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
        let control = Arc::new(AgentControl::new(
            Arc::new(Listens),
            CancellationToken::new(),
        ));
        let steering = Arc::clone(&control);
        // Steer from outside while the child runs, which is the only way this
        // is ever used.
        tokio::spawn(async move {
            loop {
                // Address it the way a caller would: by the id the roster
                // reports, once the child is actually up.
                if let Some(run) = steering.active().first()
                    && !steering
                        .steer(&run.session, "only the parser, skip the lexer")
                        .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        });

        let result = control
            .spawn(spec("survey the compiler"), Arc::new(Tools), 5)
            .await
            .unwrap();
        assert_eq!(result.summary, "only the parser, skip the lexer");
    }

    #[tokio::test]
    async fn a_stalled_child_is_distinguishable_from_a_working_one() {
        let outbox = Arc::new(MemoryOutbox::default());
        let control = AgentControl::new(Arc::new(Hangs), CancellationToken::new())
            .with_outbox(outbox.clone())
            .with_limits(4, 1);
        let parent = Ulid::new();
        let busy = control
            .spawn_background(spec("busy worker"), Arc::new(Tools), 5, parent)
            .await
            .unwrap();
        let stalled = control
            .spawn_background(spec("stalled worker"), Arc::new(Tools), 5, parent)
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
        let control = AgentControl::new(Arc::new(Hangs), CancellationToken::new())
            .with_outbox(Arc::new(MemoryOutbox::default()));
        let parent = Ulid::new();
        let starting = control
            .spawn_background(spec("just started"), Arc::new(Tools), 5, parent)
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
        assert!(control.steer("no-such-agent", "hello").is_empty());
    }

    #[tokio::test]
    async fn empty_steer_text_is_refused_before_it_reaches_anyone() {
        let control = AgentControl::new(Arc::new(Hangs), CancellationToken::new());
        control.active.lock().unwrap().push(Running {
            handle: AgentHandle {
                agent: "worker".into(),
                session: Ulid::new().to_string(),
                objective: "work".into(),
                started_ms: epoch_ms(),
                background: false,
            },
            cancel: CancellationToken::new(),
            steer: kernel::InterruptQueue::pair().0,
        });
        // Whitespace would arrive as an empty user message and cost the child a
        // turn to read nothing.
        assert!(control.steer("worker", "   ").is_empty());
        assert_eq!(control.steer("worker", "narrow it").len(), 1);
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
        std::fs::write(root.join("a.txt"), "original\n").unwrap();
        run(vec!["add", "."]);
        run(vec!["commit", "-m", "base"]);
        let state = tempfile::tempdir().unwrap();
        (repo, state, root)
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
        async fn verify(&self, _root: &Path) -> Option<Verification> {
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
            control
                .spawn(writer("fix the bug"), Arc::new(Tools), 5)
                .await,
            Err(Error::NoIsolation(_))
        ));
        assert!(!control.can_write());
    }

    #[tokio::test]
    async fn a_writer_edits_its_own_checkout_and_never_the_parents() {
        let (_repo, state, root) = writable().await;
        let control = AgentControl::new(Arc::new(Edits), CancellationToken::new())
            .with_workspaces(isolation(&root, &state));

        let result = control
            .spawn(writer("rewrite a.txt"), Arc::new(Tools), 5)
            .await
            .unwrap();
        assert_eq!(result.status, AgentStatus::Completed);
        // The parent's working tree is exactly as it was.
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "original\n"
        );
        assert!(!root.join("added.txt").exists());
    }

    #[tokio::test]
    async fn a_writer_returns_a_patch_rather_than_an_account_of_its_edits() {
        let (_repo, state, root) = writable().await;
        let control = AgentControl::new(Arc::new(Edits), CancellationToken::new())
            .with_workspaces(isolation(&root, &state));

        let result = control
            .spawn(writer("rewrite a.txt"), Arc::new(Tools), 5)
            .await
            .unwrap();
        let patch = result.patch.expect("a writer reports a patch");
        assert!(patch.diff.contains("rewritten by the child"));
        assert!(patch.files.contains(&"a.txt".to_string()));
        assert!(patch.files.contains(&"added.txt".to_string()));
        // Which is reviewable *and* applicable — the point of a diff over prose.
        assert_eq!(control.check(&patch).await, Some(MergeCheck::Clean));
        control.merge(&patch, false).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "rewritten by the child\n"
        );
    }

    #[tokio::test]
    async fn nothing_is_merged_until_someone_asks_for_it() {
        let (_repo, state, root) = writable().await;
        let control = AgentControl::new(Arc::new(Edits), CancellationToken::new())
            .with_workspaces(isolation(&root, &state));

        let result = control
            .spawn(writer("rewrite a.txt"), Arc::new(Tools), 5)
            .await
            .unwrap();
        // Merging is a consequential action on the user's files. Finishing a
        // child must never be what triggers it.
        assert!(result.patch.is_some());
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "original\n"
        );
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
        let control = AgentControl::new(Arc::new(Edits), CancellationToken::new())
            .with_workspaces(workspaces);

        let result = control
            .spawn(writer("rewrite a.txt"), Arc::new(Tools), 5)
            .await
            .unwrap();
        // The child said it succeeded; the build says otherwise, and the build
        // is what the merge gate reads.
        assert_eq!(result.status, AgentStatus::Completed);
        let patch = result.patch.unwrap();
        assert!(!patch.verified());
        assert_eq!(patch.verification.unwrap().output, "2 failed");
    }

    #[tokio::test]
    async fn a_cancelled_writer_still_hands_back_what_it_changed() {
        let (_repo, state, root) = writable().await;
        let control = AgentControl::new(Arc::new(Hangs), CancellationToken::new())
            .with_outbox(Arc::new(MemoryOutbox::default()))
            .with_workspaces(isolation(&root, &state));
        let parent = Ulid::new();
        control
            .spawn_background(writer("never finishes"), Arc::new(Tools), 5, parent)
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
        let control = AgentControl::new(Arc::new(Edits), CancellationToken::new()).with_workspaces(
            Arc::new(Isolation {
                pool: WorktreePool::new(&root, worktrees.clone()),
                repo: root.clone(),
                verification: None,
            }),
        );

        control
            .spawn(writer("rewrite a.txt"), Arc::new(Tools), 5)
            .await
            .unwrap();
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
        let control = AgentControl::new(Arc::new(Edits), CancellationToken::new())
            .with_workspaces(judged(&root, &state, failed_build()));
        let result = control
            .spawn(writer("rewrite a.txt"), Arc::new(Tools), 5)
            .await
            .unwrap();
        let patch = result.patch.unwrap();

        // §6.4. The child said it succeeded and the diff applies cleanly — the
        // only thing standing between a broken change and the user's tree is
        // this refusal.
        assert_eq!(control.check(&patch).await, Some(MergeCheck::Clean));
        assert!(matches!(
            control.merge(&patch, false).await,
            Err(Error::Unverified(_))
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "original\n"
        );
    }

    #[tokio::test]
    async fn a_failing_patch_can_still_be_forced_deliberately() {
        let (_repo, state, root) = writable().await;
        let control = AgentControl::new(Arc::new(Edits), CancellationToken::new())
            .with_workspaces(judged(&root, &state, failed_build()));
        let patch = control
            .spawn(writer("rewrite a.txt"), Arc::new(Tools), 5)
            .await
            .unwrap()
            .patch
            .unwrap();

        // A pre-existing unrelated build failure must not make delegation
        // permanently unusable — but getting past it has to be an explicit act.
        control.merge(&patch, true).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "rewritten by the child\n"
        );
    }

    #[tokio::test]
    async fn a_project_with_no_verifier_is_not_blocked_by_the_gate() {
        let (_repo, state, root) = writable().await;
        let control = AgentControl::new(Arc::new(Edits), CancellationToken::new())
            .with_workspaces(judged(&root, &state, None));
        let patch = control
            .spawn(writer("rewrite a.txt"), Arc::new(Tools), 5)
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
        let session = control
            .spawn(writer("rewrite a.txt"), Arc::new(Tools), 5)
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
        let recovered = after.patch(&session).await.expect("patch survived");
        assert!(recovered.diff.contains("rewritten by the child"));
        assert_eq!(after.outstanding().await.len(), 1);

        after.merge(&recovered, false).await.unwrap();
        after.forget(&session).await;
        // And once applied it stays applied — a later run must not offer it
        // again, or the same patch lands twice.
        let later = restarted(outbox, owner, judged(&root, &state, None));
        assert!(later.outstanding().await.is_empty());
    }

    #[tokio::test]
    async fn finished_children_stay_visible_after_they_leave_the_roster() {
        let (_repo, state, root) = writable().await;
        let control = AgentControl::new(Arc::new(Edits), CancellationToken::new())
            .with_workspaces(judged(&root, &state, None));
        control
            .spawn(writer("rewrite a.txt"), Arc::new(Tools), 5)
            .await
            .unwrap();

        // The roster is empty the instant a child settles — correct for "what
        // is running", useless for "what happened" (§6.6 asks for both).
        assert!(control.active().is_empty());
        let history = control.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, AgentStatus::Completed);
        assert!(history[0].patched);
        assert_eq!(history[0].objective, "rewrite a.txt");
    }

    #[tokio::test]
    async fn a_failed_child_is_recorded_not_forgotten() {
        let (_, control) = control();
        let control = AgentControl::new(Arc::new(Hangs), control.cancel.clone());
        control.cancel.cancel();
        control
            .spawn(spec("will be cancelled"), Arc::new(Tools), 5)
            .await
            .unwrap();
        let history = control.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, AgentStatus::Cancelled);
        assert!(!history[0].patched);
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
