//! The concrete [`ChildRunner`]: runs a sub-agent as a real Medha session.
//!
//! A child is `run_session` on a fresh session id with a narrowed executor and a
//! child cancellation token. Because the event log is keyed by session, the
//! child's transcript is durable, resumable and independently addressable with
//! no extra persistence — the parent only ever sees the bounded result.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kernel::{
    Event, EventKind, EventLog, Kernel, Message, NullSink, Provider, Role, Session, StopReason,
    TrustLabel,
};
use orchestrator::{AgentStatus, ChildOutcome, ChildRun, ChildRunner};

/// Framing for a delegated task. The child cannot see the parent's conversation,
/// so the objective has to stand alone — this says so explicitly rather than
/// letting the model assume shared context.
fn child_prompt(run: &ChildRun) -> String {
    let mut prompt = format!(
        "You are a focused sub-agent. You have been delegated one task and you \
         cannot see the conversation that produced it, so work only from what is \
         written here.\n\nTask:\n{}\n",
        run.spec.objective
    );
    if let Some(contract) = &run.spec.contract {
        prompt.push_str(&format!("\nYour answer must be: {contract}\n"));
    }
    match &run.workspace {
        // A writer is told plainly that its tree is private. Without this the
        // model hedges — it avoids edits it was asked to make, or narrates a
        // patch instead of applying one, because it assumes it is touching the
        // user's live files.
        Some(_) => prompt.push_str(
            "\nYou can modify code. You are working in a private checkout of the \
             repository that nobody else can see: your edits affect nothing \
             outside it, so make them directly rather than describing them. Do \
             not commit, and do not use git to branch, stash or reset — your \
             changes are collected as a diff exactly as you leave them in the \
             working tree.\n\n\
             Build or test what you changed before you finish, and say what you \
             ran. Your patch is reviewed by a human alongside that evidence, so \
             an honest \"this compiles but I could not run the tests\" is worth \
             far more than a confident claim that does not hold.\n",
        ),
        None => prompt.push_str(
            "\nYou are read-only: you cannot modify anything. Investigate, then \
             finish with your findings as your final message.\n",
        ),
    }
    prompt.push_str(&format!(
        "\nYou have {} turns. That is a hard stop — when it runs out you are cut \
         off wherever you are, and whatever you had said last is what gets \
         returned. So do not save the answer for the end: once you are around \
         two thirds through, stop exploring and write what you have, marking \
         anything you could not confirm. A partial answer delivered is worth \
         more than a complete one you never reached.\n\n\
         Your final message is the only thing returned, and it lands in the \
         parent's context window — so keep it tight. Lead with the answer, use \
         bullets over paragraphs, cite concrete file paths and line numbers, and \
         do not replay how you got there. If you could not answer, say so plainly \
         and say what you ruled out.",
        run.max_turns
    ));
    prompt
}

/// Everything needed to rebuild the workspace sandbox at another root.
///
/// A writer's sandbox has to be *the parent's sandbox with a different root* —
/// same permission store, same audit log, same execution backend, same
/// snapshotting. Rebuilding it from a template rather than mutating the
/// parent's is what keeps the two independent: a child must not be able to
/// widen the permissions its parent is operating under.
#[derive(Clone)]
pub struct SandboxTemplate {
    pub trust: PathBuf,
    pub audit: PathBuf,
    pub gate: Arc<dyn kernel::HumanGate>,
    pub exec: Arc<dyn sandbox::ExecBackend>,
    pub snapshots: PathBuf,
    pub readable: Vec<PathBuf>,
}

/// Shared slot for the registry a child's tools are rebased from. The registry
/// is what hosts `agent.spawn`, so it cannot exist before the control plane
/// that owns this — the same cycle `agent_parent_handle` breaks, broken the
/// same way.
pub type RegistryHandle = Arc<Mutex<Option<Arc<tools::ToolRegistry>>>>;

/// Writer isolation over git worktrees (§6.4).
pub struct WorktreeWorkspaces {
    pool: orchestrator::WorktreePool,
    repo: PathBuf,
    registry: RegistryHandle,
    template: SandboxTemplate,
    /// The project's verification command, run inside the child's checkout so
    /// its patch arrives with evidence rather than a claim.
    verify: Option<String>,
}

impl WorktreeWorkspaces {
    /// Build isolation for `repo`, with checkouts under `dir`.
    ///
    /// Returns `None` when the workspace is not a git repository: writers are
    /// then refused outright, which is the only safe answer. Degrading to
    /// "write in the parent's tree" would be exactly the collision this exists
    /// to prevent, and it would be silent.
    pub async fn discover(
        repo: &Path,
        dir: PathBuf,
        registry: RegistryHandle,
        template: SandboxTemplate,
        verify: Option<String>,
    ) -> Option<Self> {
        let pool = orchestrator::WorktreePool::discover(repo, dir).await.ok()?;
        // Clear anything a crashed run left behind before the first checkout —
        // `git worktree add` refuses a path that already exists, so a leftover
        // would fail the *next* agent for a reason that has nothing to do with
        // it.
        pool.sweep().await;
        Some(Self {
            pool,
            repo: repo.to_path_buf(),
            registry,
            template,
            verify,
        })
    }

