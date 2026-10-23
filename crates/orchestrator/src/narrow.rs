//! Capability narrowing — the point where "a child can never widen" stops being
//! a prompt instruction and becomes a runtime property.
//!
//! The requested set is intersected with the parent's at construction, and then
//! enforced a second time on dispatch. Both halves are load-bearing: `specs()`
//! decides what the child is *shown*, but a model can name a tool it was never
//! shown, so `execute()` must refuse independently.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use kernel::{BlastRadius, Containment, Executor, Observation, ToolCategory, ToolIntent, ToolSpec};

/// An executor restricted to a subset of another executor's tools.
pub struct NarrowedExecutor {
    inner: Arc<dyn Executor>,
    scope: Scope,
    read_only: bool,
    no_clarify: bool,
}

enum Scope {
    /// Snapshotted at construction: what a child was handed.
    Fixed(BTreeSet<String>),
    /// Asked on every use, so a tool that appears later, such as an MCP server's, is judged too.
    Live(Box<dyn Fn(&str) -> bool + Send + Sync>),
}

impl NarrowedExecutor {
    /// Intersect `requested` with what `inner` exposes; `None` means the parent's
    /// whole set, never more. Unknown names are dropped, so a child gets a smaller
    /// set rather than an error. Snapshotted here, so later tools stay invisible.
    pub fn new(inner: Arc<dyn Executor>, requested: Option<&[String]>) -> Self {
        let available: BTreeSet<String> = inner.specs().into_iter().map(|spec| spec.name).collect();
        // A name that is not available is dropped, so narrowing can only ever
        // shrink what the child is allowed to call.
        let allowed = match requested {
            Some(names) => names
                .iter()
                .filter(|name| available.contains(*name))
                .cloned()
                .collect(),
            None => available,
        };
        Self::scoped(inner, Scope::Fixed(allowed))
    }

    /// Keep the tools `admit` accepts, judged whenever a tool is listed or called.
    pub fn filtered(
        inner: Arc<dyn Executor>,
        admit: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self::scoped(inner, Scope::Live(Box::new(admit)))
    }

    fn scoped(inner: Arc<dyn Executor>, scope: Scope) -> Self {
        Self {
            inner,
            scope,
            read_only: false,
            no_clarify: false,
        }
    }

    /// Drop everything that can mutate anything. Read-only children may share a
    /// workspace safely; writers require an isolated workspace.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Drop `clarify`: a child cannot see the conversation, so its question is
    /// unanswerable and a background one blocks forever. Not permission gating —
    /// consequential actions still route through the human gate.
    pub fn no_clarifying_questions(mut self) -> Self {
        self.no_clarify = true;
        self
    }

    pub fn allows(&self, tool: &str) -> bool {
        let in_scope = match &self.scope {
            Scope::Fixed(allowed) => allowed.contains(tool),
            Scope::Live(admit) => admit(tool),
        };
        in_scope
            && (!self.read_only || matches!(self.inner.blast_radius(tool), Some(BlastRadius::Read)))
            && !(self.no_clarify && tool == "clarify")
    }

    fn allows_intent(&self, intent: &ToolIntent) -> bool {
        self.allows(&intent.tool) && (!self.read_only || self.inner.mutation_key(intent).is_none())
    }
}

#[async_trait]
impl Executor for NarrowedExecutor {
    async fn mutation_preflight(&self, intent: &ToolIntent) -> Option<Observation> {
        if !self.allows_intent(intent) {
            return Some(Observation::denial(
                &intent.id,
                "Outside this agent's capabilities",
            ));
        }
        self.inner.mutation_preflight(intent).await
    }

    async fn prepare_effect(&self, intent: &ToolIntent) -> Result<serde_json::Value, Observation> {
        if !self.allows_intent(intent) {
            return Err(Observation::denial(
                &intent.id,
                "Outside this agent's capabilities",
            ));
        }
        self.inner.prepare_effect(intent).await
    }

    async fn finish_effect(
        &self,
        intent: &ToolIntent,
        preparation: &serde_json::Value,
        observation: &mut Observation,
    ) {
        if self.allows_intent(intent) {
            self.inner
                .finish_effect(intent, preparation, observation)
                .await;
        }
    }

