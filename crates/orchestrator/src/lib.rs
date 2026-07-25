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

/// Ceiling on children alive at once. Hermes defaults to 3 and warns past 10,
/// on the grounds that each child burns tokens independently; the same reasoning
/// applies here.
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
    #[error("no agent named '{0}'")]
    UnknownAgent(String),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Completed,
    /// Stopped on a budget ceiling; partial work is still reported.
    Exhausted,
    Failed,
    Cancelled,
}

impl AgentStatus {
    pub fn is_final(self) -> bool {
        true
    }
}

/// What the parent sees. Never the child's transcript — that stays in the event
/// log under the child's own session id, durable and resumable.
#[derive(Debug, Clone, Serialize)]
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
    active: Arc<futures::lock::Mutex<Vec<AgentHandle>>>,
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
            active: Arc::new(futures::lock::Mutex::new(Vec::new())),
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

    /// Currently running children, for the UI.
    pub async fn active(&self) -> Vec<AgentHandle> {
        self.active.lock().await.clone()
    }

    /// Reserve capacity before anything is published. Returns `AtCapacity`
    /// rather than queueing: an unbounded backlog of children is worse than a
    /// refusal the model can react to.
    fn reserve(&self) -> Result<OwnedSemaphorePermit, Error> {
        Arc::clone(&self.capacity)
            .try_acquire_owned()
            .map_err(|_| Error::AtCapacity(self.max_active))
    }

    /// Run one child to completion and return its bounded result. Foreground
    /// spawn waits (§6.2); background delivery is O2.
    pub async fn spawn(
        &self,
        mut spec: AgentSpec,
        parent_executor: Arc<dyn Executor>,
        remaining_turns: u32,
    ) -> Result<AgentResult, Error> {
        if spec.objective.trim().is_empty() {
            return Err(Error::NoObjective);
        }
        if self.depth >= self.max_depth {
            return Err(Error::TooDeep {
                depth: self.depth,
                max: self.max_depth,
            });
        }
        let _permit = self.reserve()?;

        if spec.name.trim().is_empty() {
            spec.name = default_name(&spec.objective);
        }
        // O1 children are read-only: writer isolation does not exist yet, so a
        // child that could mutate the workspace could corrupt its parent's.
        let executor: Arc<dyn Executor> =
            Arc::new(NarrowedExecutor::new(parent_executor, spec.tools.as_deref()).read_only());
        // A child's budget is a slice of what the parent has left, never more.
        let max_turns = spec
            .max_turns
            .unwrap_or(remaining_turns)
            .min(remaining_turns)
            .max(1);

        let session = Ulid::new();
        let handle = AgentHandle {
            agent: spec.name.clone(),
            session: session.to_string(),
            objective: spec.objective.clone(),
            started_ms: 0,
        };
        self.active.lock().await.push(handle.clone());

        let started = Instant::now();
        let outcome = self
            .runner
            .run(ChildRun {
                session,
                spec: spec.clone(),
                executor,
                max_turns,
                // Child token: cancelling the parent settles every descendant.
                cancel: self.cancel.child_token(),
            })
            .await;
        self.active
            .lock()
            .await
            .retain(|entry| entry.session != session.to_string());

        let elapsed = started.elapsed();
        match outcome {
            Ok(outcome) => Ok(bound(spec.name, session, outcome, elapsed)),
            Err(error) => Err(Error::Failed(error)),
        }
    }
}

/// Trim a child's summary to something a parent can hold in context. The caller
/// spills the remainder to the artifact store and fills in `artifact`.
fn bound(name: String, session: Ulid, outcome: ChildOutcome, elapsed: Duration) -> AgentResult {
    let truncated = outcome.summary.chars().count() > MAX_SUMMARY_CHARS;
    let summary = if truncated {
        let cut = outcome
            .summary
            .char_indices()
            .nth(MAX_SUMMARY_CHARS)
            .map_or(outcome.summary.len(), |(index, _)| index);
        format!(
            "{}\n… [full result in the artifact store]",
            &outcome.summary[..cut]
        )
    } else {
        outcome.summary
    };
    AgentResult {
        agent: name,
        session: session.to_string(),
        status: outcome.status,
        summary,
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
