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

/// A command that was left running has ended, or has news on the way there.
#[derive(Debug, Clone)]
pub struct TaskEnd {
    /// The call that started it, where its changed files belong.
    pub intent_id: String,
    /// What the chat records: the task, its exit code and `_checkpoint_files`. `None` while it still runs.
    pub result: Option<serde_json::Value>,
    /// Whether this may start a turn by itself; if not, the model reads it with the next message.
    pub wake: bool,
    /// What the model reads: one line and the end of the output.
    pub note: String,
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

    async fn call_containment(&self, _intent: &ToolIntent) -> Containment {
        self.containment()
    }

    /// Execute one validated intent and return its observation.
    async fn execute(&self, intent: &ToolIntent) -> Observation;

    /// A quiet exact proposal can replace an existing approval, never a denial.
    async fn mutation_preflight(&self, _intent: &ToolIntent) -> Option<Observation> {
        None
    }

    /// Prepare an authorized mutation before its durable effect record.
    async fn prepare_effect(&self, _intent: &ToolIntent) -> Result<serde_json::Value, Observation> {
        Ok(serde_json::Value::Null)
    }

    async fn finish_effect(
        &self,
        _intent: &ToolIntent,
        _preparation: &serde_json::Value,
        _observation: &mut Observation,
    ) {
    }

    /// Whether this call only waits, so a new message may end it early without losing anything.
    fn only_waits(&self, _intent: &ToolIntent) -> bool {
        false
    }

    /// Apply a memory change once its event is logged, so the log always holds what the index does.
    async fn apply_logged(&self, _tool: &str, _applied: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }

    /// Observe only the final model-facing result after fitting and storage.
    fn observation_delivered(&self, _tool: &str, _observation: &Observation) {}

    fn changed_files(&self, tool: &str, observation: &Observation) -> bool {
        !matches!(
            observation.status,
            crate::ObsStatus::Denied | crate::ObsStatus::Rejected | crate::ObsStatus::SchemaInvalid
        ) && matches!(
            self.blast_radius(tool),
            Some(BlastRadius::ReversibleLocal | BlastRadius::IrreversibleLocal)
        ) && observation
            .payload
            .get("changed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    }

    /// The call did not do what it was asked: an error, or a tool that ran but reports failing (a command's exit code).
    fn failed(&self, _tool: &str, observation: &Observation) -> bool {
        observation.status == crate::ObsStatus::Error
    }

    async fn check_outbound(&self, _intent: &ToolIntent) -> Result<(), String> {
        Ok(())
    }

    /// Validate requested capabilities and return the subset not already granted.
    fn missing_access(&self, _intent: &ToolIntent) -> Result<crate::ExecutionAccess, String> {
        Ok(crate::ExecutionAccess::default())
    }

    async fn grant_access(
        &self,
        _access: &crate::ExecutionAccess,
        _ask: &crate::AccessAsk,
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

    /// The command an intent runs and where, for an access card. Default: not a command.
    fn command_line(&self, _intent: &ToolIntent) -> Option<crate::CommandLine> {
        None
    }

    /// Owned commands currently running, for a surface to show the user.
    /// Default: none (executors without a task table).
    fn background_tasks(&self) -> Vec<BackgroundTask> {
        Vec::new()
    }

    /// Where commands that were left running report their end; handed out once, to the chat that owns them.
    fn task_endings(
        &self,
        _session: &str,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<TaskEnd>> {
        None
    }

    /// The person asks that a running command stop holding its call; false when none is waiting.
    fn send_to_background(&self) -> bool {
        false
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
