//! Delegation tools bound to the calling agent's address and limits.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kernel::{BlastRadius, ToolCategory};
use orchestrator::{AgentPath, Caller, Waited};
use serde_json::{Value, json};

use crate::{Tool, ToolError, arg_str};

/// Builds delegation tools through a weak control-plane handle.
pub struct Delegate {
    control: std::sync::Weak<orchestrator::AgentControl>,
    executor: ParentHandle,
    max_turns: u32,
    session: SessionHandle,
}

impl Delegate {
    pub fn new(
        control: &Arc<orchestrator::AgentControl>,
        executor: ParentHandle,
        max_turns: u32,
        session: SessionHandle,
    ) -> Self {
        Self {
            control: Arc::downgrade(control),
            executor,
            max_turns,
            session,
        }
    }

    pub fn tools_for_root(&self) -> Vec<Arc<dyn Tool>> {
        self.tools_for(CallerSlot::Root(Arc::clone(&self.session)))
    }

    fn tools_for(&self, caller: CallerSlot) -> Vec<Arc<dyn Tool>> {
        let Some(control) = self.control.upgrade() else {
            return Vec::new();
        };
        let verb = |action| AgentControlTool {
            control: Arc::clone(&control),
            action,
            caller: caller.clone(),
            executor: Arc::clone(&self.executor),
            max_turns: self.max_turns,
        };
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(AgentSpawn {
            control: Arc::clone(&control),
            executor: Arc::clone(&self.executor),
            max_turns: self.max_turns,
            caller: caller.clone(),
            followup: verb(AgentAction::Followup),
        })];
        tools.push(Arc::new(AgentControls {
            verbs: [
                AgentAction::List,
                AgentAction::Cancel,
                AgentAction::Transcript,
                AgentAction::Steer,
                AgentAction::Message,
                AgentAction::Wait,
            ]
            .map(verb)
            .into(),
        }));
        if control.can_write() {
            tools.push(Arc::new(AgentApply { control }));
        }
        tools
    }
}

impl orchestrator::Delegation for Delegate {
    fn rebind(
        &self,
        executor: Arc<dyn kernel::Executor>,
        caller: Caller,
    ) -> Arc<dyn kernel::Executor> {
        let exposed: std::collections::HashSet<String> =
            executor.specs().into_iter().map(|spec| spec.name).collect();
        // Only names the child already holds. A read-only child has lost
        // `agent.spawn` to narrowing, and re-rooting must not hand it back.
        let rebound: std::collections::HashMap<String, Arc<dyn Tool>> = self
            .tools_for(CallerSlot::Agent(caller))
            .into_iter()
            .filter(|tool| exposed.contains(tool.name()))
            .map(|tool| (tool.name().to_string(), tool))
            .collect();
        if rebound.is_empty() {
            return executor;
        }
        Arc::new(Rebound {
            inner: executor,
            tools: rebound,
        })
    }
}

/// Rebinds existing delegation tools without adding capabilities.
struct Rebound {
    inner: Arc<dyn kernel::Executor>,
    tools: std::collections::HashMap<String, Arc<dyn Tool>>,
}

#[async_trait]
impl kernel::Executor for Rebound {
    fn specs(&self) -> Vec<kernel::ToolSpec> {
        self.inner
            .specs()
            .into_iter()
            .map(|spec| match self.tools.get(&spec.name) {
                Some(tool) => kernel::ToolSpec {
                    description: tool.description().to_string(),
                    schema: tool.schema(),
                    ..spec
                },
                None => spec,
            })
            .collect()
    }

    fn blast_radius(&self, tool: &str) -> Option<BlastRadius> {
        self.inner.blast_radius(tool)
    }

    fn category(&self, tool: &str) -> Option<ToolCategory> {
        self.inner.category(tool)
    }

    fn mutation_key(&self, intent: &kernel::ToolIntent) -> Option<String> {
        match self.tools.get(&intent.tool) {
            Some(tool) => tool.mutation_key(&intent.args),
            None => self.inner.mutation_key(intent),
        }
    }

    fn containment(&self) -> kernel::Containment {
        self.inner.containment()
    }