    pub fn registry_handle() -> RegistryHandle {
        Arc::new(Mutex::new(None))
    }
}

#[async_trait::async_trait]
impl orchestrator::Workspaces for WorktreeWorkspaces {
    async fn checkout(&self, session: ulid::Ulid) -> Result<orchestrator::Workspace, String> {
        let registry = self
            .registry
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .ok_or("the tool registry is not available yet")?;
        let worktree = self
            .pool
            .checkout(session)
            .await
            .map_err(|error| error.to_string())?;
        let sandbox = sandbox::WorkspaceSandbox::new(
            worktree.path(),
            self.template.trust.clone(),
            self.template.audit.clone(),
            Some(Arc::clone(&self.template.gate)),
        )
        .map_err(|error| error.to_string())?
        .with_exec_backend(Arc::clone(&self.template.exec))
        .with_readable_roots(&self.template.readable)
        .with_snapshots_dir(self.template.snapshots.clone());

        let executor = registry
            .rebase(Arc::new(sandbox))
            .ok_or("this session's tools are not rooted in a workspace")?;
        Ok(orchestrator::Workspace {
            worktree,
            executor: Arc::new(executor),
        })
    }

    async fn verify(&self, root: &Path) -> Option<orchestrator::Verification> {
        let command = self.verify.clone()?;
        let output = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .current_dir(root)
            .kill_on_drop(true)
            .output()
            .await
        {
            Ok(output) => output,
            // A configured verifier that could not run is a *failure*, not an
            // absence. Reporting `None` here would read as "this project has no
            // verifier" and wave the patch straight through the merge gate.
            Err(error) => {
                return Some(orchestrator::Verification {
                    command,
                    passed: false,
                    output: format!("could not run the verify command: {error}"),
                });
            }
        };
        let mut text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // The tail, because that is where a build or test run says what failed;
        // the head is setup noise. Bounded so a verbose suite cannot flood the
        // parent's context through the patch.
        const MAX_LINES: usize = 60;
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() > MAX_LINES {
            text = format!(
                "[… {} earlier line(s)]\n{}",
                lines.len() - MAX_LINES,
                lines[lines.len() - MAX_LINES..].join("\n")
            );
        }
        Some(orchestrator::Verification {
            command,
            passed: output.status.success(),
            output: text,
        })
    }

    fn repo(&self) -> PathBuf {
        self.repo.clone()
    }
}

/// Durable delivery folded from the dispatching session's own event chain.
///
/// Medha's log is append-only, hash-chained and already per-session, so the
/// outbox needs no storage of its own: `agent.spawned` is the dispatch row,
/// a terminal event carries the report, and `agent.delivered` closes it. State
/// is whatever the fold says, which means it survives a restart for free and
/// cannot disagree with the audit trail.
pub struct LogOutbox<L: EventLog> {
    log: Arc<L>,
}

impl<L: EventLog> LogOutbox<L> {
    pub fn new(log: Arc<L>) -> Self {
        Self { log }
    }
}

/// Events are addressed by session id; the rest of `Session` is not read when
/// one is appended, so this is enough to write onto another session's chain.
fn chain(id: ulid::Ulid) -> Session {
    Session {
        id,
        done: false,
        autonomy: kernel::AutonomyLevel::Careful,
    }
}

