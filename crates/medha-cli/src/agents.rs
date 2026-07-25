//! The concrete [`ChildRunner`]: runs a sub-agent as a real Medha session.
//!
//! A child is `run_session` on a fresh session id with a narrowed executor and a
//! child cancellation token. Because the event log is keyed by session, the
//! child's transcript is durable, resumable and independently addressable with
//! no extra persistence — the parent only ever sees the bounded result.

use std::sync::Arc;

use kernel::{
    EventLog, Kernel, Message, NullSink, Provider, Role, Session, StopReason, TrustLabel,
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
    prompt.push_str(
        "\nYou are read-only: you cannot modify anything. Investigate, then \
         finish with your findings as your final message. That message is the \
         only thing returned, so make it complete and self-contained — cite \
         concrete file paths and line numbers where they apply.",
    );
    prompt
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

        let outcome = tokio::select! {
            biased;
            _ = run.cancel.cancelled() => None,
            outcome = child.run_session(&session, messages, budget, &NullSink, None) => Some(outcome),
        };
        let (transcript, stop) = match outcome {
            None => {
                return Ok(ChildOutcome {
                    status: AgentStatus::Cancelled,
                    summary: "cancelled before the agent reported".into(),
                    turns: 0,
                    tool_calls: 0,
                    trust: TrustLabel::Tool,
                });
            }
            Some(Ok(result)) => result,
            Some(Err(error)) => return Err(error.to_string()),
        };

        // The child's last assistant message is its report; the rest of the
        // transcript stays in the log under its own session id.
        let summary = transcript
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant && !message.content.trim().is_empty())
            .map(|message| message.content.clone())
            .unwrap_or_else(|| "the agent finished without reporting anything".into());
        let tool_calls = transcript
            .iter()
            .map(|message| message.tool_calls.len() as u32)
            .sum();
        let turns = transcript
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .count() as u32;

        Ok(ChildOutcome {
            status: match stop {
                StopReason::Finished => AgentStatus::Completed,
                StopReason::Budget(_) => AgentStatus::Exhausted,
                StopReason::Interrupted => AgentStatus::Cancelled,
            },
            summary,
            turns,
            tool_calls,
            // Every observation the child made is already labelled, so the
            // weakest one is what its summary is worth. A child that read the
            // web hands back a web-labelled result and the kernel's trust-flow
            // escalation still applies to whatever the parent does next.
            trust: orchestrator::least_trusted(
                kernel
                    .log
                    .events(run.session)
                    .await
                    .into_iter()
                    .filter(|event| event.kind == kernel::EventKind::ToolObs)
                    .map(|event| event.trust),
            ),
        })
    }
}