    fn missing_access(
        &self,
        intent: &kernel::ToolIntent,
    ) -> Result<kernel::ExecutionAccess, String> {
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

    fn allows_network_retry(&self, intent: &kernel::ToolIntent) -> bool {
        self.inner.allows_network_retry(intent)
    }

    async fn grant_network(
        &self,
        detail: Option<&str>,
        escalated: bool,
    ) -> kernel::NetworkDecision {
        self.inner.grant_network(detail, escalated).await
    }

    async fn execute(&self, intent: &kernel::ToolIntent) -> kernel::Observation {
        let Some(tool) = self.tools.get(&intent.tool) else {
            return self.inner.execute(intent).await;
        };
        // The inner executor is the authority on whether this child may call
        // this at all; the overlay only changes *whose* call it is.
        if self.inner.blast_radius(&intent.tool).is_none() {
            return kernel::Observation::denial(
                &intent.id,
                format!("'{}' is outside this agent's capabilities", intent.tool),
            );
        }
        crate::run_tool(tool.as_ref(), intent).await
    }

    async fn preview(&self, intent: &kernel::ToolIntent) -> Option<String> {
        match self.tools.get(&intent.tool) {
            Some(tool) => tool.preview(&intent.args).await,
            None => self.inner.preview(intent).await,
        }
    }
}

/// Delegate a bounded task to a child agent. The child is built from this
/// objective — nothing has to be registered first — and runs with its own
/// transcript, so its intermediate work never enters the parent's context.
struct AgentSpawn {
    control: Arc<orchestrator::AgentControl>,
    executor: ParentHandle,
    /// Operator ceiling on one child's turns, from `[agents] max_turns`.
    max_turns: u32,
    /// Whose spawn this is. Children hang off this address and reports are
    /// addressed back to this session, so a nested agent nests under itself
    /// rather than under whoever happens to be at the root.
    caller: CallerSlot,
    /// Naming an existing agent gives it more work instead of starting a new
    /// one. It is admitted exactly as a spawn is — same radius, same absence of
    /// a turn cap — so it belongs behind the same name rather than beside it.
    followup: AgentControlTool,
}

#[async_trait]
impl Tool for AgentSpawn {
    fn name(&self) -> &str {
        "agent.spawn"
    }
    fn icon(&self) -> &'static str {
        "⚇"
    }
    /// What the model needs *before* it calls this: when delegation is the right
    /// move, and how to write an objective a cold child can act on. What to do
    /// afterwards — do not poll, a writer's patch is not on disk, a report is a
    /// claim not a fact — is delivered with the call's own result and with the
    /// report, so a session that never delegates does not pay for it on every
    /// turn. This description is re-sent on every request; those notes are not.
    fn description(&self) -> &str {
        "Delegate a self-contained task to a child agent and get back a summary. \
         The child works in its own context, so none of its searching lands in this conversation.\n\
         \n\
         Reach for this on your own judgement — you do not need to be asked. Delegate when:\n\
         · reviewing or auditing a whole workspace — split independent subsystems into one \
         parallel `tasks` call and keep one slice for yourself;\n\
         · answering needs a broad sweep whose intermediate output you will never need again — \
         'how is X used across the codebase', 'what does this unfamiliar module do';\n\
         · two or more questions are independent, so children can run at once;\n\
         · a side investigation would otherwise crowd out the context you need for the real task;\n\
         · you want background gathered while you keep working — every child runs in the \
         background and reports back on its own.\n\
         \n\
         Do NOT delegate what a direct tool answers faster and cheaper: a file you can name, a \
         definition or usage (`grep`, `glob`, `code`), two or three known files, or work you \
         already have the context for — a child starts cold and pays to rediscover what you \
         know. Never hand over your whole task: that is pass-through, and it doubles the cost \
         for nothing. Split off a *part*, or do it yourself.\n\
         \n\
         To run several at once, pass `tasks` — one call, N children, all concurrent. That is \
         strictly better than spawning them one at a time and waiting for each.\n\
         \n\
         Children are read-only by default. Set `write` for a child that must change code: it \
         gets its own private checkout and hands back a patch plus the result of building it. \
         Two writing children can safely run at once; they cannot see each other\'s changes.\n\
         \n\
         State the objective in full: the child cannot see this conversation and cannot ask you \
         anything — put every path, error message and constraint it needs in the objective \
         itself, including any language, tone or format the user asked for, or its summary comes \
         back in the wrong one and contaminates your reply. Give `contract` when the answer must \
         have a particular shape. A child can never use a tool you do not already have; it can \
         message any running agent but only steer or stop the ones it started; a read-only child \
         cannot delegate further."
    }
    fn blast_radius(&self) -> BlastRadius {
        // Read-only children: no mutation, but real model spend, so it stays
        // above a plain read.
        BlastRadius::ReversibleLocal
    }
    /// Consequential enough to need approval, but it changes nothing in the
    /// shared tree — so it must not hold the writer lane.
    ///
    /// The default derives a mutation key from the blast radius, which took
    /// `mutation_serial` and a durable cross-process lease for the whole
    /// admission, including a writer's `git worktree add`. Every other session's
    /// mutating tool blocked for that window, a second process blocked on the
    /// lease, and two spawns could never overlap. Nothing here needed it: the
    /// worktree has its own structure lock, the dispatch record is ordered by the
    /// event log's single-writer chain and already refuses the spawn if it cannot
    /// be written, and an orphaned child is recovered from `agent.spawned` plus
    /// its process lease rather than from an effect record.
    ///
    /// It is also what made a blocking spawn impossible: holding the writer lane
    /// across a child's run deadlocks the first thing that child writes.
    fn mutation_key(&self, _args: &Value) -> Option<String> {
        None
    }
    fn timeout(&self, _args: &Value) -> Option<std::time::Duration> {
        // No tool-level cap. A child is a whole session — it runs to its own
        // turn budget, which is the bound that means anything here. The default
        // 60s killed any child that did real work, and killed it *silently* from
        // the parent's side: the report was lost even though the child had been
        // making progress. Cancellation still settles it, through the parent's
        // own interrupt and the child's token.
        None
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Other
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "The complete task. The child sees only this — not this conversation."
                },
                "agent": {
                    "type": "string",
                    "description": "Continue one of your own agents instead of starting a new one, by name or session id. Give `text` with it."
                },
                "text": {
                    "type": "string",
                    "description": "With `agent`: the further work. Stands alone — it cannot see this conversation."
                },
                "name": { "type": "string", "description": "Short label for the agent (optional)" },
                "contract": {
                    "type": "string",
                    "description": "What the result must contain, e.g. 'a list of file:line with one sentence each'"
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Narrow the child to these tools. Omit this — inheriting your set is almost always right, and a child missing something it turns out to need cannot ask for it. Name tools exactly as they are registered, such as web, shell.exec, and read. Use this only to take a capability away deliberately. Cannot exceed yours, and reading files is always kept."
                },
                "max_turns": { "type": "integer", "description": "Turn ceiling, clamped to what remains" },
                "wait": {
                    "type": "boolean",
                    "description": "Hold this call until the child answers, and return its report here. Use it when you cannot take the next step without the result — a failure then comes back as an error on this call instead of as a report you have to notice later. Leave it off for a genuine fan-out you want to keep working alongside; those reports arrive on their own. Typing while it waits detaches it and the agent keeps running."
                },
                "write": {
                    "type": "boolean",
                    "description": "REQUIRED for any task that changes something — write, edit, add, fix, rename, create a file. Without it the child gets read-only tools, cannot edit anything, and will come back describing the change it would have made instead of making it. It works in a private checkout and returns a patch, so your files are never touched until you apply it with `agent.apply`. Refused if this workspace is not a git repository."
                },
                "fork": {
                    "type": "string",
                    "description": "How much of this conversation the child inherits: 'none' (default — it works from the objective alone), 'all', or a number of recent turns. Raise it only when the task genuinely depends on what was said earlier and you cannot restate it in the objective; the child pays for that history in its own context."
                },
                // One entry per child, each taking the same fields as above. The
                // meanings are not restated here: every one of them would be
                // re-sent on every request for the sake of a form most calls
                // never use.
                "tasks": {
                    "type": "array",
                    "description": "Run several independent investigations at once, each its own agent, and get every report back together. Use this instead of one call per question — they run concurrently rather than in sequence. Give `tasks` OR `objective`, not both. Each entry takes the same fields as a single spawn.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "objective": { "type": "string" },
                            "name": { "type": "string" },
                            "contract": { "type": "string" },
                            "tools": { "type": "array", "items": { "type": "string" } },
                            "max_turns": { "type": "integer" },
                            "write": { "type": "boolean" },
                            "fork": { "type": "string" }
                        },
                        "required": ["objective"]
                    }
                }
            }
        })
    }
    async fn preview(&self, args: &Value) -> Option<String> {
        let kind = match args.get("write").and_then(Value::as_bool).unwrap_or(false) {
            true => "a writing agent (private checkout, returns a patch)",
            false => "a read-only agent",
        };
        Some(format!(
            "delegate to {kind}:\n{}",
            args.get("objective")?.as_str()?
        ))
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        // Naming an agent means continuing that one, not starting another.
        if args.get("agent").is_some() {
            return self.followup.execute(args).await;
        }
        // Batch: independent questions run at once rather than one call after
        // another. Capacity is already bounded per tree, so an over-large batch
        // is refused by the runtime rather than flooding it.
        if let Some(tasks_value) = args.get("tasks") {
            let tasks = tasks_value
                .as_array()
                .ok_or_else(|| ToolError::Args("expected array 'tasks'".into()))?;
            if tasks.is_empty() {
                return Err(ToolError::Args("tasks is empty".into()));
            }
            // Parse the complete batch before launching its first child. A bad
            // later row must not return an error after earlier rows have already
            // acquired capacity and started invisibly.
            let specs: Vec<orchestrator::AgentSpec> = tasks
                .iter()
                .map(parse_agent_spec)
                .collect::<Result<_, _>>()?;
            let Some(parent) = parent_executor(&self.executor) else {
                return Err(ToolError::Failed(
                    "the agent runtime is not available in this session".into(),
                ));
            };
            let caller = self.caller.resolve()?;
            let mut started = Vec::with_capacity(specs.len());
            for spec in specs {
                // One refusal must not discard its siblings, so each is
                // reported on its own terms.
                started.push(
                    match self
                        .control
                        .spawn_background(
                            spec,
                            &caller,
                            parent.clone(),
                            child_budget(&self.control, &caller, self.max_turns),
                        )
                        .await
                    {
                        Ok(agent) => json!({
                            "agent": agent.path,
                            "session": agent.session,
                            "status": "running",
                        }),
                        Err(error) => json!({ "status": "refused", "reason": error.to_string() }),
                    },
                );
            }
            if args.get("wait").and_then(Value::as_bool).unwrap_or(false) {
                let mine: Vec<AgentPath> = started
                    .iter()
                    .filter_map(|row| row.get("agent"))
                    .filter_map(|path| serde_json::from_value(path.clone()).ok())
                    .collect();
                return wait_for(&self.control, &caller.path, caller.session, mine, started).await;
            }
            return Ok(json!({
                "agents": started,
                "count": started.len(),
                "note": "All started. Their reports arrive on their own — do not poll or wait \
                         unless you cannot continue without them, in which case wait on one.",
            }));
        }
        let spec = parse_agent_spec(args)?;
        let writing = spec.write;
        let Some(parent) = parent_executor(&self.executor) else {
            return Err(ToolError::Failed(
                "the agent runtime is not available in this session".into(),
            ));
        };
        // Every child runs asynchronously. Waiting inside the call blocked the
        // turn for as long as the child ran, with no timeout and no way out but
        // an interrupt that discarded the work — and bought nothing, since the
        // report arrives on its own either way.
        let caller = self.caller.resolve()?;
        let agent = self
            .control
            .spawn_background(
                spec,
                &caller,
                parent,
                child_budget(&self.control, &caller, self.max_turns),
            )
            .await
            .map_err(|error| match error {
                orchestrator::Error::BadTools(message) => ToolError::Args(message),
                other => ToolError::Failed(other.to_string()),
            })?;
        let started = vec![json!({
            "agent": agent.path,
            "session": agent.session,
            "status": "running",
        })];
        if args.get("wait").and_then(Value::as_bool).unwrap_or(false) {
            return wait_for(
                &self.control,
                &caller.path,
                caller.session,
                vec![agent.path.clone()],
                started,
            )
            .await;
        }
        let mut note = "Started. Its report arrives on its own when ready — do not poll, do not \
                        read its transcript to check progress, and do not start doing its work. \
                        If you need the result before continuing, wait on it."
            .to_string();
        if writing {
            note.push_str(
                " It is writing in its own private checkout, so nothing it changes appears in \
                 your files: it hands back a patch to review and apply.",
            );
        }
        Ok(json!({
            "agent": agent.path,
            "session": agent.session,
            "status": "running",
            "note": note,
        }))
    }
}