    fn observation_delivered(&self, tool: &str, observation: &Observation) {
        if self.allows(tool) {
            self.inner.observation_delivered(tool, observation);
        }
    }

    fn changed_files(&self, tool: &str, observation: &Observation) -> bool {
        self.allows(tool) && self.inner.changed_files(tool, observation)
    }

    fn background_tasks(&self) -> Vec<kernel::BackgroundTask> {
        if self.read_only {
            Vec::new()
        } else {
            self.inner.background_tasks()
        }
    }

    async fn tool_inventory(
        &self,
        previous_sources: &std::collections::BTreeMap<String, String>,
    ) -> kernel::tool_snapshot::ToolInventory {
        let mut inventory = self.inner.tool_inventory(previous_sources).await;
        inventory.specs.retain(|spec| self.allows(&spec.name));
        inventory.sources.retain(|name, _| self.allows(name));
        inventory.unavailable.retain(|name| self.allows(name));
        inventory
    }

    fn specs(&self) -> Vec<ToolSpec> {
        self.inner
            .specs()
            .into_iter()
            .filter(|spec| self.allows(&spec.name))
            .collect()
    }

    fn blast_radius(&self, tool: &str) -> Option<BlastRadius> {
        if !self.allows(tool) {
            return None;
        }
        self.inner.blast_radius(tool)
    }

    fn call_radius(&self, intent: &ToolIntent) -> Option<BlastRadius> {
        if !self.allows(&intent.tool) {
            return None;
        }
        self.inner.call_radius(intent)
    }

    fn category(&self, tool: &str) -> Option<ToolCategory> {
        if !self.allows(tool) {
            return None;
        }
        self.inner.category(tool)
    }

    fn mutation_key(&self, intent: &ToolIntent) -> Option<String> {
        if !self.allows(&intent.tool) {
            return None;
        }
        self.inner.mutation_key(intent)
    }

    fn containment(&self) -> Containment {
        self.inner.containment()
    }
    async fn call_containment(&self, intent: &ToolIntent) -> Containment {
        self.inner.call_containment(intent).await
    }
    async fn check_outbound(&self, intent: &ToolIntent) -> Result<(), String> {
        if !self.allows_intent(intent) {
            return Err(format!(
                "'{}' is outside this agent's capabilities",
                intent.tool
            ));
        }
        self.inner.check_outbound(intent).await
    }

    fn missing_access(&self, intent: &ToolIntent) -> Result<kernel::ExecutionAccess, String> {
        if !self.allows_intent(intent) {
            return Err(format!(
                "'{}' is outside this agent's capabilities",
                intent.tool
            ));
        }
        self.inner.missing_access(intent)
    }

    async fn grant_access(
        &self,
        access: &kernel::ExecutionAccess,
        detail: &str,
        escalated: bool,
    ) -> Result<kernel::NetworkDecision, String> {
        self.inner.grant_access(access, detail, escalated).await
    }

    fn allows_network_retry(&self, intent: &ToolIntent) -> bool {
        self.allows_intent(intent) && self.inner.allows_network_retry(intent)
    }

    async fn grant_network(
        &self,
        detail: Option<&str>,
        escalated: bool,
    ) -> kernel::NetworkDecision {
        self.inner.grant_network(detail, escalated).await
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        // Second gate. The child was never shown this tool, but being unable to
        // see it is not the same as being unable to call it.
        if !self.allows_intent(intent) {
            return Observation::denial(
                &intent.id,
                format!("'{}' is outside this agent's capabilities", intent.tool),
            );
        }
        self.inner.execute(intent).await
    }

    async fn preview(&self, intent: &ToolIntent) -> Option<String> {
        if !self.allows_intent(intent) {
            return None;
        }
        self.inner.preview(intent).await
    }

    fn images_reach_model(&self, reach: bool) {
        self.inner.images_reach_model(reach);
    }

