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

mod fork;
mod narrow;
mod path;
mod registry;
pub mod worktree;
pub use fork::Fork;
pub use narrow::NarrowedExecutor;
pub use path::{AgentPath, PathError};
pub use registry::{Agent, AgentRegistry, State};
pub use worktree::{MergeCheck, Patch, Verification, Worktree, WorktreePool};

/// Ceiling on children alive at once. Each child burns tokens independently, so
/// this is a spend bound as much as a concurrency one.
pub const DEFAULT_MAX_ACTIVE: usize = 3;
/// How deep delegation may nest, counted in [`AgentPath`] segments.
pub const DEFAULT_MAX_DEPTH: u32 = 1;
/// Summary text handed back to the parent before the rest spills to an artifact.
pub const MAX_SUMMARY_CHARS: usize = 16_000;

/// Bounds on one [`AgentControl::wait`]. The floor is what stops a wait being
/// turned back into a poll; the ceiling is what stops it being turned back into
/// the unbounded block it replaced.
#[derive(Debug, Clone, Copy)]
pub struct WaitBounds {
    pub min: Duration,
    pub default: Duration,
    pub max: Duration,
}

impl Default for WaitBounds {
    fn default() -> Self {
        Self {
            min: Duration::from_secs(1),
            default: Duration::from_secs(120),
            max: Duration::from_secs(600),
        }
    }
}

impl WaitBounds {
    /// Resolve a requested wait. Out of range is an error, never a clamp: a
    /// silently shortened wait returns "nothing finished" and reads as an
    /// answer.
    pub fn resolve(&self, requested: Option<Duration>) -> Result<Duration, Error> {
        let Some(requested) = requested else {
            return Ok(self.default);
        };
        if requested < self.min || requested > self.max {
            return Err(Error::WaitOutOfRange {
                min: self.min.as_secs(),
                max: self.max.as_secs(),
            });
        }
        Ok(requested)
    }
}

/// Why a [`AgentControl::wait`] returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waited {
    /// These descendants of the caller settled.
    Settled(Vec<AgentPath>),
    /// Nothing settled inside the bound. The children are still running.
    TimedOut,
    /// Someone spoke to the caller while it waited. The children are untouched;
    /// what changed is that there is now something better to do than wait.
    Interrupted,
    /// The caller's own run is being torn down; there is nothing to wait for.
    Cancelled,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("sub-agents are not available in this session")]
    Unavailable,
    #[error("agent objective is empty")]
    NoObjective,
    #[error("delegation depth {depth} exceeds the limit of {max}")]
    TooDeep { depth: u32, max: u32 },
    #[error("too many agents already running (limit {0}); wait for one to finish")]
    AtCapacity(usize),
    #[error("no agent '{0}'")]
    UnknownAgent(String),
    #[error("'{0}' is not one of this agent's own children")]
    OutOfReach(String),
    #[error("'{0}' has already finished — read its report, or send it a follow-up task")]
    Settled(String),
    #[error("timeout must be between {min}s and {max}s")]
    WaitOutOfRange { min: u64, max: u64 },
    #[error("agent failed: {0}")]
    Failed(String),
    #[error("an agent is already called '{0}'")]
    NameTaken(String),
    #[error("{0}")]
    BadName(String),
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
    /// How much of the caller's conversation the child starts with.
    pub fork: Fork,
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

/// How the runtime actually runs a child. Implemented outside this crate by
/// whoever owns a `Kernel`, which keeps the orchestrator free of the kernel's
/// provider/log type parameters — and free of a dependency cycle with the tool
/// registry that hosts `agent.spawn`.
#[async_trait::async_trait]
pub trait ChildRunner: Send + Sync {
    async fn run(&self, run: ChildRun) -> Result<ChildOutcome, String>;
}

