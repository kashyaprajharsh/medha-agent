//! The concrete [`ChildRunner`]: runs a sub-agent as a real Medha session.
//!
//! A child is `run_session` on a fresh session id with a narrowed executor and a
//! child cancellation token. Because the event log is keyed by session, the
//! child's transcript is durable, resumable and independently addressable with
//! no extra persistence — the parent only ever sees the bounded result.

use std::sync::Arc;

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
    prompt.push_str(&format!(
        "\nYou are read-only: you cannot modify anything. Investigate, then \
         finish with your findings as your final message.\n\n\
         You have {} turns. That is a hard stop — when it runs out you are cut \
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
