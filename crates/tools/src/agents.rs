//! The delegation tools: `agent.spawn`, the control verbs, and `agent.apply`.
//!
//! Every one of them is bound to the [`Caller`] that holds it. A tool shared
//! with the whole tree would resolve `parser` from the root no matter who
//! called it, which is how a depth limit comes to read the root's depth and a
//! child comes to cancel its own siblings.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kernel::{BlastRadius, ToolCategory};
use orchestrator::{AgentPath, Caller, Waited};
use serde_json::{Value, json};

use crate::{Tool, ToolError, arg_str};

/// Builds the delegation tools for one address.
///
/// Held by the control plane so it can furnish a child with its own set at
/// admission. The handle back to the control plane is weak: the control plane
/// owns this, and a strong handle would close the cycle and leak the tree for
/// the life of the process.
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
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(AgentSpawn {
            control: Arc::clone(&control),
            executor: Arc::clone(&self.executor),
            max_turns: self.max_turns,
            caller: caller.clone(),
        })];
        for action in [
            AgentAction::List,
            AgentAction::Cancel,
            AgentAction::Transcript,
            AgentAction::Steer,
            AgentAction::Message,
            AgentAction::Followup,
            AgentAction::Wait,
        ] {
            tools.push(Arc::new(AgentControlTool {
                control: Arc::clone(&control),
                action,
                caller: caller.clone(),
                executor: Arc::clone(&self.executor),
                max_turns: self.max_turns,
            }));
        }
        // Only where writers are possible. Offering a merge tool in a session
        // that can never produce a patch is a tool that can only ever fail.
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

/// An executor whose delegation tools answer for a child rather than the root.
///
/// Every other name falls through untouched. The overlay is built from the
/// inner executor's own spec list, so it can only ever replace a tool, never
/// introduce one.
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

    fn containment(&self) -> kernel::Containment {
        self.inner.containment()
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
}