/// Who is asking. Depth, name resolution and reach all follow from this, so an
/// agent's authority is a property of its own address rather than of the
/// control plane it happens to hold — which reads the same for every descendant
/// and so bounds nothing.
#[derive(Debug, Clone)]
pub struct Caller {
    pub path: AgentPath,
    pub session: Ulid,
}

impl Caller {
    pub fn root(session: Ulid) -> Self {
        Self {
            path: AgentPath::root(),
            session,
        }
    }
}

/// Rebinds a child's delegation tools to the child's own address.
///
/// The tools live in the registry crate, which depends on this one, so the
/// orchestrator cannot build them; it can only ask. Without this a child holds
/// the *parent's* `agent.spawn`, and every path it derives is the parent's —
/// which is how a depth limit comes to read the root's depth forever.
pub trait Delegation: Send + Sync {
    /// Re-root the `agent.*` tools in `executor` at `caller`. Only names
    /// `executor` already exposes may be replaced, so rebinding can narrow but
    /// never widen.
    fn rebind(&self, executor: Arc<dyn Executor>, caller: Caller) -> Arc<dyn Executor>;
}

/// Reads a session's conversation back, so a child can be forked from it.
///
/// Separate from [`Outbox`]: that answers "what has been delivered", this
/// answers "what was said". Both happen to be folds over the same event log,
/// but a surface can supply one without the other.
#[async_trait::async_trait]
pub trait Transcripts: Send + Sync {
    async fn history(&self, session: Ulid) -> Vec<kernel::Message>;
}

/// Everything a runner needs to execute one child.
pub struct ChildRun {
    pub session: Ulid,
    pub spec: AgentSpec,
    /// The caller's conversation, already filtered to what a child may inherit.
    /// The runner puts this *before* the objective, so the child reads the
    /// history and then what it is being asked to do.
    pub history: Vec<kernel::Message>,
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
    max_depth: u32,
    wait_bounds: WaitBounds,
    /// Builds a child's own delegation tools. Installed after construction: the
    /// tools it builds are hosted by the registry that owns this control plane,
    /// so it cannot exist first — the same cycle [`DeferredRunner`] breaks,
    /// broken the same way.
    delegation: std::sync::OnceLock<Arc<dyn Delegation>>,
    cancel: CancellationToken,
    registry: Arc<AgentRegistry>,
    outbox: Option<Arc<dyn Outbox>>,
    /// Writer isolation. `None` means writers are refused: without it a writing
    /// child would edit the parent's tree, which is the one thing §6.4 exists
    /// to prevent.
    workspaces: Option<Arc<dyn Workspaces>>,
    /// Where a fork reads the caller's conversation from. `None` means every
    /// child starts cold, which is the pre-existing behaviour rather than a
    /// failure.
    transcripts: Option<Arc<dyn Transcripts>>,
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
    /// The root operator's interrupt handle, so a wait at the top of the tree
    /// ends when the *user* says something. Children are found in the registry;
    /// the root has no entry there, because nothing spawned it.
    root: std::sync::Mutex<Option<kernel::InterruptHandle>>,
    /// Woken whenever a child settles, so [`Self::wait`] blocks on the event
    /// rather than a poll interval.
    settled: Arc<tokio::sync::Notify>,
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

#[derive(Clone)]
struct Delivered {
    agent: String,
    session: String,
    patch: Patch,
}

impl AgentControl {
    pub fn new(runner: Arc<dyn ChildRunner>, cancel: CancellationToken) -> Self {
        Self {
            runner,
            capacity: Arc::new(Semaphore::new(DEFAULT_MAX_ACTIVE)),
            max_active: DEFAULT_MAX_ACTIVE,
            max_depth: DEFAULT_MAX_DEPTH,
            wait_bounds: WaitBounds::default(),
            delegation: std::sync::OnceLock::new(),
            registry: Arc::new(AgentRegistry::new()),
            cancel,
            outbox: None,
            workspaces: None,
            transcripts: None,
            patches: Arc::new(std::sync::Mutex::new(Vec::new())),
            owner: Arc::new(std::sync::Mutex::new(None)),
            notifier: Arc::new(std::sync::Mutex::new(None)),
            root: std::sync::Mutex::new(None),
            settled: Arc::new(tokio::sync::Notify::new()),
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
            registry: Arc::clone(&self.registry),
            settled: Arc::clone(&self.settled),
        }
    }