    async fn named_outputs(&self, text: &str, since: f64) -> Vec<serde_json::Value> {
        self.inner.named_outputs(text, since).await
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
            ["read", "edit", "web.search"]
                .iter()
                .map(|name| ToolSpec {
                    name: (*name).to_string(),
                    description: String::new(),
                    schema: json!({}),
                    blast_radius: if *name == "edit" {
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

    /// Exposes the delegation tools alongside a mutating one, which is the
    /// shape a writing child actually inherits.
    struct Delegating;

    #[async_trait]
    impl Executor for Delegating {
        fn specs(&self) -> Vec<ToolSpec> {
            ["read", "edit", "agent.spawn", "agent.cancel"]
                .iter()
                .map(|name| ToolSpec {
                    name: (*name).to_string(),
                    description: String::new(),
                    schema: json!({}),
                    // `agent.spawn` is not a read: it spends real money, which
                    // is why `read_only` happened to strip it and why a writer
                    // kept it.
                    blast_radius: if *name == "read" {
                        BlastRadius::Read
                    } else {
                        BlastRadius::ReversibleLocal
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

    /// `clarify` as the registry declares it: a read, because asking changes
    /// nothing.
    struct Asking;

    #[async_trait]
    impl Executor for Asking {
        fn specs(&self) -> Vec<ToolSpec> {
            ["read", "clarify"]
                .iter()
                .map(|name| ToolSpec {
                    name: (*name).to_string(),
                    description: String::new(),
                    schema: json!({}),
                    blast_radius: BlastRadius::Read,
                    category: ToolCategory::Other,
                    icon: "•".into(),
                })
                .collect()
        }
        fn blast_radius(&self, _tool: &str) -> Option<BlastRadius> {
            Some(BlastRadius::Read)
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
        let narrowed =
            NarrowedExecutor::new(Arc::new(Fake), Some(&["read".into(), "shell.exec".into()]));
        assert_eq!(names(&narrowed), vec!["read"]);
        assert!(!narrowed.allows("shell.exec"));
        assert!(!narrowed.allows("edit"));
    }

    #[test]
    fn inheriting_everything_is_still_only_the_parents_set() {
        let narrowed = NarrowedExecutor::new(Arc::new(Fake), None);
        assert_eq!(names(&narrowed), vec!["read", "edit", "web.search"]);
        assert!(!narrowed.allows("shell.exec"));
    }

    #[test]
    fn a_writer_keeps_the_delegation_tools_the_depth_limit_now_governs() {
        let writer = NarrowedExecutor::new(Arc::new(Delegating), None);
        assert!(names(&writer).contains(&"edit".to_string()));
        // Nesting is bounded by the child's path depth, not by hiding the tool.
        assert!(writer.allows("agent.spawn"));
    }

    /// Asking a question mutates nothing, so `clarify` is a *read* and survives
    /// `read_only`. The child prompt tells a child it cannot ask; without this
    /// that was a claim rather than a fact, and a background child could raise
    /// a dialog nobody could attribute.
    #[test]
    fn a_child_cannot_block_on_an_open_question_it_has_no_one_to_ask() {
        let child = NarrowedExecutor::new(Arc::new(Asking), None).read_only();
        assert!(
            child.allows("clarify"),
            "read_only leaves clarify — which is why this filter has to exist separately"
        );
        let child = NarrowedExecutor::new(Arc::new(Asking), None)
            .read_only()
            .no_clarifying_questions();
        assert!(!child.allows("clarify"));
        assert!(child.allows("read"), "it can still do its work");
    }

    #[test]
    fn read_only_children_cannot_hold_a_mutating_tool() {
        let narrowed = NarrowedExecutor::new(Arc::new(Fake), None).read_only();
        assert_eq!(names(&narrowed), vec!["read", "web.search"]);
    }

    #[tokio::test]
    async fn dispatch_refuses_a_tool_the_child_was_never_shown() {
        let narrowed = NarrowedExecutor::new(Arc::new(Fake), Some(&["read".into()]));
        // Filtering specs is not enforcement: a model can name a tool it never saw.
        let denied = narrowed.execute(&intent("edit")).await;
        assert_eq!(denied.status, kernel::ObsStatus::Denied);
        let allowed = narrowed.execute(&intent("read")).await;
        assert_eq!(allowed.status, kernel::ObsStatus::Ok);
    }

    #[tokio::test]
    async fn read_only_children_reject_mutating_ops_on_merged_read_tools() {
        struct Mixed;
        #[async_trait]
        impl Executor for Mixed {
            fn specs(&self) -> Vec<ToolSpec> {
                Fake.specs()
            }
            fn blast_radius(&self, tool: &str) -> Option<BlastRadius> {
                Fake.blast_radius(tool)
            }
            fn mutation_key(&self, intent: &ToolIntent) -> Option<String> {
                (intent.args["op"] == "write").then(|| "memory:*".to_owned())
            }
            async fn execute(&self, intent: &ToolIntent) -> Observation {
                Fake.execute(intent).await
            }
        }
        let parent = Arc::new(NarrowedExecutor::new(Arc::new(Mixed), None).read_only());
        let child = NarrowedExecutor::new(parent, None);
        let mut call = intent("read");
        call.args = json!({"op": "write"});
        assert!(child.missing_access(&call).is_err());
        assert_eq!(child.execute(&call).await.status, kernel::ObsStatus::Denied);
        call.args = json!({"op": "search"});
        assert_eq!(child.execute(&call).await.status, kernel::ObsStatus::Ok);
    }

    /// A registry whose tool list grows, as it does when an MCP server connects mid-session.
    struct Growing(std::sync::Mutex<Vec<&'static str>>);

    #[async_trait]
    impl Executor for Growing {
        fn specs(&self) -> Vec<ToolSpec> {
            let connected = self.0.lock().unwrap().clone();
            std::iter::once("read")
                .chain(connected)
                .map(|name| ToolSpec {
                    name: name.into(),
                    description: String::new(),
                    schema: json!({}),
                    blast_radius: BlastRadius::Read,
                    category: ToolCategory::Other,
                    icon: "•".into(),
                })
                .collect()
        }
        fn blast_radius(&self, _tool: &str) -> Option<BlastRadius> {
            Some(BlastRadius::Read)
        }
        async fn execute(&self, intent: &ToolIntent) -> Observation {
            Fake.execute(intent).await
        }
        async fn named_outputs(&self, _text: &str, _since: f64) -> Vec<Value> {
            vec![json!({"path": "deck.pptx", "kind": "deck"})]
        }
    }

    #[tokio::test]
    async fn a_rule_judges_tools_that_connect_later_and_a_denied_one_stays_uncallable() {
        let inner = Arc::new(Growing(Default::default()));
        let session = NarrowedExecutor::filtered(inner.clone(), |tool| tool != "mcp__db__drop");
        inner
            .0
            .lock()
            .unwrap()
            .extend(["mcp__db__query", "mcp__db__drop"]);
        assert_eq!(names(&session), vec!["read", "mcp__db__query"]);
        let refused = session.execute(&intent("mcp__db__drop")).await;
        assert_eq!(
            refused.status,
            kernel::ObsStatus::Denied,
            "hidden is not enough; naming it must fail"
        );
        let child = NarrowedExecutor::new(Arc::new(session), Some(&["mcp__db__drop".into()]));
        assert!(
            names(&child).is_empty(),
            "a child cannot win back what its parent denies"
        );
    }

    #[tokio::test]
    async fn outputs_a_reply_names_survive_narrowing() {
        let narrowed = NarrowedExecutor::filtered(Arc::new(Growing(Default::default())), |_| true);
        assert_eq!(narrowed.named_outputs("made deck.pptx", 0.0).await.len(), 1);
    }

    #[tokio::test]
    async fn narrowing_composes_and_never_re_widens() {
        // A child of a child can only ever shrink further.
        let parent = Arc::new(NarrowedExecutor::new(
            Arc::new(Fake),
            Some(&["read".into(), "web.search".into()]),
        ));
        let child = NarrowedExecutor::new(
            parent,
            Some(&["read".into(), "edit".into(), "shell.exec".into()]),
        );
        assert_eq!(names(&child), vec!["read"]);
        assert_eq!(
            child.execute(&intent("edit")).await.status,
            kernel::ObsStatus::Denied
        );
    }
}