#[async_trait]
impl Tool for AgentSpawn {
    fn name(&self) -> &str {
        "agent.spawn"
    }
    fn icon(&self) -> &'static str {
        "⚇"
    }
    fn description(&self) -> &str {
        "Delegate a self-contained task to a child agent and get back a summary. \
         The child works in its own context, so none of its searching lands in this conversation.\n\
         \n\
         Reach for this on your own judgement — you do not need to be asked. Delegate when:\n\
         · answering needs a broad sweep whose intermediate output you will never need again — \
         'how is X used across the codebase', 'what does this unfamiliar module do';\n\
         · two or more questions are independent, so children can run at once;\n\
         · a side investigation would otherwise crowd out the context you need for the real task;\n\
         · you want background gathered while you keep working — every child runs in the \
         background and reports back on its own.\n\
         \n\
         Do NOT delegate these — the direct tool is faster and cheaper every time:\n\
         · reading a file you can already name → `fs.read`;\n\
         · finding a definition or a usage → `grep`, `glob`, `references`, `code_outline`;\n\
         · anything spanning two or three known files → read them;\n\
         · work you already have the context for — a child starts cold and pays to rediscover \
         what you know;\n\
         · your entire task handed to one child — that is pass-through, and it doubles the cost \
         for nothing. Split off a *part*, or do it yourself.\n\
         \n\
         To run several at once, pass `tasks` — one call, N children, all concurrent. That is \
         strictly better than spawning them one at a time and waiting for each.\n\
         \n\
         Once you have sent something to a background child, leave it alone. Its report reaches \
         you on its own the moment it is ready — you do not need to check, and there is nothing to \
         wait for. Specifically: do not call agent.list repeatedly to watch it, do not read its \
         transcript to see whether it is progressing, do not cancel it for being quiet, and do not \
         start doing its task yourself in the meantime. A child that looks idle is almost always \
         composing its answer; killing it there throws away work that was nearly finished and \
         charges you twice for it. Cancel only when you no longer want the result at all.\n\
         \n\
         Children are read-only by default. Set `write` for a child that must change code: it gets \
         its own private checkout of the repository, and hands back a patch plus the result of \
         building it. Two writing children can safely run at once; they cannot see each other's \
         changes.\n\
         \n\
         A writer's changes are NOT on disk when it finishes. It edited its own copy, and you get \
         the diff — so the real file still reads exactly as it did before, and checking it proves \
         nothing about whether the agent worked. That is the design, not a failure: review the \
         diff, then call `agent.apply` to land it, which asks the user first. If a writer says it \
         made changes and the file looks untouched, you are looking at the wrong place — apply \
         the patch rather than redoing the work.\n\
         \n\
         State the objective in full: the child cannot see this conversation and cannot ask you \
         anything — put every path, error message and constraint it needs in the objective itself. \
         If the user asked for a particular language, tone or format, say so there too, or the \
         child's summary will come back in the wrong one and contaminate your reply. Give \
         `contract` when the answer must have a particular shape. A child can never use a tool you \
         do not already have. It can message you and any other running agent, but it can only stop \
         or steer the agents it started itself. A read-only child cannot delegate further; a \
         writing one can.\n\
         \n\
         A child's report is its own account of what it did, not an established fact. For anything \
         with an effect outside its own reasoning — a file written, a request sent, a test claimed \
         to pass — get the verifiable handle (path, URL, status, command output) and check it \
         yourself before you tell the user it happened. Its report comes back to you, not to the \
         user — relay what matters."
    }
    fn blast_radius(&self) -> BlastRadius {
        // Read-only children: no mutation, but real model spend, so it stays
        // above a plain read.
        BlastRadius::ReversibleLocal
    }
    fn timeout(&self) -> Option<std::time::Duration> {
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
                "name": { "type": "string", "description": "Short label for the agent (optional)" },
                "contract": {
                    "type": "string",
                    "description": "What the result must contain, e.g. 'a list of file:line with one sentence each'"
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Narrow the child to these tools. Omit to inherit yours. Cannot exceed yours."
                },
                "max_turns": { "type": "integer", "description": "Turn ceiling, clamped to what remains" },
                "write": {
                    "type": "boolean",
                    "description": "REQUIRED for any task that changes something — write, edit, add, fix, rename, create a file. Without it the child gets read-only tools, cannot edit anything, and will come back describing the change it would have made instead of making it. It works in a private checkout and returns a patch, so your files are never touched until you apply it with `agent.apply`. Refused if this workspace is not a git repository."
                },
                "fork": {
                    "type": "string",
                    "description": "How much of this conversation the child inherits: 'all' (default), 'none' for a cold start, or a number of recent turns. Lower it when the task is self-contained and the history is long; 'none' when the objective says everything."
                },
                "tasks": {
                    "type": "array",
                    "description": "Run several independent investigations at once, each its own agent, and get every report back together. Use this instead of one call per question — they run concurrently rather than in sequence. Give `tasks` OR `objective`, not both.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "objective": { "type": "string" },
                            "name": { "type": "string" },
                            "contract": { "type": "string" },
                            "tools": { "type": "array", "items": { "type": "string" } },
                            "write": { "type": "boolean", "description": "REQUIRED if this task changes anything; without it the child is read-only and cannot edit." },
                            "fork": { "type": "string", "description": "How much of this conversation this child inherits: 'all' (default), 'none', or a number of turns." }
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
        // Batch: independent questions run at once rather than one call after
        // another. Capacity is already bounded per tree, so an over-large batch
        // is refused by the runtime rather than flooding it.
        if let Some(tasks) = args.get("tasks").and_then(Value::as_array) {
            if tasks.is_empty() {
                return Err(ToolError::Args("tasks is empty".into()));
            }
            let Some(parent) = parent_executor(&self.executor) else {
                return Err(ToolError::Failed(
                    "the agent runtime is not available in this session".into(),
                ));
            };
            let caller = self.caller.resolve()?;
            let mut started = Vec::with_capacity(tasks.len());
            for task in tasks {
                let spec = orchestrator::AgentSpec {
                    name: task
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    objective: task
                        .get("objective")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    contract: task
                        .get("contract")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    tools: task.get("tools").and_then(Value::as_array).map(|names| {
                        names
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    }),
                    max_turns: None,
                    write: task.get("write").and_then(Value::as_bool).unwrap_or(false),
                    fork: parse_fork(task)?,
                };
                // One refusal must not discard its siblings, so each is
                // reported on its own terms.
                started.push(
                    match self
                        .control
                        .spawn_background(spec, &caller, parent.clone(), self.max_turns)
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
            return Ok(json!({
                "agents": started,
                "count": started.len(),
                "note": "All started. Their reports arrive on their own — do not poll or wait \
                         unless you cannot continue without them, in which case call agent.wait.",
            }));
        }
        let objective = arg_str(args, "objective")?;
        let Some(parent) = parent_executor(&self.executor) else {
            return Err(ToolError::Failed(
                "the agent runtime is not available in this session".into(),
            ));
        };
        let spec = orchestrator::AgentSpec {
            name: args
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            objective,
            contract: args
                .get("contract")
                .and_then(Value::as_str)
                .map(str::to_string),
            tools: args.get("tools").and_then(Value::as_array).map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            }),
            max_turns: args
                .get("max_turns")
                .and_then(Value::as_u64)
                .map(|turns| turns as u32),
            write: args.get("write").and_then(Value::as_bool).unwrap_or(false),
            fork: parse_fork(args)?,
        };
        // Every child runs asynchronously. Waiting inside the call blocked the
        // turn for as long as the child ran, with no timeout and no way out but
        // an interrupt that discarded the work — and bought nothing, since the
        // report arrives on its own either way.
        let caller = self.caller.resolve()?;
        let agent = self
            .control
            .spawn_background(spec, &caller, parent, self.max_turns)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        Ok(json!({
            "agent": agent.path,
            "session": agent.session,
            "status": "running",
            "note": "Started. Its report arrives on its own when ready — do not poll, do not read \
                     its transcript to check progress, and do not start doing its work. If you \
                     need the result before continuing, call agent.wait.",
        }))
    }
}