/// Shared slot for the session a background report belongs to.
pub type SessionHandle = Arc<Mutex<Option<ulid::Ulid>>>;

/// Intersects the operator cap with the caller's inherited budget.
fn child_budget(
    control: &orchestrator::AgentControl,
    caller: &Caller,
    max_turns: u32,
) -> kernel::Budget {
    let mut budget = control.budget_for(&caller.path);
    let caller_ceiling = budget.max_turns.unwrap_or(max_turns);
    budget.max_turns = Some(max_turns.min(caller_ceiling));
    budget
}

/// Weak executor slot that breaks the registry/tool ownership cycle.
pub type ParentHandle = Arc<Mutex<Option<std::sync::Weak<dyn kernel::Executor>>>>;

fn parent_executor(slot: &ParentHandle) -> Option<Arc<dyn kernel::Executor>> {
    slot.lock().ok()?.as_ref()?.upgrade()
}

/// Hold the turn until `mine` have settled, then hand back their reports.
///
/// The alternative — return at once and let the reports arrive on a later turn —
/// stays the default, and is the better shape for a genuine fan-out. This is for
/// the case where the caller cannot continue without the answer: a failure then
/// surfaces as a tool error it already knows how to handle, rather than as a
/// report saying FAILED that it has to notice a turn later.
///
/// Typing detaches. The control plane watches the operator's interrupt handle, so
/// a wait ends the moment its own operator speaks — the children carry on and
/// their reports arrive through the outbox, exactly as an unwaited spawn's would.
async fn wait_for(
    control: &orchestrator::AgentControl,
    from: &AgentPath,
    owner: ulid::Ulid,
    mine: Vec<AgentPath>,
    started: Vec<Value>,
) -> Result<Value, ToolError> {
    let timeout = control.wait_bounds().max;
    let deadline = std::time::Instant::now() + timeout;
    let mut outstanding: Vec<AgentPath> = mine;
    loop {
        // Only this call's children. Waiting on the whole roster would return as
        // soon as any unrelated sibling finished.
        outstanding.retain(|path| {
            control
                .address(from, &path.to_string())
                .map(|agent| agent.is_running())
                .unwrap_or(false)
        });
        if outstanding.is_empty() {
            break;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ok(json!({
                "agents": started,
                "waited": false,
                "note": "Still running past the wait ceiling. They were not stopped; their \
                         reports arrive on their own.",
            }));
        }
        match control.wait(from, left).await {
            Waited::Settled(_) => continue,
            Waited::TimedOut => continue,
            // The operator spoke. Read what they said before anything else.
            Waited::Interrupted => {
                return Ok(json!({
                    "agents": started,
                    "waited": false,
                    "detached": true,
                    "note": "Stopped waiting — a message arrived for you, and it may change what \
                             you were waiting for. The agents are untouched and still running; \
                             their reports arrive on their own.",
                }));
            }
            Waited::Cancelled => {
                return Err(ToolError::Failed(
                    "this session is shutting down; the agents were stopped".into(),
                ));
            }
        }
    }
    let collected = control.collect(owner).await;
    // Not acknowledged here: execution precedes the durable observation append,
    // and a failure in that gap would lose a report the caller never saw. The
    // outbox folds these ids out of the observation, so the append is the commit.
    let acknowledgements: Vec<&str> = collected
        .iter()
        .map(|result| result.dispatch.as_str())
        .collect();
    let relayed = orchestrator::least_trusted(collected.iter().map(|result| result.trust));
    let reports: Vec<Value> = collected
        .iter()
        .map(|result| {
            json!({
                "agent": result.agent,
                "session": result.session,
                "status": result.status,
                "report": result.summary,
            })
        })
        .collect();
    Ok(json!({
        "agents": started,
        "waited": true,
        "reports": reports,
        "note": "These are their answers. Do not read their transcripts.",
        orchestrator::REPORT_ACKS_FIELD: acknowledgements,
        crate::RELAYED_TRUST: relayed,
    }))
}

