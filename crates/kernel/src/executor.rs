//! Execution bridge for validated, authorized tool intents.

use crate::gate::NetworkDecision;
use crate::types::{BlastRadius, Containment, Observation, ToolCategory, ToolIntent, ToolSpec};
use async_trait::async_trait;

/// A tracked background command.
#[derive(Debug, Clone)]
pub struct BackgroundTask {
    pub id: String,
    pub command: String,
    pub running: bool,
}

#[async_trait]
pub trait Executor: Send + Sync {
    /// Tool specs exposed for this session.
    fn specs(&self) -> Vec<ToolSpec>;

    /// Fresh definitions plus registered tools that are temporarily unavailable.
    async fn tool_inventory(
        &self,
        _previous_sources: &std::collections::BTreeMap<String, String>,
    ) -> crate::tool_snapshot::ToolInventory {
        crate::tool_snapshot::ToolInventory {
            specs: self.specs(),
            ..Default::default()
        }
    }

    /// The registered tool's blast radius, or `None` if unknown.
    fn blast_radius(&self, _tool: &str) -> Option<BlastRadius> {
        None
    }

    /// Risk of this one call, for a tool whose verb is an argument. Defaults to
    /// the tool's own, which is never lower.
    fn call_radius(&self, intent: &ToolIntent) -> Option<BlastRadius> {
        self.blast_radius(&intent.tool)
    }

    /// The registered tool's presentation category, or `None` if unknown.
    fn category(&self, _tool: &str) -> Option<ToolCategory> {
        None
    }

    /// Stable identity of state this intent mutates, or `None` if side-effect
    /// free. Separate from [`BlastRadius`], which authorizes rather than orders —
    /// a `Read` radius can still mutate a replayable projection. Defaults to one
    /// global mutation lane, so such executors must override this.
    fn mutation_key(&self, intent: &ToolIntent) -> Option<String> {
        match self.blast_radius(&intent.tool) {
            Some(BlastRadius::Read) | None => None,
            Some(_) => Some("state:*".to_string()),
        }
    }

    /// How strongly this executor confines command execution. Drives the
    /// kernel's trust-flow escalation — a web-tainted action is gated unless the
    /// containment blocks exfiltration. Defaults to no containment.
    fn containment(&self) -> Containment {
        Containment::None
    }

    /// Execute one validated intent and return its observation.
    async fn execute(&self, intent: &ToolIntent) -> Observation;

    /// Validate requested capabilities and return the subset not already granted.
    fn missing_access(&self, _intent: &ToolIntent) -> Result<crate::ExecutionAccess, String> {
        Ok(crate::ExecutionAccess::default())
    }

    async fn grant_access(
        &self,
        _access: &crate::ExecutionAccess,
        _detail: &str,
        _escalated: bool,
    ) -> Result<NetworkDecision, String> {
        Ok(NetworkDecision::Deny)
    }

    /// Arbitrary shell commands may have partial side effects and must not replay.
    fn allows_network_retry(&self, _intent: &ToolIntent) -> bool {
        true
    }

    /// Prompt to grant the sandbox network access and retry after a command
    /// failed under a net-denying jail. A session or persistent grant flips the
    /// shared network flag (and, for persistent, records it durably) so the retry
    /// and later commands reach the network. Defaults to deny — an executor with
    /// no sandbox cannot open a network it does not confine.
    async fn grant_network(&self, _detail: Option<&str>, _escalated: bool) -> NetworkDecision {
        NetworkDecision::Deny
    }

    /// Side-effect-free preview of an intent (e.g. a rendered diff), for the
    /// human gate. Async because building a real diff means reading the file's
    /// current contents through the (async) sandbox.
    async fn preview(&self, _intent: &ToolIntent) -> Option<String> {
        None
    }

    /// Owned commands currently running, for a surface to show the user.
    /// Default: none (executors without a task table).
    fn background_tasks(&self) -> Vec<BackgroundTask> {
        Vec::new()
    }

    /// Whether an image can reach the model (it reads them, or one is described
    /// for it); image-only tools decline when it cannot. Default: ignored.
    fn images_reach_model(&self, _reach: bool) {}

    /// Files a reply names that changed since `since` (Unix seconds), each as
    /// `{"path", "kind"}`, for a surface to show beside the reply. Default: none.
    async fn named_outputs(&self, _text: &str, _since: f64) -> Vec<serde_json::Value> {
        Vec::new()
    }
}