/// Shared slot for the session a background report belongs to.
pub type SessionHandle = Arc<Mutex<Option<ulid::Ulid>>>;

/// Shared slot for the executor a child inherits from.
///
/// Weak, and it has to be: that executor is the registry which owns the tool
/// holding this slot. A strong handle closes the loop, and nothing in the cycle
/// is ever dropped — the registry, every tool in it, the control plane and each
/// child's worktree lease all outlive the session that made them, for the life
/// of the process.
pub type ParentHandle = Arc<Mutex<Option<std::sync::Weak<dyn kernel::Executor>>>>;

/// The executor a child narrows from, if the session that owns it is still up.
fn parent_executor(slot: &ParentHandle) -> Option<Arc<dyn kernel::Executor>> {
    slot.lock().ok()?.as_ref()?.upgrade()
}

/// How much conversation a child inherits. Absent means `all`, matching the
/// reading that a child asked to help with *this* work should know about it.
fn parse_fork(args: &Value) -> Result<orchestrator::Fork, ToolError> {
    match args.get("fork").and_then(Value::as_str) {
        None => Ok(orchestrator::Fork::default()),
        Some(text) => orchestrator::Fork::parse(text).map_err(ToolError::Args),
    }
}

/// The address a delegation tool acts from.
///
/// The root's session id does not exist when its tools are built, so it arrives
/// through a slot; a child's is known at the moment it is admitted, and fixing
/// it there is what stops a nested report being posted to the root's chain.
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