/// An omitted fork mode starts the child cold, which is what its objective and
/// its briefing both already promise.
fn parse_fork(args: &Value) -> Result<orchestrator::Fork, ToolError> {
    match args.get("fork") {
        None => Ok(orchestrator::Fork::default()),
        Some(value) => value
            .as_str()
            .ok_or_else(|| ToolError::Args("'fork' must be a string".into()))
            .and_then(|text| orchestrator::Fork::parse(text).map_err(ToolError::Args)),
    }
}

fn parse_agent_spec(args: &Value) -> Result<orchestrator::AgentSpec, ToolError> {
    let optional_string = |key: &str| -> Result<Option<String>, ToolError> {
        match args.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .map(|text| Some(text.to_string()))
                .ok_or_else(|| ToolError::Args(format!("'{key}' must be a string"))),
        }
    };
    let max_turns = match args.get("max_turns") {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| ToolError::Args("'max_turns' must be an integer".into()))?
                .min(u32::MAX as u64) as u32,
        ),
    };
    let write = match args.get("write") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| ToolError::Args("'write' must be a boolean".into()))?,
    };
    Ok(orchestrator::AgentSpec {
        name: optional_string("name")?.unwrap_or_default(),
        objective: arg_str(args, "objective")?,
        contract: optional_string("contract")?,
        tools: optional_string_list(args, "tools")?,
        max_turns,
        write,
        fork: parse_fork(args)?,
    })
}

/// Parse an optional string-array without turning malformed narrowing into
/// `None`, which means "inherit every parent capability" to the orchestrator.
fn optional_string_list(args: &Value, key: &str) -> Result<Option<Vec<String>>, ToolError> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| ToolError::Args(format!("expected array '{key}'")))?;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                ToolError::Args(format!("'{key}[{index}]' must be a tool-name string"))
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Keeps nested reports addressed to the session that spawned them.
#[derive(Clone)]
enum CallerSlot {
    Root(SessionHandle),
    Agent(Caller),
}

impl CallerSlot {
    fn path(&self) -> AgentPath {
        match self {
            Self::Root(_) => AgentPath::root(),
            Self::Agent(caller) => caller.path.clone(),
        }
    }

