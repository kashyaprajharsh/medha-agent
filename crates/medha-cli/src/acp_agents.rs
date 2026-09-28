//! The live sub-agent roster for a bridge client: the same registry and
//! progress the TUI's agent tree reads, sent as one `agents` notification
//! whenever it changes, so a window can say what each agent is doing now.

use orchestrator::{Agent, AgentControl, AgentPath, State};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) struct Roster {
    control: Arc<AgentControl>,
    since_ms: u64,
    last: Option<Value>,
}

impl Roster {
    pub(crate) fn new(control: Arc<AgentControl>) -> Self {
        Self {
            control,
            since_ms: now_ms(),
            last: None,
        }
    }

    /// The roster, only when it differs from the one last sent.
    pub(crate) fn changed(&mut self) -> Option<Value> {
        let next = roster(
            &self.control.agents(),
            &self.control.progress(),
            self.since_ms,
        );
        if next["agents"].as_array().is_none_or(Vec::is_empty) && self.last.is_none() {
            return None;
        }
        (self.last.as_ref() != Some(&next)).then(|| {
            self.last = Some(next.clone());
            next
        })
    }
}

/// Agents started since this bridge opened, and any still running, in the
/// order they were started.
pub(crate) fn roster(
    agents: &[Agent],
    progress: &HashMap<AgentPath, kernel::Progress>,
    since_ms: u64,
) -> Value {
    let mut shown: Vec<&Agent> = agents
        .iter()
        .filter(|agent| agent.is_running() || agent.started_ms >= since_ms)
        .collect();
    shown.sort_by_key(|agent| agent.started_ms);
    let rows: Vec<Value> = shown
        .into_iter()
        .map(|agent| {
            let progress = progress.get(&agent.path);
            let status = match agent.state {
                State::Running => json!("running"),
                State::Settled(status) => json!(status),
            };
            json!({
                "name": agent.path.name(),
                "path": agent.path.as_str(),
                "write": agent.write,
                "session": agent.session,
                "objective": agent.objective,
                "started_ms": agent.started_ms,
                "status": status,
                "doing": progress.map(|progress| doing(&progress.phase)),
                "tool_calls": progress.map(|progress| progress.tool_calls),
                "tokens": progress.map(|progress| progress.tokens),
            })
        })
        .collect();
    json!({ "agents": rows })
}

fn doing(phase: &kernel::Phase) -> Value {
    match phase {
        kernel::Phase::Generating => json!({ "state": "thinking" }),
        kernel::Phase::InTool { tool, target } => json!({
            "state": "tool",
            "verb": transcript_view::tool_verb(tool),
            "target": target.as_deref().map(|target| transcript_view::clip(target, 160)),
        }),
        kernel::Phase::AwaitingApproval { action } => {
            json!({ "state": "waiting", "action": transcript_view::clip(action, 160) })
        }
        kernel::Phase::Idle => json!({ "state": "idle" }),
        kernel::Phase::Settled => json!({ "state": "finished" }),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "acp_agents_tests.rs"]
mod tests;