/// Inspect and stop running agents. Read-only listing plus a targeted stop, so
/// the model can abandon work it no longer needs rather than paying for it.
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
            AgentAction::List => "agent.list",
            AgentAction::Cancel => "agent.cancel",
            AgentAction::Transcript => "agent.transcript",
            AgentAction::Steer => "agent.steer",
            AgentAction::Message => "agent.message",
            AgentAction::Followup => "agent.followup",
            AgentAction::Wait => "agent.wait",
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
                "List the agents running right now, with what each was asked to do.\n\
                 \n\
                 `idle_ms` is time since the agent last *recorded* a step, and a model writes \
                 nothing while it is composing a long answer — so a large value usually means it \
                 is mid-generation, not stuck. Minutes of silence on a big model is ordinary. Do \
                 not treat this as a fault signal, do not poll it, and do not cancel an agent \
                 because it is quiet: you would be throwing away work that was nearly done and \
                 paying for it twice."
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
        BlastRadius::Read
    }
    fn timeout(&self) -> Option<std::time::Duration> {
        match self.action {
            // The wait *is* the bound, and it is checked against the operator's
            // ceiling before it starts. A tool-level cap on top of it turns a
            // legitimate long wait into an error, which reads as a failed agent
            // rather than as one still working.
            AgentAction::Wait => None,
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
                // Elapsed-since-start cannot answer "is it moving?", which is
                // the only question worth asking about a long-running agent.
                let idle = self.control.idle_times().await;
                let agents: Vec<Value> = self
                    .control
                    .active()
                    .into_iter()
                    .filter(|agent| agent.path.under(&from) && agent.path != from)
                    .map(|handle| {
                        let quiet = idle.get(&handle.session).copied().flatten();
                        let mut row = serde_json::to_value(&handle).unwrap_or_default();
                        if let Some(object) = row.as_object_mut() {
                            object.insert("idle_ms".into(), json!(quiet));
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
                    .followup(&caller, &id, &text, parent, self.max_turns)
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
                    Waited::Settled(settled) => Ok(json!({
                        "settled": settled,
                        "timed_out": false,
                        "note": "Their reports arrive with the rest of this turn's results.",
                    })),
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
                // A child can run for dozens of turns; the tail is usually where
                // the answer was forming when it stopped.
                if let Some(tail) = args.get("tail").and_then(Value::as_u64)
                    && (tail as usize) < total
                {
                    steps = steps.split_off(total - tail as usize);
                }
                Ok(json!({ "agent": id, "steps": steps, "total": total }))
            }
        }
    }
}

/// Merge a writing child's patch into the user's working tree (§6.4).
///
/// Separate from `agent.spawn` on purpose. A child finishing is not consent to
/// change the user's files, so the patch waits until someone asks for it — and
/// because this is a consequential action, the ask goes through the human gate
/// with the diff on screen.
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
        "Apply a writing agent's patch to the working tree, by its session id or name. Until you \
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
                "agent": {
                    "type": "string",
                    "description": "The writing agent's session id (or name). Omit to list the patches waiting to be applied."
                },
                "force": {
                    "type": "boolean",
                    "description": "Apply even though verification failed. Only after you have read the failure and established it is pre-existing and unrelated to this patch."
                }
            }
        })
    }
    async fn preview(&self, args: &Value) -> Option<String> {
        let id = args.get("agent").and_then(Value::as_str)?;
        let patch = self.control.patch(id).await?;
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
        let Some(id) = args.get("agent").and_then(Value::as_str) else {
            let waiting: Vec<Value> = self
                .control
                .outstanding()
                .await
                .into_iter()
                .map(|(agent, session, patch)| {
                    json!({
                        "agent": agent,
                        "session": session,
                        "files": patch.files,
                        "verified": patch.verified(),
                    })
                })
                .collect();
            return Ok(json!({ "unmerged": waiting, "count": waiting.len() }));
        };
        let patch = self.control.patch(id).await.ok_or_else(|| {
            ToolError::Failed(format!(
                "no patch from '{id}' — it may have changed nothing, or been a read-only agent"
            ))
        })?;
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        match self.control.merge(&patch, force).await {
            Ok(check) => {
                // Applied, so it is no longer outstanding. Leaving it listed
                // would invite a second apply, which would either fail
                // confusingly or double-apply.
                self.control.forget(id).await;
                Ok(json!({
                    "applied": true,
                    "files": patch.files,
                    "merge": check,
                    "verified": patch.verified(),
                    "forced": force,
                }))
            }
            // A patch that does not build does not merge (§6.4). The failure
            // output travels with the refusal, so the next step is reading it
            // rather than guessing or reaching for `force`.
            Err(orchestrator::Error::Unverified(command)) => Err(ToolError::Failed(format!(
                "{} — nothing was applied.\n\n{}",
                orchestrator::Error::Unverified(command),
                patch
                    .verification
                    .as_ref()
                    .map(|evidence| evidence.output.clone())
                    .unwrap_or_default()
            ))),
            // §6.4: conflicting patches go to reconciliation, never
            // last-writer-wins. Nothing was applied, and saying so precisely is
            // what stops the model from "fixing" it by force.
            Err(error) => Err(ToolError::Failed(format!(
                "{error} — nothing was applied. The files {} changed since this agent started; \
                 read the patch and make the edits yourself, or re-run the agent from the \
                 current state.",
                patch.files.join(", ")
            ))),
        }
    }
}