    fn resolve(&self) -> Result<Caller, ToolError> {
        match self {
            Self::Agent(caller) => Ok(caller.clone()),
            Self::Root(session) => session
                .lock()
                .ok()
                .and_then(|slot| *slot)
                .map(Caller::root)
                .ok_or_else(|| {
                    ToolError::Failed("no session to deliver this agent's report to".into())
                }),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentAction {
    List,
    Cancel,
    Transcript,
    Steer,
    Message,
    Followup,
    Wait,
}

impl AgentAction {
    fn verb(&self) -> &'static str {
        match self {
            AgentAction::List => "list",
            AgentAction::Cancel => "cancel",
            AgentAction::Transcript => "transcript",
            AgentAction::Steer => "steer",
            AgentAction::Message => "message",
            AgentAction::Followup => "followup",
            AgentAction::Wait => "wait",
        }
    }
}

/// Addressing agents that already exist: see them, read them, tell them
/// something, wait for one, stop one. Every verb here only reads or redirects,
/// so they share one name and one Read radius.
///
/// Starting work stays outside it: a spawn or a follow-up is `ReversibleLocal`
/// because it spends real money and a writer resumes as a writer, and folding
/// that in would have put an approval card in front of every verb that merely
/// looks.
struct AgentControls {
    verbs: Vec<AgentControlTool>,
}

impl AgentControls {
    fn of(&self, args: &Value) -> Result<&AgentControlTool, ToolError> {
        let asked = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Args("expected string 'action'".into()))?;
        self.verbs
            .iter()
            .find(|verb| verb.action.verb() == asked)
            .ok_or_else(|| {
                ToolError::Args(format!(
                    "unknown action '{asked}'; expected one of {}",
                    self.verbs
                        .iter()
                        .map(|verb| verb.action.verb())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }
}

#[async_trait]
impl Tool for AgentControls {
    fn name(&self) -> &str {
        "agent"
    }
    fn icon(&self) -> &'static str {
        "⚇"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn description(&self) -> &str {
        "Address the agents already running under you.\n\
         \n\
         `list` — what each one is doing right now: its objective, `doing` (current \
         phase), and the running counters `tool_calls` and `tokens`. `quiet_ms` \
         appears only in phases where silence can mean a stall, and is null while an \
         agent waits on the operator. Do not poll, and do not cancel an agent merely \
         because it is quiet.\n\
         `transcript` — what an agent actually did, by session id. A report is a \
         summary; when one looks thin, wrong, or cut short, read the work behind it \
         instead of guessing or re-running the search yourself. `tail` limits it to \
         the last N steps.\n\
         `steer` — send `text` to one of your own agents that is still running: a \
         correction, a constraint you forgot, a narrowing of scope. It arrives at the \
         agent's next step and does not restart it or discard what it has found. Use \
         it the moment you realise an agent is working from something wrong — the \
         only alternative is cancelling and paying for the whole run again, and an \
         agent cannot ask you a question when it gets stuck.\n\
         `message` — send `text` to any live agent in this session, including the one \
         that started you. For passing along what the other agent needs and cannot \
         find on its own: a finding from your work, an answer its objective left \
         open, a constraint that arrived after it started. Unlike `steer`, the target \
         need not be your own child.\n\
         `cancel` — stop one agent by name or session id. Its siblings keep running, \
         and whatever it had found is still reported.\n\
         `wait` — pause until a running agent finishes, and only when you genuinely \
         cannot continue without its answer. Returns as soon as one settles, or empty \
         if `timeout_seconds` passes first, so it is bounded and a timeout is not a \
         failure. Prefer not to: agents report on their own, and waiting spends the \
         turn doing nothing."
    }
    /// Per verb: a wait is bounded by its own `timeout_seconds`, checked against
    /// the operator's ceiling before it starts. A tool-level cap on top of that
    /// turns a legitimate long wait into an error that reads as a failed agent.
    fn timeout(&self, args: &Value) -> Option<std::time::Duration> {
        match self.of(args) {
            Ok(verb) => verb.timeout(args),
            Err(_) => Some(crate::TOOL_TIMEOUT),
        }
    }
    fn schema(&self) -> Value {
        let bounds = self
            .verbs
            .first()
            .map(|verb| verb.control.wait_bounds())
            .unwrap_or_default();
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "transcript", "steer", "message", "cancel", "wait"],
                    "description": "Which verb to apply"
                },
                "agent": { "type": "string", "description": "Agent name, path, or session id — required by transcript, steer, message and cancel" },
                "text": { "type": "string", "description": "steer/message: what to tell it. Stands alone — the agent cannot see this conversation." },
                "tail": { "type": "integer", "description": "transcript: only the last N steps (default: all)" },
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": bounds.min.as_secs(),
                    "maximum": bounds.max.as_secs(),
                    "description": format!(
                        "wait: how long before giving up, in seconds ({}–{}, default {}).",
                        bounds.min.as_secs(), bounds.max.as_secs(), bounds.default.as_secs(),
                    ),
                }
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        self.of(args)?.execute(args).await
    }
}

struct AgentControlTool {
    control: Arc<orchestrator::AgentControl>,
    action: AgentAction,
    caller: CallerSlot,
    /// Only `agent.followup` needs these — resuming an agent runs it, so it
    /// goes through the same admission as a spawn.
    executor: ParentHandle,
    max_turns: u32,
}

#[async_trait]
impl Tool for AgentControlTool {
    fn name(&self) -> &str {
        match self.action {
            AgentAction::Followup => "agent.followup",
            AgentAction::Wait => "agent.wait",
            _ => "agent",
        }
    }
    fn icon(&self) -> &'static str {
        "⚇"
    }
    fn description(&self) -> &str {
        match self.action {
            AgentAction::Cancel => {
                "Stop one running agent by its name or session id. Its siblings keep running, and \
                 whatever it had found is still reported."
            }
            AgentAction::List => {
                "List the agents running right now, with their objective and live progress. \
                 `doing` names the current phase; `tool_calls` and `tokens` are running counters. \
                 `quiet_ms` is present only in phases where silence can indicate a stalled \
                 operation, and is null while waiting on the operator or in other exempt phases. \
                 Do not poll or cancel an agent merely because it is quiet."
            }
            AgentAction::Transcript => {
                "Read what an agent actually did, by its session id. A report is a summary; when \
                 one looks thin, wrong, or was cut short, read the work behind it instead of \
                 guessing or re-running the search yourself."
            }
            AgentAction::Steer => {
                "Send further instruction to an agent that is still running — a correction, a \
                 constraint you forgot, or a narrowing of scope. It arrives as a message at the \
                 agent's next step; it does not restart the agent or discard what it has already \
                 found.\n\
                 \n\
                 Use this the moment you realise a running agent is working from something wrong. \
                 The only alternative is cancelling it and paying for the whole run again, and an \
                 agent cannot ask you a question when it gets stuck."
            }
            AgentAction::Message => {
                "Send a note to any live agent in this session — including the one that started \
                 you. It arrives as a message at that agent's next step.\n\
                 \n\
                 This is for passing along something the other agent needs and cannot find on its \
                 own: a finding from your own work, an answer to a question its objective left \
                 open, a constraint that arrived after it started. Unlike `agent.steer`, the \
                 target does not have to be one of your own children."
            }
            AgentAction::Followup => {
                "Give one of your own agents more work, by name or session id.\n\
                 \n\
                 If it is still running the note joins its queue. If it has already finished it is \
                 resumed — same agent, its own prior conversation restored — so it continues from \
                 what it found rather than starting cold. Reach for this instead of spawning a \
                 near-duplicate whenever the new task builds on the old one: a fresh agent pays \
                 again for everything the finished one already learned."
            }
            AgentAction::Wait => {
                "Pause until a running agent finishes, when you genuinely cannot continue without \
                 its answer. Returns as soon as one settles, or empty if the timeout passes first \
                 — so it is bounded, and a timeout is not a failure.\n\
                 \n\
                 Prefer not to. Agents report on their own, and waiting spends the turn doing \
                 nothing. Use it only when the next step depends on the result and there is no \
                 other work to do meanwhile."
            }
        }
    }
    fn blast_radius(&self) -> BlastRadius {
        match self.action {
            // A follow-up starts a session. It restarts a settled agent under the
            // contract it was admitted with — so a writer resumes as a writer,
            // takes a fresh checkout and produces a new patch, at real cost. The
            // rest of these only read, address or stop what already exists.
            //
            // It was declared `Read` alongside them, which meant the one verb here
            // that spends money and can edit code was the one that raised no card.
            AgentAction::Followup => BlastRadius::ReversibleLocal,
            _ => BlastRadius::Read,
        }
    }
    /// Delegation never holds the writer lane; see the note on `agent.spawn`.
    /// A follow-up is admitted exactly as a spawn is, so it inherits that.
    fn mutation_key(&self, _args: &Value) -> Option<String> {
        None
    }
    fn timeout(&self, _args: &Value) -> Option<std::time::Duration> {
        match self.action {
            // The wait *is* the bound, and it is checked against the operator's
            // ceiling before it starts. A tool-level cap on top of it turns a
            // legitimate long wait into an error, which reads as a failed agent
            // rather than as one still working.
            AgentAction::Wait => None,
            // A follow-up runs a whole session, like a spawn; the child's own
            // turn budget and inactivity bound are what limit it.
            AgentAction::Followup => None,
            _ => Some(crate::TOOL_TIMEOUT),
        }
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }
    fn schema(&self) -> Value {
        match self.action {
            AgentAction::List => json!({ "type": "object", "properties": {} }),
            AgentAction::Cancel => json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "Agent name or session id" }
                },
                "required": ["agent"]
            }),
            AgentAction::Transcript => json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "The agent's session id, as returned by agent.spawn" },
                    "tail": { "type": "integer", "description": "Only the last N steps (default: all)" }
                },
                "required": ["agent"]
            }),
            AgentAction::Steer | AgentAction::Message | AgentAction::Followup => json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "Agent path or session id" },
                    "text": { "type": "string", "description": "What to tell it. Stands alone — the agent cannot see this conversation." }
                },
                "required": ["agent", "text"]
            }),
            AgentAction::Wait => {
                let bounds = self.control.wait_bounds();
                json!({
                    "type": "object",
                    "properties": {
                        "timeout_seconds": {
                            "type": "integer",
                            "minimum": bounds.min.as_secs(),
                            "maximum": bounds.max.as_secs(),
                            "description": format!(
                                "How long to wait before giving up, in seconds ({}–{}, default {}).",
                                bounds.min.as_secs(), bounds.max.as_secs(), bounds.default.as_secs(),
                            ),
                        }
                    }
                })
            }
        }
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let from = self.caller.path();
        match self.action {
            AgentAction::List => {
                // What each child is doing, not how long since it last wrote an
                // event: elapsed silence cannot tell a model composing a reply
                // from a connection that died, and "is it moving?" is the only
                // question worth asking about a long-running agent.
                let progress = self.control.progress();
                let agents: Vec<Value> = self
                    .control
                    .active()
                    .into_iter()
                    .filter(|agent| agent.path.under(&from) && agent.path != from)
                    .map(|handle| {
                        let seen = progress.get(&handle.path);
                        let mut row = serde_json::to_value(&handle).unwrap_or_default();
                        if let Some(object) = row.as_object_mut() {
                            object.insert(
                                "doing".into(),
                                json!(seen.map(|p| p.phase.label()).unwrap_or_default()),
                            );
                            object.insert("tool_calls".into(), json!(seen.map(|p| p.tool_calls)));
                            object.insert("tokens".into(), json!(seen.map(|p| p.tokens)));
                            // Only for a phase where silence means something, so
                            // an agent blocked on the operator never reads as
                            // stuck and never invites the caller to cancel it.
                            object.insert(
                                "quiet_ms".into(),
                                json!(seen.and_then(|p| p.stalled_for()).map(|d| d.as_millis())),
                            );
                        }
                        row
                    })
                    .collect();
                Ok(json!({ "agents": agents }))
            }
            AgentAction::Cancel => {
                let id = arg_str(args, "agent")?;
                let stopped = self
                    .control
                    .cancel(&from, &id)
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
                Ok(json!({ "cancelled": stopped }))
            }
            AgentAction::Steer | AgentAction::Message => {
                let id = arg_str(args, "agent")?;
                let text = arg_str(args, "text")?;
                // Reporting success for text nobody received is the worst
                // outcome: the caller carries on believing it corrected a run.
                let delivered = match self.action {
                    AgentAction::Steer => self.control.steer(&from, &id, &text),
                    _ => self.control.message(&from, &id, &text),
                }
                .map_err(|error| ToolError::Failed(error.to_string()))?;
                Ok(json!({ "sent_to": delivered, "delivers": "at the agent's next step" }))
            }
            AgentAction::Followup => {
                let id = arg_str(args, "agent")?;
                let text = arg_str(args, "text")?;
                let Some(parent) = parent_executor(&self.executor) else {
                    return Err(ToolError::Failed(
                        "the agent runtime is not available in this session".into(),
                    ));
                };
                let caller = self.caller.resolve()?;
                let agent = self
                    .control
                    .followup(
                        &caller,
                        &id,
                        &text,
                        parent,
                        child_budget(&self.control, &caller, self.max_turns),
                    )
                    .await
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
                Ok(json!({
                    "agent": agent.path,
                    "session": agent.session,
                    "status": "running",
                    "note": "Picked up where it left off. Its report arrives on its own.",
                }))
            }
            AgentAction::Wait => {
                let requested = args
                    .get("timeout_seconds")
                    .and_then(Value::as_u64)
                    .map(std::time::Duration::from_secs);
                let timeout = self
                    .control
                    .wait_bounds()
                    .resolve(requested)
                    .map_err(|error| ToolError::Args(error.to_string()))?;
                // A timeout is an outcome, not a failure — say so, or the model
                // reads an error and abandons work that is still running fine.
                match self.control.wait(&from, timeout).await {
                    Waited::Settled(settled) => {
                        // Collection normally happens at a turn boundary, so a
                        // mid-turn wait must return the reports directly.
                        let owner = self.caller.resolve()?.session;
                        let collected = self.control.collect(owner).await;
                        // Do not acknowledge here. Tool execution precedes the
                        // durable ToolObs append; a crash or append failure in
                        // that gap would lose reports the caller never received.
                        // The log outbox folds these dispatch ids from the
                        // observation itself, making the append the commit.
                        let acknowledgements: Vec<&str> = collected
                            .iter()
                            .map(|result| result.dispatch.as_str())
                            .collect();
                        // The weakest label across the reports being handed
                        // back, so web-derived findings keep escalating what the
                        // caller does with them.
                        let relayed = orchestrator::least_trusted(
                            collected.iter().map(|result| result.trust),
                        );
                        let reports: Vec<Value> = collected
                            .iter()
                            .map(|result| {
                                json!({
                                    "agent": result.agent,
                                    "session": result.session,
                                    "status": result.status,
                                    "report": result.summary,
                                })
                            })
                            .collect();
                        Ok(json!({
                            "settled": settled,
                            "timed_out": false,
                            "reports": reports,
                            "note": "These are their answers. Do not read their transcripts.",
                            orchestrator::REPORT_ACKS_FIELD: acknowledgements,
                            crate::RELAYED_TRUST: relayed,
                        }))
                    }
                    Waited::TimedOut => Ok(json!({
                        "settled": [],
                        "timed_out": true,
                        "note": "Nothing finished in time. They are still running; their reports still arrive on their own.",
                    })),
                    Waited::Interrupted => Ok(json!({
                        "settled": [],
                        "timed_out": false,
                        "interrupted": true,
                        "note": "Stopped waiting — a new message arrived for you. Read it before \
                                 doing anything else; it may change what you were waiting for. The \
                                 agents are untouched and still running.",
                    })),
                    Waited::Cancelled => Err(ToolError::Failed(
                        "this session is shutting down; the agents were stopped".into(),
                    )),
                }
            }
            AgentAction::Transcript => {
                let id = arg_str(args, "agent")?;
                let mut steps = self
                    .control
                    .transcript(&from, &id)
                    .await
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
                if steps.is_empty() {
                    return Err(ToolError::Failed(format!(
                        "no transcript for '{id}' — pass the session id from agent.spawn, not the name"
                    )));
                }
                let total = steps.len();
                // Bounded by default. A child that ran sixty tool calls produces
                // a step list too large for the context it is being read into —
                // it spills to an artifact, and the caller then pages the
                // artifact, spending several turns reading a log to answer a
                // question the report already answered. The tail is also the
                // part worth having: it is where the answer was forming.
                // Capped, not merely defaulted. The failure this prevents is an
                // oversized payload, and a caller free to ask for more walks
                // straight back into it.
                let cap = self.control.transcript_tail();
                let tail = args
                    .get("tail")
                    .and_then(Value::as_u64)
                    .map_or(cap, |asked| (asked as usize).min(cap));
                if tail < total {
                    steps = steps.split_off(total - tail);
                }
                Ok(json!({
                    "agent": id,
                    "steps": steps,
                    "showing": steps.len(),
                    "total": total,
                    // Raw output from another agent's tools, verbatim. Whatever
                    // it read, the caller is now reading.
                    crate::RELAYED_TRUST: kernel::TrustLabel::Web,
                }))
            }
        }
    }
}

