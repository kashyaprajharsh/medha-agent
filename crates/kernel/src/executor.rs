//! The execution bridge (§4.5/§4.8). After the kernel validates and polices an
//! intent, it hands off here. Implementations own tool lookup and sandboxed
//! execution, and must always return a structured `Observation` (P10) — a tool
//! crash, denial, or timeout is data the model reasons about, never a panic.

use crate::types::{BlastRadius, Containment, Observation, ToolCategory, ToolIntent, ToolSpec};
use async_trait::async_trait;

/// An owned command the executor is currently tracking, for surfaces to display
/// while it runs — so the *user* can see what's active, not just the model.
#[derive(Debug, Clone)]
pub struct BackgroundTask {
    pub id: String,
    pub command: String,
    pub running: bool,
}

#[async_trait]
pub trait Executor: Send + Sync {
    /// Tool specs to expose into the K2 capability sheath for this session
    /// (registration ≠ exposure — only exposed specs can be called).
    fn specs(&self) -> Vec<ToolSpec>;

    /// The blast radius of a registered tool, or `None` if it isn't registered.
    /// Lets the policy authorize by radius (§4.7) instead of a hardcoded name
    /// list — the tool declares its radius once and the policy reads it.
    fn blast_radius(&self, _tool: &str) -> Option<BlastRadius> {
        None
    }

    /// The presentation category of a registered tool, or `None` if unknown.
    /// Lets the kernel label observations by provenance (e.g. `Web` results are
    /// stamped `TrustLabel::Web`) without a hardcoded tool-name list.
    fn category(&self, _tool: &str) -> Option<ToolCategory> {
        None
    }

    /// Stable identity of state this intent mutates, or `None` for a
    /// side-effect-free call.
    ///
    /// Mutation identity is deliberately separate from [`BlastRadius`].
    /// Blast radius is an authorization/verification classification, while
    /// this method drives execution ordering. Some durable operations (notably
    /// project-scoped memory writes) are intentionally low-risk enough to carry
    /// a `Read` radius but still mutate a replayable projection.
    ///
    /// The default is conservative: every non-read tool shares one global
    /// mutation lane. Executors with low-risk mutations declared as `Read`
    /// must override this method.
    fn mutation_key(&self, intent: &ToolIntent) -> Option<String> {
        match self.blast_radius(&intent.tool) {
            Some(BlastRadius::Read) | None => None,
            Some(_) => Some("state:*".to_string()),
        }
    }

    /// How strongly this executor confines command execution (§4.8). Drives the
    /// kernel's trust-flow escalation — a web-tainted action is gated unless the
    /// containment blocks exfiltration. Defaults to no containment.
    fn containment(&self) -> Containment {
        Containment::None
    }

    /// Execute one validated intent and return its observation.
    async fn execute(&self, intent: &ToolIntent) -> Observation;

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
}