    /// Every child this session knows about, running or settled.
    pub fn agents(&self) -> Vec<Agent> {
        self.registry.all()
    }

    pub fn with_limits(mut self, max_active: usize, max_depth: u32) -> Self {
        self.max_active = max_active.max(1);
        self.max_depth = max_depth;
        self.capacity = Arc::new(Semaphore::new(self.max_active));
        self
    }

    pub fn with_wait_bounds(mut self, bounds: WaitBounds) -> Self {
        self.wait_bounds = bounds;
        self
    }

    /// Let children hold delegation tools addressed at themselves. Later calls
    /// are ignored, so nothing can swap the tree's addressing out from under a
    /// running child.
    pub fn install_delegation(&self, delegation: Arc<dyn Delegation>) {
        let _ = self.delegation.set(delegation);
    }

    pub fn wait_bounds(&self) -> WaitBounds {
        self.wait_bounds
    }

    /// Register the operator's interrupt handle for this turn, so a wait at the
    /// root ends when the user speaks. A surface makes a fresh handle per turn,
    /// so this replaces rather than accumulates.
    pub fn attend(&self, interrupt: kernel::InterruptHandle) {
        if let Ok(mut slot) = self.root.lock() {
            *slot = Some(interrupt);
        }
    }

    /// What is being queued against `from`'s own session, if anything can be.
    fn activity(&self, from: &AgentPath) -> Option<kernel::Activity> {
        match from.is_root() {
            true => self
                .root
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().map(kernel::InterruptHandle::activity)),
            false => self.registry.activity(from),
        }
    }