#[async_trait::async_trait]
impl<L: EventLog + 'static> orchestrator::Outbox for LogOutbox<L> {
    async fn dispatched(&self, dispatch: &orchestrator::Dispatch) {
        // On the parent's chain, and written before the child starts: a crash
        // in between leaves a dispatch with no terminal event, which reads as an
        // orphan rather than as nothing having happened.
        let _ = self
            .log
            .append(Event::agent_dispatched(
                &chain(dispatch.parent),
                &dispatch.agent,
                dispatch.child,
                &dispatch.objective,
            ))
            .await;
    }

    async fn finished(
        &self,
        dispatch: &orchestrator::Dispatch,
        result: &orchestrator::AgentResult,
    ) {
        let kind = match result.status {
            orchestrator::AgentStatus::Cancelled => EventKind::AgentCancelled,
            orchestrator::AgentStatus::Failed => EventKind::AgentFailed,
            _ => EventKind::AgentCompleted,
        };
        let payload = serde_json::to_value(result).unwrap_or_default();
        let _ = self
            .log
            .append(Event::agent_report(
                &chain(dispatch.parent),
                kind,
                dispatch.child,
                payload,
                result.trust,
            ))
            .await;
    }

    async fn delivered(&self, parent: ulid::Ulid, child: ulid::Ulid) {
        let _ = self
            .log
            .append(Event::agent_delivered(&chain(parent), child))
            .await;
    }

    async fn transcript(&self, child: ulid::Ulid) -> Vec<String> {
        // The child's own chain, rendered as readable lines. A report is a
        // summary by design; when one looks thin the work behind it has to be
        // reachable, or the only recourse is guessing.
        self.log
            .events(child)
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::UserMessage => Some(format!(
                    "objective: {}",
                    event.payload["text"].as_str().unwrap_or_default()
                )),
                EventKind::ModelText => Some(format!(
                    "said: {}",
                    event.payload["text"].as_str().unwrap_or_default()
                )),
                EventKind::ModelIntent => Some(format!(
                    "called {}({})",
                    event.payload["tool"].as_str().unwrap_or("?"),
                    event.payload["args"]
                )),
                EventKind::ToolObs => Some(format!(
                    "  → {}",
                    event.payload["status"].as_str().unwrap_or("ok")
                )),
                _ => None,
            })
            .collect()
    }

    async fn recorded(
        &self,
        parent: ulid::Ulid,
        agent: &str,
        child: ulid::Ulid,
        patch: &orchestrator::Patch,
    ) {
        // On the owner's chain, like every other outbox row. The child's
        // worktree is already reaped by the time this runs, so this event is
        // the work — a failure to append here loses it outright.
        let payload = serde_json::to_value(patch).unwrap_or_default();
        let _ = self
            .log
            .append(Event::agent_patch(&chain(parent), agent, child, payload))
            .await;
    }

    async fn applied(&self, parent: ulid::Ulid, child: ulid::Ulid) {
        let _ = self
            .log
            .append(Event::agent_applied(&chain(parent), child))
            .await;
    }

    async fn unapplied(&self, parent: ulid::Ulid) -> Vec<(String, String, orchestrator::Patch)> {
        let mut recorded: Vec<(String, String, orchestrator::Patch)> = Vec::new();
        let mut applied: Vec<String> = Vec::new();
        for event in self.log.events(parent).await {
            let child = event
                .payload
                .get("child")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            match event.kind {
                EventKind::AgentPatch => {
                    if let Some(patch) = event.payload.get("patch").cloned()
                        && let Ok(patch) = serde_json::from_value(patch)
                    {
                        let agent = event.payload["agent"]
                            .as_str()
                            .unwrap_or("agent")
                            .to_string();
                        recorded.push((agent, child, patch));
                    }
                }
                EventKind::AgentApplied => applied.push(child),
                _ => {}
            }
        }
        // The fold is what makes a re-apply impossible, rather than a flag
        // someone has to remember to clear.
        recorded
            .into_iter()
            .filter(|(_, child, _)| !applied.contains(child))
            .collect()
    }

    async fn undelivered(&self, parent: ulid::Ulid) -> Vec<orchestrator::AgentResult> {
        let mut ready: Vec<(ulid::Ulid, orchestrator::AgentResult)> = Vec::new();
        let mut delivered: Vec<String> = Vec::new();
        for event in self.log.events(parent).await {
            let child = event
                .payload
                .get("child")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            match event.kind {
                EventKind::AgentCompleted | EventKind::AgentFailed | EventKind::AgentCancelled => {
                    if let Ok(result) = serde_json::from_value(event.payload.clone())
                        && let Ok(id) = child.parse()
                    {
                        ready.push((id, result));
                    }
                }
                EventKind::AgentDelivered => delivered.push(child),
                _ => {}
            }
        }
        // Oldest first, and never one already handed over — the fold is what
        // makes redelivery impossible rather than a flag someone has to remember.
        ready.sort_by_key(|(id, _)| *id);
        ready
            .into_iter()
            .filter(|(id, _)| !delivered.contains(&id.to_string()))
            .map(|(_, result)| result)
            .collect()
    }
}

pub struct KernelRunner<P: Provider, L: EventLog> {
    /// Weak on purpose: the kernel owns the executor that hosts `agent.spawn`,
    /// which owns the control plane that owns this runner. A strong handle would
    /// close that cycle and leak the kernel for the process lifetime.
    kernel: std::sync::Weak<Kernel<P, L>>,
}

impl<P: Provider, L: EventLog> KernelRunner<P, L> {
    pub fn new(kernel: &Arc<Kernel<P, L>>) -> Self {
        Self {
            kernel: Arc::downgrade(kernel),
        }
    }
}