/// Applies a child patch only through a separate human-gated action.
struct AgentApply {
    control: Arc<orchestrator::AgentControl>,
}

#[async_trait]
impl Tool for AgentApply {
    fn name(&self) -> &str {
        "agent.apply"
    }
    fn icon(&self) -> &'static str {
        "⚇"
    }
    fn description(&self) -> &str {
        "Apply a writing agent's patch to the working tree by its exact patch_id. Until you \
         call this, the agent's work exists only as a diff and nothing in the workspace has \
         changed.\n\
         \n\
         A patch whose verification failed is refused: it does not build, so it is a draft, not a \
         fix. Read the failure and fix it, or re-run the agent. `force` overrides that and should \
         be used only once you have established the failure was already there and is unrelated — \
         never as a way past an error you have not read.\n\
         \n\
         If the agent's changes overlap edits made since it started, this reports a conflict and \
         applies nothing — resolve it yourself rather than retrying. With no arguments it lists \
         the patches still waiting."
    }
    fn blast_radius(&self) -> BlastRadius {
        // It rewrites files the user owns. Reversible — the tree is a git repo
        // by construction here — but never something to do unasked.
        BlastRadius::ReversibleLocal
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Vcs
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "patch_id": {
                    "type": "string",
                    "description": "The exact patch_id returned by the listing. Omit to list the patches waiting to be applied."
                },
                "force": {
                    "type": "boolean",
                    "description": "Apply even though verification failed. Only after you have read the failure and established it is pre-existing and unrelated to this patch."
                }
            }
        })
    }
    async fn preview(&self, args: &Value) -> Option<String> {
        let id = args.get("patch_id").and_then(Value::as_str)?;
        let pending = self.control.pending(id).await?;
        if pending.dispatch != id {
            return Some(
                "Refusing an ambiguous patch reference. List pending patches and pass the exact \
                 patch_id."
                    .into(),
            );
        }
        let patch = &pending.patch;
        // The gate shows the diff itself. Approving "apply agent-3's patch"
        // without seeing what it does is not a decision, it is a formality.
        let files = patch.files.join(", ");
        let evidence = match &patch.verification {
            Some(v) if v.passed => format!("verified: `{}` passed", v.command),
            Some(v) => format!("NOT VERIFIED: `{}` failed", v.command),
            None => "NOT VERIFIED: nothing was run against this patch".to_string(),
        };
        let body: String = patch.diff.lines().take(200).collect::<Vec<_>>().join("\n");
        let elided = patch.diff.lines().count().saturating_sub(200);
        Some(format!(
            "apply {id}'s patch to {files}\n{evidence}\n\n{body}{}",
            match elided {
                0 => String::new(),
                n => format!("\n… {n} more line(s)"),
            }
        ))
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let Some(id) = args.get("patch_id").and_then(Value::as_str) else {
            let waiting: Vec<Value> = self
                .control
                .outstanding()
                .await
                .into_iter()
                .map(|pending| {
                    json!({
                        "agent": pending.agent,
                        "session": pending.session,
                        // The exact handout. A session can hold several once it
                        // has been followed up, so this is what addresses one.
                        "patch_id": pending.dispatch,
                        "files": pending.patch.files,
                        "verified": pending.patch.verified(),
                    })
                })
                .collect();
            return Ok(json!({ "unmerged": waiting, "count": waiting.len() }));
        };
        let pending = self.control.pending(id).await.ok_or_else(|| {
            ToolError::Failed(format!(
                "no patch from '{id}' — it may have changed nothing, or been a read-only agent"
            ))
        })?;
        if pending.dispatch != id {
            return Err(ToolError::Failed(format!(
                "'{id}' is not an exact patch_id, so nothing was applied. Call agent.apply with \
                 no arguments, then pass the patch_id it lists."
            )));
        }
        let patch = &pending.patch;
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        match self.control.merge(patch, force).await {
            Ok(check) => {
                // Closed by handout, not by session: a follow-up reuses the
                // session, and closing by it would mark a diff nobody applied
                // as applied. Leaving it open instead would invite a second
                // apply, which either fails confusingly or double-applies.
                self.control.forget(&pending.dispatch).await;
                Ok(json!({
                    "applied": true,
                    // Carries verifier output, which is whatever running the
                    // agent's own edits printed — never the operator speaking.
                    crate::RELAYED_TRUST: kernel::TrustLabel::Tool,
                    "files": patch.files,
                    "merge": check,
                    "verified": patch.verified(),
                    "forced": force,
                }))
            }
            Err(orchestrator::Error::Unverified(command)) => Err(ToolError::Failed(format!(
                "{} — nothing was applied.\n\n{}",
                orchestrator::Error::Unverified(command),
                patch
                    .verification
                    .as_ref()
                    .map(|evidence| evidence.output.clone())
                    .unwrap_or_default()
            ))),
            Err(error) => Err(ToolError::Failed(format!(
                "{error} — nothing was applied. The files {} changed since this agent started; \
                 read the patch and make the edits yourself, or re-run the agent from the \
                 current state.",
                patch.files.join(", ")
            ))),
        }
    }
}

#[cfg(test)]
mod argument_tests {
    use super::*;

    #[test]
    fn malformed_tool_narrowing_never_becomes_inherit_all() {
        assert!(matches!(
            optional_string_list(&json!({ "tools": "fs.read" }), "tools"),
            Err(ToolError::Args(message)) if message.contains("expected array")
        ));
        assert!(matches!(
            optional_string_list(&json!({ "tools": ["fs.read", 7] }), "tools"),
            Err(ToolError::Args(message)) if message.contains("tools[1]")
        ));
        assert_eq!(
            optional_string_list(&json!({ "tools": ["fs.read"] }), "tools").unwrap(),
            Some(vec!["fs.read".into()])
        );
        assert_eq!(optional_string_list(&json!({}), "tools").unwrap(), None);
    }
}