    pub fn max_depth(&self) -> u32 {
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

    /// Let children inherit the caller's conversation. Without it every child
    /// starts cold and pays to rediscover what the caller already knows.
    pub fn with_transcripts(mut self, transcripts: Arc<dyn Transcripts>) -> Self {
        self.transcripts = Some(transcripts);
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
    pub fn active(&self) -> Vec<Agent> {
        self.registry.running()
    }

    /// Resolve `reference` from `from`'s address, refusing anything outside
    /// `from`'s own subtree.
    ///
    /// Reach is the security boundary here: the delegation tools are shared
    /// with every descendant, so without this a read-only child could cancel or
    /// steer its siblings — agents it did not create and cannot see.
    pub fn reach(&self, from: &AgentPath, reference: &str) -> Result<Agent, Error> {
        let agent = self.address(from, reference)?;
        if !agent.path.under(from) || &agent.path == from {
            return Err(Error::OutOfReach(reference.to_string()));
        }
        Ok(agent)
    }

    /// Resolve `reference` anywhere in the tree.
    ///
    /// Wider than [`Self::reach`] on purpose, and only for *talking*: a message
    /// costs its recipient a turn to read, where a cancel destroys work and a
    /// transcript exposes it. Sideways collaboration is the point of a tree, so
    /// containment here would be a limit with nothing behind it.
    pub fn address(&self, from: &AgentPath, reference: &str) -> Result<Agent, Error> {
        self.registry
            .find(from, reference)
            .ok_or_else(|| Error::UnknownAgent(reference.to_string()))
    }

    /// Put text in a live agent's queue, from anywhere in the tree. It is read
    /// at that agent's next turn boundary.
    pub fn message(
        &self,
        from: &AgentPath,
        reference: &str,
        text: &str,
    ) -> Result<AgentPath, Error> {
        if text.trim().is_empty() {
            return Err(Error::NoObjective);
        }
        let agent = self.address(from, reference)?;
        if &agent.path == from {
            return Err(Error::OutOfReach(reference.to_string()));
        }
        match self.registry.steer(&agent.path, text) {
            true => Ok(agent.path),
            false => Err(Error::Settled(reference.to_string())),
        }
    }

    /// Block until one of `from`'s own children settles, `from` is spoken to, or
    /// `timeout` elapses.
    ///
    /// Bounded on purpose: an unbounded wait is the foreground spawn this
    /// replaces. Reports the paths that settled — the results themselves come
    /// through the outbox like any other, so waiting never becomes the only way
    /// to collect one.
    pub async fn wait(&self, from: &AgentPath, timeout: Duration) -> Waited {
        let mut spoken_to = self.activity(from);
        if let Some(activity) = spoken_to.as_mut() {
            activity.borrow_and_update();
        }
        let mine: Vec<String> = self
            .registry
            .running()
            .into_iter()
            .filter(|agent| agent.path.under(from) && &agent.path != from)
            .map(|agent| agent.session)
            .collect();
        if mine.is_empty() {
            return Waited::Settled(Vec::new());
        }
        let deadline = Instant::now() + timeout;
        loop {
            // `enable`, not merely constructing the future: a `Notified` does
            // not register until it is first polled, so building it before the
            // check would still lose a child that settles between the two and
            // leave this asleep until the deadline.
            let woken = self.settled.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            let done: Vec<AgentPath> = self
                .registry
                .all()
                .into_iter()
                .filter(|agent| !agent.is_running() && mine.contains(&agent.session))
                .map(|agent| agent.path)
                .collect();
            if !done.is_empty() {
                return Waited::Settled(done);
            }
            if self.cancel.is_cancelled() {
                return Waited::Cancelled;
            }
            if spoken_to
                .as_ref()
                .is_some_and(|activity| activity.has_changed().unwrap_or(false))
            {
                return Waited::Interrupted;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Waited::TimedOut;
            }
            // A wait exists so the caller can pause on work it needs. The moment
            // its own operator says something, that premise is gone — holding
            // the turn to its deadline would be obeying instructions already
            // known to be superseded.
            let interrupted = async {
                match spoken_to.as_mut() {
                    Some(activity) => activity.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                outcome = tokio::time::timeout(left, woken) => {
                    if outcome.is_err() {
                        return Waited::TimedOut;
                    }
                }
                spoke = interrupted => if spoke { return Waited::Interrupted },
                _ = self.cancel.cancelled() => return Waited::Cancelled,
            }
        }
    }

    /// Stop one of `from`'s children, leaving its siblings running. `false` when
    /// it had already finished — reporting success there would read as work
    /// abandoned that in fact completed.
    pub fn cancel(&self, from: &AgentPath, reference: &str) -> Result<AgentPath, Error> {
        let agent = self.reach(from, reference)?;
        match self.registry.cancel(&agent.path) {
            true => Ok(agent.path),
            false => Err(Error::UnknownAgent(reference.to_string())),
        }
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
            let Ok(id) = run.session.parse::<Ulid>() else {
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
    pub fn steer(&self, from: &AgentPath, reference: &str, text: &str) -> Result<AgentPath, Error> {
        if text.trim().is_empty() {
            return Err(Error::NoObjective);
        }
        let agent = self.reach(from, reference)?;
        match self.registry.steer(&agent.path, text) {
            true => Ok(agent.path),
            false => Err(Error::UnknownAgent(reference.to_string())),
        }
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
    pub async fn transcript(&self, from: &AgentPath, child: &str) -> Result<Vec<String>, Error> {
        let outbox = self.outbox.as_ref().ok_or(Error::Unavailable)?;
        let agent = self.reach(from, child)?;
        let id = agent
            .session
            .parse()
            .map_err(|_| Error::UnknownAgent(child.to_string()))?;
        Ok(outbox.transcript(id).await)
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
        caller: &Caller,
        resuming: Option<&Agent>,
        parent_executor: Arc<dyn Executor>,
        remaining_turns: u32,
    ) -> Result<Admitted, Error> {
        if spec.objective.trim().is_empty() {
            return Err(Error::NoObjective);
        }
        if spec.name.trim().is_empty() {
            spec.name = default_name(&spec.objective);
        }
        let (path, reservation, session, inherit_from) = match resuming {
            // A follow-up continues one agent: same address, same session, so
            // its transcript stays one readable chain rather than two halves
            // under different ids.
            Some(agent) => {
                let session = agent
                    .session
                    .parse()
                    .map_err(|_| Error::UnknownAgent(agent.path.to_string()))?;
                let reservation = self.registry.revive(&agent.path)?;
                (agent.path.clone(), reservation, session, session)
            }
            None => {
                // Depth is checked against the *caller's* address before a name
                // is claimed, so a refusal costs nothing and cannot leave a
                // reservation behind.
                let depth = caller.path.depth() + 1;
                if depth > self.max_depth {
                    return Err(Error::TooDeep {
                        depth,
                        max: self.max_depth,
                    });
                }
                let (path, reservation) = self.registry.claim(&caller.path, &spec.name)?;
                (path, reservation, Ulid::new(), caller.session)
            }
        };
        spec.name = path.name().to_string();
        let permit = self.reserve()?;
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
            let narrowed: Arc<dyn Executor> = Arc::new(
                NarrowedExecutor::new(executor, Some(&requested)).no_clarifying_questions(),
            );
            (narrowed, Some(worktree))
        } else {
            // Read-only children may share the parent's tree safely; that is
            // what makes them safe to run in parallel.
            let narrowed: Arc<dyn Executor> = Arc::new(
                NarrowedExecutor::new(parent_executor, spec.tools.as_deref())
                    .read_only()
                    .no_clarifying_questions(),
            );
            (narrowed, None)
        };
        // Re-root the child's delegation tools at its own address. Applied
        // *after* narrowing so it can only replace names that survived it — a
        // read-only child has already lost `agent.spawn`, and rebinding must
        // not hand it back.
        let executor = match self.delegation.get() {
            Some(delegation) => delegation.rebind(
                executor,
                Caller {
                    path: path.clone(),
                    session,
                },
            ),
            None => executor,
        };
        // A child's budget is a slice of what the parent has left, never more.
        let max_turns = spec
            .max_turns
            .unwrap_or(remaining_turns)
            .min(remaining_turns)
            .max(1);

        // A resume inherits its *own* prior conversation, whole: a follow-up to
        // an agent that has forgotten what it already did would redo it.
        let history = match (&self.transcripts, spec.fork) {
            (_, Fork::None) | (None, _) => Vec::new(),
            (Some(transcripts), fork) => fork.apply(&transcripts.history(inherit_from).await),
        };

        // Per-child token, so cancelling one agent leaves its siblings alone;
        // it is a child of the tree's, so cancelling the tree still settles all.
        let cancel = self.cancel.child_token();
        let (steer, interrupts) = kernel::InterruptQueue::pair();
        reservation.commit(
            registry::Agent {
                path: path.clone(),
                session: session.to_string(),
                objective: spec.objective.clone(),
                started_ms: epoch_ms(),
                state: registry::State::Running,
            },
            registry::Live {
                cancel: cancel.clone(),
                steer,
            },
        );
        Ok(Admitted {
            session,
            path,
            history,
            cancel,
            interrupts,
            executor,
            workspace,
            max_turns,
            permit,
        })
    }

    /// Start a child and return its handle at once.
    ///
    /// The dispatch is persisted *before* the child starts and carries the
    /// caller's session as given, so the report reaches the agent that asked
    /// for it even across a restart — never "whichever session is current when
    /// it finishes".
    pub async fn spawn_background(
        &self,
        spec: AgentSpec,
        caller: &Caller,
        parent_executor: Arc<dyn Executor>,
        remaining_turns: u32,
    ) -> Result<Agent, Error> {
        self.start(spec, caller, None, parent_executor, remaining_turns)
            .await
    }

    /// Give more work to one of the caller's own agents.
    ///
    /// A running agent takes it as a message at its next turn boundary. A
    /// settled one is resumed — same address, same session, its own prior
    /// conversation restored — so the follow-up continues that agent instead of
    /// starting a stranger who has to rediscover everything it already knew.
    pub async fn followup(
        &self,
        caller: &Caller,
        reference: &str,
        text: &str,
        parent_executor: Arc<dyn Executor>,
        remaining_turns: u32,
    ) -> Result<Agent, Error> {
        if text.trim().is_empty() {
            return Err(Error::NoObjective);
        }
        let agent = self.reach(&caller.path, reference)?;
        if agent.is_running() {
            self.registry.steer(&agent.path, text);
            return Ok(agent);
        }
        let spec = AgentSpec {
            name: agent.path.name().to_string(),
            objective: text.to_string(),
            // Whole, and its own: a follow-up to an agent that has forgotten
            // what it already did would pay to redo it.
            fork: Fork::All,
            ..Default::default()
        };
        self.start(spec, caller, Some(&agent), parent_executor, remaining_turns)
            .await
    }

    async fn start(
        &self,
        mut spec: AgentSpec,
        caller: &Caller,
        resuming: Option<&Agent>,
        parent_executor: Arc<dyn Executor>,
        remaining_turns: u32,
    ) -> Result<Agent, Error> {
        // No durable delivery means no background: a result that cannot outlive
        // the process is not a background result, it is a lost one.
        let outbox = Arc::clone(self.outbox.as_ref().ok_or(Error::Unavailable)?);
        let parent = caller.session;
        let admitted = self
            .admit(
                &mut spec,
                caller,
                resuming,
                parent_executor,
                remaining_turns,
            )
            .await?;
        let handle = Agent {
            path: admitted.path.clone(),
            session: admitted.session.to_string(),
            objective: spec.objective.clone(),
            started_ms: epoch_ms(),
            state: registry::State::Running,
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
    path: AgentPath,
    /// The caller's conversation, filtered at admission — before the child's
    /// task is spawned, so a fork reads the history as it was when the spawn
    /// was asked for rather than whatever it has become by the time the child
    /// starts.
    history: Vec<kernel::Message>,
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
    registry: Arc<AgentRegistry>,
    settled: Arc<tokio::sync::Notify>,
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
        registry,
        settled,
    } = shared;
    let Admitted {
        session,
        cancel,
        executor,
        workspace,
        max_turns,
        permit,
        interrupts,
        path,
        history,
    } = admitted;
    let started = Instant::now();
    let run = runner.run(ChildRun {
        session,
        spec: spec.clone(),
        history,
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
    // Hand the report to the agent that asked for it. A nested parent is a
    // running session, not a surface: it has no outbox pass of its own, so
    // without this its child's result is written to a chain nobody reads and
    // the parent waits forever on work that finished.
    if let Ok(parent) = path.parent()
        && !parent.is_root()
    {
        registry.steer(&parent, &report(&result));
    }
    registry.settled(&path, result.status);
    // After the registry and the outbox, so anything woken here can read both.
    settled.notify_waiters();
    result.patch = patch;
    result
}

/// A child's result as its parent reads it. Named and status-tagged, because a
/// parent with several children cannot tell whose answer this is otherwise.
fn report(result: &AgentResult) -> String {
    let status = match result.status {
        AgentStatus::Completed => "finished",
        AgentStatus::Exhausted => "stopped on its turn budget",
        AgentStatus::Failed => "failed",
        AgentStatus::Cancelled => "was cancelled",
    };
    format!(
        "Agent '{}' {status}. Its report:\n\n{}",
        result.agent, result.summary
    )
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
#[path = "lib_tests.rs"]
mod tests;
