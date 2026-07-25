//! Capability narrowing — the point where "a child can never widen" stops being
//! a prompt instruction and becomes a runtime property.
//!
//! Hermes filters a child's toolsets against the parent's available set at
//! construction; this does the same, and then enforces it a second time on
//! dispatch. Both halves are load-bearing: `specs()` decides what the child is
//! *shown*, but a model can name a tool it was never shown, so `execute()` must
//! refuse independently.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use kernel::{BlastRadius, Containment, Executor, Observation, ToolCategory, ToolIntent, ToolSpec};

/// An executor restricted to a subset of another executor's tools.
pub struct NarrowedExecutor {
    inner: Arc<dyn Executor>,
    allowed: BTreeSet<String>,
}

impl NarrowedExecutor {
    /// Intersect `requested` with what `inner` actually exposes. `None` requests
    /// everything the parent has — which is still only the parent's set, never
    /// more. Unknown names are dropped rather than erroring: a child asking for
    /// a tool that does not exist gets a smaller set, not a wider one.
    ///
    /// The set is snapshotted here. Tools can appear later (an MCP server
    /// connecting mid-session), and the child will not see them — the stale
    /// direction is the closed one, which is the direction to be stale in.
    pub fn new(inner: Arc<dyn Executor>, requested: Option<&[String]>) -> Self {
        let available: BTreeSet<String> = inner.specs().into_iter().map(|spec| spec.name).collect();
        let allowed = match requested {
            Some(names) => names
                .iter()
                .filter(|name| available.contains(*name))
                .cloned()
                .collect(),
            None => available,
        };
        Self { inner, allowed }
    }

    /// Drop everything that can mutate anything. Read-only children may share a
    /// workspace safely; writers may not, and writer isolation is not built yet
    /// (§6.4), so this is what keeps that impossible rather than merely
    /// discouraged.
    pub fn read_only(mut self) -> Self {
        self.allowed
            .retain(|name| matches!(self.inner.blast_radius(name), Some(BlastRadius::Read)));
        self
    }

    pub fn allows(&self, tool: &str) -> bool {
        self.allowed.contains(tool)
    }

    pub fn allowed(&self) -> impl Iterator<Item = &str> {
        self.allowed.iter().map(String::as_str)
    }
}

#[async_trait]
impl Executor for NarrowedExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner
            .specs()
            .into_iter()
            .filter(|spec| self.allowed.contains(&spec.name))
            .collect()
    }

    fn blast_radius(&self, tool: &str) -> Option<BlastRadius> {
        if !self.allows(tool) {
            return None;
        }
        self.inner.blast_radius(tool)
    }

    fn category(&self, tool: &str) -> Option<ToolCategory> {
        if !self.allows(tool) {
            return None;
        }
        self.inner.category(tool)
    }

    fn containment(&self) -> Containment {
        self.inner.containment()
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        // Second gate. The child was never shown this tool, but being unable to
        // see it is not the same as being unable to call it.
        if !self.allows(&intent.tool) {
            return Observation::denial(
                &intent.id,
                format!("'{}' is outside this agent's capabilities", intent.tool),
            );
        }
        self.inner.execute(intent).await
    }

    async fn preview(&self, intent: &ToolIntent) -> Option<String> {
        if !self.allows(&intent.tool) {
            return None;
        }
        self.inner.preview(intent).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    struct Fake;

    #[async_trait]
    impl Executor for Fake {
        fn specs(&self) -> Vec<ToolSpec> {
            ["fs.read", "fs.write", "web.search"]
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
            Observation::ok(&intent.id, Value::String(format!("ran {}", intent.tool)))
        }
    }

    fn intent(tool: &str) -> ToolIntent {
        ToolIntent {
            id: "i1".into(),
            tool: tool.into(),
            args: json!({}),
        }
    }

    fn names(executor: &NarrowedExecutor) -> Vec<String> {
        executor.specs().into_iter().map(|s| s.name).collect()
    }

    #[test]
    fn a_child_cannot_widen_past_its_parent() {
        // Asking for a tool the parent does not have yields a smaller set.
        let narrowed = NarrowedExecutor::new(
            Arc::new(Fake),
            Some(&["fs.read".into(), "shell.exec".into()]),
        );
        assert_eq!(names(&narrowed), vec!["fs.read"]);
        assert!(!narrowed.allows("shell.exec"));
        assert!(!narrowed.allows("fs.write"));
    }

    #[test]
    fn inheriting_everything_is_still_only_the_parents_set() {
        let narrowed = NarrowedExecutor::new(Arc::new(Fake), None);
        assert_eq!(names(&narrowed), vec!["fs.read", "fs.write", "web.search"]);
        assert!(!narrowed.allows("shell.exec"));
    }

    #[test]
    fn read_only_children_cannot_hold_a_mutating_tool() {
        let narrowed = NarrowedExecutor::new(Arc::new(Fake), None).read_only();
        assert_eq!(names(&narrowed), vec!["fs.read", "web.search"]);
    }

    #[tokio::test]
    async fn dispatch_refuses_a_tool_the_child_was_never_shown() {
        let narrowed = NarrowedExecutor::new(Arc::new(Fake), Some(&["fs.read".into()]));
        // Filtering specs is not enforcement: a model can name a tool it never saw.
        let denied = narrowed.execute(&intent("fs.write")).await;
        assert_eq!(denied.status, kernel::ObsStatus::Denied);
        let allowed = narrowed.execute(&intent("fs.read")).await;
        assert_eq!(allowed.status, kernel::ObsStatus::Ok);
    }

    #[tokio::test]
    async fn narrowing_composes_and_never_re_widens() {
        // A child of a child can only ever shrink further.
        let parent = Arc::new(NarrowedExecutor::new(
            Arc::new(Fake),
            Some(&["fs.read".into(), "web.search".into()]),
        ));
        let child = NarrowedExecutor::new(
            parent,
            Some(&["fs.read".into(), "fs.write".into(), "shell.exec".into()]),
        );
        assert_eq!(names(&child), vec!["fs.read"]);
        assert_eq!(
            child.execute(&intent("fs.write")).await.status,
            kernel::ObsStatus::Denied
        );
    }
}