#[async_trait::async_trait]
impl<P: Provider + 'static, L: EventLog + 'static> ChildRunner for KernelRunner<P, L> {
    async fn run(&self, run: ChildRun) -> Result<ChildOutcome, String> {
        let Some(kernel) = self.kernel.upgrade() else {
            return Err("this session is shutting down".into());
        };
        // Same kernel — provider, log, artifacts, policy and gate are shared —
        // but a narrowed executor, so the child's capabilities are its own.
        let child = Kernel::new(
            Arc::clone(&kernel.provider),
            Arc::clone(&kernel.log),
            Arc::clone(&run.executor),
            Arc::clone(&kernel.context),
            Arc::clone(&kernel.artifacts),
            Arc::clone(&kernel.policy),
            Arc::clone(&kernel.gate),
            Arc::clone(&kernel.verifier),
        );

        let session = Session {
            id: run.session,
            done: false,
            autonomy: kernel::AutonomyLevel::Careful,
        };
        let budget = kernel::Budget {
            max_turns: Some(run.max_turns),
            ..Default::default()
        };
        let messages = vec![Message::new(Role::User, child_prompt(&run))];

        // The child's chain opens with what it was asked to do, so its session
        // stands on its own when read back later.
        let tools: Vec<String> = run.executor.specs().into_iter().map(|s| s.name).collect();
        let _ = kernel
            .log
            .append(Event::agent_spawned(
                &session,
                &run.spec.name,
                &run.spec.objective,
                &tools,
            ))
            .await;

        let outcome = tokio::select! {
            biased;
            _ = run.cancel.cancelled() => None,
            outcome = child.run_session(&session, messages, budget, &NullSink, None) => Some(outcome),
        };
        let (transcript, stop) = match outcome {
            None => {
                let _ = kernel
                    .log
                    .append(Event::agent_finished(
                        &session,
                        EventKind::AgentCancelled,
                        &run.spec.name,
                        "cancelled by the parent",
                        TrustLabel::Tool,
                    ))
                    .await;
                return Ok(ChildOutcome {
                    status: AgentStatus::Cancelled,
                    summary: "cancelled before the agent reported".into(),
                    turns: 0,
                    tool_calls: 0,
                    trust: TrustLabel::Tool,
                });
            }
            Some(Ok(result)) => result,
            Some(Err(error)) => {
                let _ = kernel
                    .log
                    .append(Event::agent_finished(
                        &session,
                        EventKind::AgentFailed,
                        &run.spec.name,
                        &error.to_string(),
                        TrustLabel::Tool,
                    ))
                    .await;
                return Err(error.to_string());
            }
        };

        // The child's last assistant message is its report — but only when it
        // chose to stop. A child cut off by its budget was mid-sentence, so its
        // last message is a narration fragment ("Let me compile the table…"),
        // and handing that to the parent as an answer is worse than useless: it
        // reads as a report and sends the parent hunting for content that was
        // never written.
        let last = transcript
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant && !message.content.trim().is_empty())
            .map(|message| message.content.clone());
        let summary = match (&stop, last) {
            (StopReason::Finished, Some(text)) => text,
            (StopReason::Finished, None) => "the agent finished without reporting anything".into(),
            // Say what happened first, then offer the fragment as evidence of
            // where it got to rather than as the answer.
            (_, Some(text)) => format!(
                "[incomplete — the agent was stopped before it reported. \
                 Treat the following as where it had got to, not as an answer. \
                 Re-run with a narrower objective or a higher turn budget.]\n\n{text}"
            ),
            (_, None) => "the agent was stopped before it produced anything".into(),
        };
        let tool_calls = transcript
            .iter()
            .map(|message| message.tool_calls.len() as u32)
            .sum();
        let turns = transcript
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .count() as u32;

        // Every observation the child made is already labelled, so the weakest
        // one is what its summary is worth. A child that read the web hands back
        // a web-labelled result and the kernel's trust-flow escalation still
        // applies to whatever the parent does next.
        let trust = orchestrator::least_trusted(
            kernel
                .log
                .events(run.session)
                .await
                .into_iter()
                .filter(|event| event.kind == EventKind::ToolObs)
                .map(|event| event.trust),
        );
        let status = match stop {
            StopReason::Finished => AgentStatus::Completed,
            StopReason::Budget(_) => AgentStatus::Exhausted,
            StopReason::Interrupted => AgentStatus::Cancelled,
        };
        let _ = kernel
            .log
            .append(Event::agent_finished(
                &session,
                match status {
                    AgentStatus::Cancelled => EventKind::AgentCancelled,
                    AgentStatus::Failed => EventKind::AgentFailed,
                    _ => EventKind::AgentCompleted,
                },
                &run.spec.name,
                &format!("{turns} turn(s), {tool_calls} tool call(s)"),
                trust,
            ))
            .await;

        Ok(ChildOutcome {
            status,
            summary,
            turns,
            tool_calls,
            trust,
        })
    }
}
