//! Hands a child agent's steps to the terminal's event loop.

use super::TuiEvent;
use runtime::agents::{AgentStep, AgentWatcher};
use tokio::sync::mpsc::UnboundedSender;

/// Unbounded so a slow screen never stalls a child mid-tool; the viewer keeps a bounded ring.
pub(crate) struct AgentWatch {
    pub(crate) tx: UnboundedSender<TuiEvent>,
}

impl AgentWatcher for AgentWatch {
    fn step(
        &self,
        surface_session: Option<ulid::Ulid>,
        path: &orchestrator::AgentPath,
        step: AgentStep,
    ) {
        let _ = self.tx.send(TuiEvent::AgentStep {
            surface_session,
            path: path.clone(),
            step,
        });
    }

    fn usage(&self, usage: &kernel::Usage) {
        let _ = self.tx.send(TuiEvent::Usage(*usage));
    }
}

#[cfg(test)]
#[path = "agent_watch_tests.rs"]
mod tests;
