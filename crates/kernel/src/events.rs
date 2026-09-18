//! Append-only event log and source of truth.
//! State is a projection of the log; the kernel is the only writer.

use crate::errors::KernelError;
use crate::types::{
    ContentPart, Decision, Message, ModelMessage, Observation, Session, TextPart, ToolCallPart,
    ToolIntent, ToolResultPart, TrustLabel,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use ulid::Ulid;

/// Current canonical event-chain encoding. Version 1 was the original
/// `(prev, kind, session, payload, ts)` encoding; version 2 authenticates every
/// persisted event field with unambiguous length framing.
pub const EVENT_HASH_VERSION: u8 = 2;

/// Durable-only acknowledgement metadata used by the sub-agent outbox. It is
/// retained in the event log, but must never be shown to a model or UI.
pub const AGENT_REPORT_ACKS_FIELD: &str = "_agent_report_dispatches";

pub(crate) fn strip_private_observation_fields(payload: &mut Value) {
    if let Some(object) = payload.as_object_mut() {
        object.remove(AGENT_REPORT_ACKS_FIELD);
    }
}

/// Images a tool produced ride in their own user turn: every protocol Medha
/// speaks accepts text-only tool messages, so this is the one place an image
/// from a tool can legally sit. The call id ties it back to its result and is
/// the one identifier both a live turn and a replayed one have, so the two
/// build identical context.
pub fn tool_media_message(call_id: &str, media: Vec<crate::types::MediaPart>) -> crate::Message {
    let mut message = crate::Message::user(format!(
        "[{} image(s) returned by tool call {call_id}]",
        media.len()
    ));
    message.attachments = media;
    message
}

/// Rebuild that carrier when replaying a logged observation, so a resumed
/// session sees the same images the live turn did.
fn replayed_tool_media(payload: &Value, trust: TrustLabel) -> Option<crate::Message> {
    let media: Vec<crate::types::MediaPart> =
        serde_json::from_value(payload.get("media")?.clone()).ok()?;
    if media.is_empty() {
        return None;
    }
    let call_id = payload
        .get("intent_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(tool_media_message(call_id, media).carrying(trust))
}

fn visible_observation_payload(event_payload: &Value) -> Value {
    let mut payload = event_payload.get("payload").cloned().unwrap_or(Value::Null);
    strip_private_observation_fields(&mut payload);
    payload
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    UserMessage,
    ModelText,
    ModelIntent,
    /// One complete ordered canonical assistant message, including opaque
    /// protocol-tagged replay state. Native adapters use this instead of
    /// separate flat text/intent events.
    ModelMessage,
    ToolObs,
    PolicyDecision,
    /// Write-ahead marker emitted between authorization and a state-changing call,
    /// so a crash still leaves replay knowing the effect may have committed.
    ToolEffectPrepared,
    Compaction,
    Session,
    /// Reasoning retained for audit but excluded from model history.
    ModelReasoning,
    /// A processed interrupt; steer text also logs as `user.message`.
    Interrupt,
    /// A memory write/update/forget/pin. Payload = the `memory` crate's
    /// `MemoryOp`, kept as opaque JSON here — the kernel doesn't parse it.
    MemoryWrite,
    ContextFileLoaded,
    ContextFileBlocked,
    /// A sub-agent lifecycle event on the parent's chain.
    AgentSpawned,
    AgentCompleted,
    AgentFailed,
    AgentCancelled,
    /// A delivered background-agent report.
    AgentDelivered,
    /// A writing agent's durable patch.
    AgentPatch,
    /// A patch merged into the working tree.
    AgentApplied,
}

impl EventKind {
    /// Stable wire/storage string (used by persistence backends).
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::UserMessage => "user.message",
            EventKind::ModelText => "model.text",
            EventKind::ModelIntent => "model.tool_intent",
            EventKind::ModelMessage => "model.message",
            EventKind::ToolObs => "tool.observation",
            EventKind::PolicyDecision => "policy.decision",
            EventKind::ToolEffectPrepared => "tool.effect_prepared",
            EventKind::Compaction => "context.compaction",
            EventKind::Session => "session",
            EventKind::ModelReasoning => "model.reasoning",
            EventKind::Interrupt => "interrupt",
            EventKind::MemoryWrite => "memory.write",
            EventKind::ContextFileLoaded => "context.file_loaded",
            EventKind::ContextFileBlocked => "context.file_blocked",
            EventKind::AgentSpawned => "agent.spawned",
            EventKind::AgentCompleted => "agent.completed",
            EventKind::AgentFailed => "agent.failed",
            EventKind::AgentCancelled => "agent.cancelled",
            EventKind::AgentDelivered => "agent.delivered",
            EventKind::AgentPatch => "agent.patch",
            EventKind::AgentApplied => "agent.applied",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user.message" => EventKind::UserMessage,
            "model.text" => EventKind::ModelText,
            "model.tool_intent" => EventKind::ModelIntent,
            "model.message" => EventKind::ModelMessage,
            "tool.observation" => EventKind::ToolObs,
            "policy.decision" => EventKind::PolicyDecision,
            "tool.effect_prepared" => EventKind::ToolEffectPrepared,
            "context.compaction" => EventKind::Compaction,
            "session" => EventKind::Session,
            "model.reasoning" => EventKind::ModelReasoning,
            "interrupt" => EventKind::Interrupt,
            "memory.write" => EventKind::MemoryWrite,
            "context.file_loaded" => EventKind::ContextFileLoaded,
            "context.file_blocked" => EventKind::ContextFileBlocked,
            "agent.spawned" => EventKind::AgentSpawned,
            "agent.completed" => EventKind::AgentCompleted,
            "agent.failed" => EventKind::AgentFailed,
            "agent.cancelled" => EventKind::AgentCancelled,
            "agent.delivered" => EventKind::AgentDelivered,
            "agent.patch" => EventKind::AgentPatch,
            "agent.applied" => EventKind::AgentApplied,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Provenance {
    pub source: String,
}

/// One typed, hash-chained record. `prev_hash` makes the log tamper-evident.
#[derive(Clone)]
pub struct Event {
    pub id: Ulid,
    pub session_id: Ulid,
    pub parent_id: Option<Ulid>,
    pub kind: EventKind,
    pub payload: Value,
    pub trust: TrustLabel,
    pub provenance: Provenance,
    pub prev_hash: [u8; 32],
    pub hash_version: u8,
    pub ts: f64,
}

impl std::fmt::Debug for Event {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("Event");
        debug
            .field("id", &self.id)
            .field("session_id", &self.session_id)
            .field("parent_id", &self.parent_id)
            .field("kind", &self.kind);
        if self.kind == EventKind::ModelMessage {
            debug.field("payload", &"<redacted ordered model message>");
        } else if self.kind == EventKind::Compaction && self.payload.get("snapshot").is_some() {
            // A canonical snapshot can contain opaque provider replay state.
            // Treat it like `model.message`: it is durable, but never diagnostic
            // output.
            debug.field("payload", &"<redacted compacted context snapshot>");
        } else {
            debug.field("payload", &self.payload);
        }
        debug
            .field("trust", &self.trust)
            .field("provenance", &self.provenance)
            .field("prev_hash", &self.prev_hash)
            .field("hash_version", &self.hash_version)
            .field("ts", &self.ts)
            .finish()
    }
}

impl Event {
    fn new(s: &Session, kind: EventKind, payload: Value, trust: TrustLabel) -> Self {
        Self {
            id: Ulid::new(),
            session_id: s.id,
            parent_id: None,
            kind,
            payload,
            trust,
            provenance: Provenance {
                source: "kernel".into(),
            },
            prev_hash: [0u8; 32],
            hash_version: EVENT_HASH_VERSION,
            ts: now_ts(),
        }
    }

    /// The user's message (or steering injection). Logged so the conversation
    /// is fully reconstructable from the log — without it, resume/replay would
    /// have the model's answers but not the prompts that produced them.
    pub fn user_message(s: &Session, text: &str) -> Self {
        Self::user_input(s, text, TrustLabel::User)
    }

    /// A message on the user's channel, labelled with where the content actually
    /// came from. Recording a sub-agent's report as `User` lost its taint on resume.
    pub fn user_input(s: &Session, text: &str, trust: TrustLabel) -> Self {
        Self::new(s, EventKind::UserMessage, json!({ "text": text }), trust)
    }

    pub fn user_input_message(s: &Session, message: &Message) -> Self {
        let mut event = Self::user_input(
            s,
            &message.content,
            message.trust.unwrap_or(TrustLabel::User),
        );
        if !message.attachments.is_empty() {
            event.payload["attachments"] = serde_json::json!(message.attachments);
        }
        event
    }

    /// An explicit retry of an already-admitted user-channel event. Coalesced on
    /// replay only when `retry_of` matches; repeated text is never guessed.
    pub fn user_input_retry(s: &Session, text: &str, trust: TrustLabel, retry_of: Ulid) -> Self {
        Self::new(
            s,
            EventKind::UserMessage,
            json!({ "text": text, "retry_of": retry_of.to_string() }),
            trust,
        )
    }

    /// A sub-agent starting, on the child's own chain so its session is
    /// self-describing. The parent's `agent.spawn` intent links the other way.
    pub fn agent_spawned(s: &Session, name: &str, objective: &str, tools: &[String]) -> Self {
        Self::new(
            s,
            EventKind::AgentSpawned,
            json!({ "agent": name, "objective": objective, "tools": tools }),
            // The objective is authored by the model, not the user.
            TrustLabel::System,
        )
    }

    /// A background agent dispatched, on the dispatching session's chain. The
    /// outbox row: present without a terminal event means an orphaned child.
    /// `dispatch` is the handout, `child` the session serving it — a follow-up
    /// reuses the session, so folding on `child` would over-close. `instance`
    /// names the owning process lease, which recovery must prove released.
    pub fn agent_dispatched(
        s: &Session,
        dispatch: Ulid,
        name: &str,
        child: Ulid,
        objective: &str,
        instance: Ulid,
    ) -> Self {
        Self::new(
            s,
            EventKind::AgentSpawned,
            json!({
                "dispatch": dispatch.to_string(),
                "agent": name,
                "child": child.to_string(),
                "objective": objective,
                "instance": instance.to_string(),
            }),
            TrustLabel::System,
        )
    }

    /// A background agent's report, against its dispatch. `trust` is the weakest
    /// label the child touched, so the record says what the report is worth.
    pub fn agent_report(
        s: &Session,
        kind: EventKind,
        dispatch: Ulid,
        child: Ulid,
        mut payload: Value,
        trust: TrustLabel,
    ) -> Self {
        if let Some(object) = payload.as_object_mut() {
            object.insert("dispatch".into(), Value::String(dispatch.to_string()));
            object.insert("child".into(), Value::String(child.to_string()));
        }
        Self::new(s, kind, payload, trust)
    }

    /// A report handed to its owner. Without this the fold would re-deliver on
    /// every replay.
    pub fn agent_delivered(s: &Session, dispatch: Ulid) -> Self {
        Self::new(
            s,
            EventKind::AgentDelivered,
            json!({ "dispatch": dispatch.to_string() }),
            TrustLabel::System,
        )
    }

    /// A writing agent's patch on the dispatching session's chain. The worktree is
    /// reaped once the diff is taken, so this record is the work. Labelled with the
    /// parent's trust — git generated it, and the child's trust rides its report.
    pub fn agent_patch(s: &Session, dispatch: Ulid, name: &str, child: Ulid, patch: Value) -> Self {
        Self::new(
            s,
            EventKind::AgentPatch,
            json!({
                "dispatch": dispatch.to_string(),
                "agent": name,
                "child": child.to_string(),
                "patch": patch,
            }),
            TrustLabel::System,
        )
    }

    /// A patch merged into the working tree. Without this the fold would keep
    /// offering applied work as outstanding after a restart.
    pub fn agent_applied(s: &Session, dispatch: Ulid) -> Self {
        Self::new(
            s,
            EventKind::AgentApplied,
            json!({ "dispatch": dispatch.to_string() }),
            TrustLabel::System,
        )
    }

    /// A sub-agent reaching a terminal state. `trust` is the weakest label the
    /// child touched, so the audit trail records what its report is worth.
    pub fn agent_finished(
        s: &Session,
        kind: EventKind,
        name: &str,
        detail: &str,
        trust: TrustLabel,
    ) -> Self {
        Self::new(s, kind, json!({ "agent": name, "detail": detail }), trust)
    }

    pub fn model_text(s: &Session, text: &str) -> Self {
        Self::new(
            s,
            EventKind::ModelText,
            json!({ "text": text }),
            TrustLabel::System,
        )
    }

    pub fn model_reasoning(s: &Session, text: &str) -> Self {
        Self::new(
            s,
            EventKind::ModelReasoning,
            json!({ "text": text }),
            TrustLabel::System,
        )
    }

    pub fn model_intent(s: &Session, it: &ToolIntent) -> Self {
        Self::new(
            s,
            EventKind::ModelIntent,
            json!({ "id": it.id, "tool": it.tool, "args": it.args }),
            TrustLabel::System,
        )
    }

    pub fn model_message(s: &Session, message: &ModelMessage) -> Self {
        let payload = serde_json::to_value(message).unwrap_or(Value::Null);
        Self::new(s, EventKind::ModelMessage, payload, TrustLabel::System)
    }

    /// A tool observation, tagged with the provenance of its content. Local
    /// tools pass `TrustLabel::Tool`; web-facing tools pass `TrustLabel::Web`
    /// so downstream layers can treat fetched content as untrusted.
    pub fn tool_obs(s: &Session, o: &Observation, trust: TrustLabel) -> Self {
        let payload = serde_json::to_value(o).unwrap_or(Value::Null);
        Self::new(s, EventKind::ToolObs, payload, trust)
    }

    /// A memory mutation. `op` is the memory crate's `MemoryOp` JSON —
    /// opaque here; the projection rebuilds from it.
    pub fn memory_write(s: &Session, op: Value) -> Self {
        Self::new(s, EventKind::MemoryWrite, op, TrustLabel::Memory)
    }

    pub fn context_file(
        s: &Session,
        path: &str,
        content: &str,
        blocked: bool,
        trust: TrustLabel,
    ) -> Self {
        Self::new(
            s,
            if blocked {
                EventKind::ContextFileBlocked
            } else {
                EventKind::ContextFileLoaded
            },
            json!({ "path": path, "content": content }),
            trust,
        )
    }

    pub fn policy(s: &Session, intent: &ToolIntent, decision: &Decision) -> Self {
        let (verdict, reason) = match decision {
            Decision::Allow => ("allow", String::new()),
            Decision::Deny { reason } => ("deny", reason.clone()),
            Decision::Human => ("human", String::new()),
        };
        Self::new(
            s,
            EventKind::PolicyDecision,
            json!({ "tool": intent.tool, "intent_id": intent.id, "decision": verdict, "reason": reason }),
            TrustLabel::System,
        )
    }

    pub fn tool_effect_prepared(s: &Session, intent: &ToolIntent, mutation_key: &str) -> Self {
        Self::new(
            s,
            EventKind::ToolEffectPrepared,
            json!({
                "intent_id": intent.id,
                "tool": intent.tool,
                "args": intent.args,
                "mutation_key": mutation_key,
            }),
            TrustLabel::System,
        )
    }

    /// A processed interrupt: `kind` = "steer" | "cancel", `text` for steers.
    pub fn interrupt(s: &Session, kind: &str, text: Option<&str>) -> Self {
        Self::new(
            s,
            EventKind::Interrupt,
            json!({ "kind": kind, "text": text }),
            TrustLabel::System,
        )
    }

    pub fn compaction(
        s: &Session,
        before_tokens: u32,
        after_tokens: u32,
        summary: Option<&str>,
    ) -> Self {
        Self::new(
            s,
            EventKind::Compaction,
            json!({ "before_tokens": before_tokens, "after_tokens": after_tokens, "summary": summary }),
            TrustLabel::System,
        )
    }

    /// Persist both post-compaction request views — `messages` for the context
    /// engine, `ordered` for the provider including opaque state. Keeping both
    /// makes the event a real checkpoint rather than a lossy derivation.
    pub fn compaction_snapshot(
        s: &Session,
        before_tokens: u32,
        after_tokens: u32,
        summary: Option<&str>,
        messages: &[Message],
        ordered: &[ModelMessage],
    ) -> Self {
        Self::new(
            s,
            EventKind::Compaction,
            json!({
                "before_tokens": before_tokens,
                "after_tokens": after_tokens,
                "summary": summary,
                "snapshot": {
                    "version": 1,
                    "messages": messages,
                    "ordered": ordered,
                },
            }),
            TrustLabel::System,
        )
    }
}

/// Summary of one session, for a resume picker / `--sessions` list. Lives here
/// (not in the store) so it can be returned through the `EventLog` trait and any
/// surface — TUI, REPL, CLI — can browse sessions generically.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: Ulid,
    pub title: String,
    pub started_ts: f64,
    pub last_ts: f64,
    pub events: u64,
}

/// An owned, RAII mutation lease. The kernel knows nothing about how a backend
/// coordinates writers — in-memory logs return an empty lease, persistent ones
/// keep a lock alive here across the side effect and the events recording it.
#[must_use = "dropping a mutation lease allows another state change to begin"]
pub struct MutationLease {
    _guard: Option<Box<dyn MutationLeaseGuard>>,
}

trait MutationLeaseGuard: Send {}
impl<T: Send> MutationLeaseGuard for T {}

impl MutationLease {
    /// A lease for an ephemeral log. The kernel's shared in-process mutex still
    /// provides ordering between kernels derived from the same root.
    pub fn in_process() -> Self {
        Self { _guard: None }
    }

    /// Keep a backend-owned guard alive until this lease is dropped.
    pub fn guarded<T: Send + 'static>(guard: T) -> Self {
        Self {
            _guard: Some(Box::new(guard)),
        }
    }
}

/// The log interface. Append computes the hash chain; replay/projection read it.
#[async_trait]
pub trait EventLog: Send + Sync {
    async fn append(&self, e: Event) -> Result<Event, KernelError>;
    async fn events(&self, session: Ulid) -> Vec<Event>;

    /// Read trusted history, propagating integrity and storage failures. Durable
    /// backends override this; ephemeral logs have no serialized trust boundary.
    async fn checked_events(&self, session: Ulid) -> Result<Vec<Event>, KernelError> {
        Ok(self.events(session).await)
    }

    /// Acquire the durable writer lane for one state identity. The lease must
    /// stay alive from before the side effect through its `ToolObs` and any
    /// derived event. Single-process backends can use this no-op default.
    async fn acquire_mutation_lease(
        &self,
        _mutation_key: &str,
    ) -> Result<MutationLease, KernelError> {
        Ok(MutationLease::in_process())
    }

    /// Every session in the log, newest activity first — for the resume picker.
    /// Default: none (in-memory/ephemeral logs need not implement it).
    async fn sessions(&self) -> Vec<SessionMeta> {
        Vec::new()
    }

    /// Fork a session, re-appending everything before `at_event` under a fresh id
    /// as an independently hash-valid chain, so the original is never mutated.
    /// This is what makes rewind non-destructive. `at_event` is a cut *before* it.
    /// The default impl rebuilds via [`Self::events`] + [`Self::append`].
    async fn fork(&self, session: Ulid, at_event: Ulid) -> Result<Ulid, KernelError> {
        let events = self.checked_events(session).await?;
        let idx = cut_index(&events, at_event).ok_or_else(|| {
            KernelError::Log(format!("event {at_event} not in session {session}"))
        })?;
        let new_id = Ulid::new();
        // Fresh timestamps keep the branch visible in newest-first session
        // lists; the increment preserves intra-fork order.
        let forked_at = now_ts();
        for (i, e) in events[..idx].iter().enumerate() {
            let mut clone = e.clone();
            clone.id = Ulid::new();
            clone.session_id = new_id;
            clone.parent_id = None;
            clone.ts = forked_at + i as f64 * 1e-6;
            clone.provenance = Provenance {
                source: "fork".into(),
            };
            self.append(clone).await?;
        }
        Ok(new_id)
    }
}

/// Cut point for rewind/fork: everything below the returned index is retained,
/// everything from it onward is discarded. `None` if the event isn't present.
pub fn cut_index(events: &[Event], at_event: Ulid) -> Option<usize> {
    events.iter().position(|e| e.id == at_event)
}

/// A file to roll back during a code rewind: restore `snapshot` (the pre-write
/// copy the sandbox took) to `path`, or *delete* `path` when `snapshot` is
/// `None` (the write that follows the cut is the one that created the file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRollback {
    pub path: String,
    pub snapshot: Option<String>,
}

/// Normalize recorded write paths so aliases share one rollback entry.
fn rollback_path_identity(path: &str, workspace_root: &Path) -> String {
    let path = Path::new(path);
    let rooted = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    };
    let identity = rooted.canonicalize().unwrap_or_else(|_| {
        let mut normalized = PathBuf::new();
        for component in rooted.components() {
            match component {
                Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
                Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR_STR),
                Component::CurDir => {}
                Component::ParentDir => {
                    let can_pop = matches!(
                        normalized.components().next_back(),
                        Some(Component::Normal(_))
                    );
                    if can_pop {
                        normalized.pop();
                    } else if !normalized.has_root() {
                        normalized.push("..");
                    }
                }
                Component::Normal(part) => normalized.push(part),
            }
        }
        normalized
    });
    identity.to_string_lossy().into_owned()
}

/// Return one rollback entry per written path at or after `at_event`.
/// The earliest post-cut snapshot restores the path's state at the cut;
/// `None` means the path must be deleted.
pub fn rollback_plan(events: &[Event], at_event: Ulid) -> Vec<FileRollback> {
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    rollback_plan_in(events, at_event, &workspace_root)
}

/// Compute a rollback plan relative to the execution workspace.
pub fn rollback_plan_in(
    events: &[Event],
    at_event: Ulid,
    workspace_root: &Path,
) -> Vec<FileRollback> {
    let Some(idx) = cut_index(events, at_event) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut plan = Vec::new();
    for e in &events[idx..] {
        if e.kind != EventKind::ToolObs {
            continue;
        }
        // Observation payload is { intent_id, ok, payload: <tool result json> }.
        let Some(result) = e.payload.get("payload").and_then(Value::as_object) else {
            continue;
        };
        // Write-family results (fs.write / fs.edit / fs.multi_edit) are exactly
        // those carrying both a `path` and a `snapshot` key (id or null).
        if !result.contains_key("snapshot") {
            continue;
        }
        let Some(path) = result.get("path").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(rollback_path_identity(path, workspace_root)) {
            continue; // keep the earliest write per path — that's the cut state
        }
        let snapshot = result
            .get("snapshot")
            .and_then(Value::as_str)
            .map(str::to_owned);
        plan.push(FileRollback {
            path: path.to_string(),
            snapshot,
        });
    }
    plan
}

/// In-memory event log.
pub struct InMemoryLog {
    inner: Mutex<Vec<Event>>,
    last_hash: Mutex<[u8; 32]>,
}

impl InMemoryLog {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            last_hash: Mutex::new([0u8; 32]),
        }
    }
}

impl Default for InMemoryLog {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventLog for InMemoryLog {
    async fn append(&self, mut e: Event) -> Result<Event, KernelError> {
        let mut last = self
            .last_hash
            .lock()
            .map_err(|_| KernelError::Log("poisoned".into()))?;
        e.prev_hash = *last;
        let next = chain_hash(&e.prev_hash, &e);
        *last = next;
        let mut v = self
            .inner
            .lock()
            .map_err(|_| KernelError::Log("poisoned".into()))?;
        v.push(e.clone());
        Ok(e)
    }

    async fn events(&self, session: Ulid) -> Vec<Event> {
        self.inner
            .lock()
            .map(|v| {
                v.iter()
                    .filter(|e| e.session_id == session)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn now_ts() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The canonical tamper-evident link — every backend calls this so in-memory and
/// on-disk logs hash identically. Version 2 is domain-separated and length-framed
/// and authenticates every stored field; v1 rows are migrated at open.
pub fn chain_hash(prev: &[u8; 32], e: &Event) -> [u8; 32] {
    let payload = serde_json::to_vec(&e.payload).unwrap_or_else(|_| b"null".to_vec());
    chain_hash_with_payload(prev, e, &payload)
}

/// Hash an event using the exact JSON bytes held by its durable backend.
///
/// Persistent logs use this entry point so verification authenticates what was
/// stored instead of parsing and reserializing JSON first. That distinction is
/// observable for valid JSON number spellings and object order, and the latter
/// can vary across serializer implementations and platforms.
pub fn chain_hash_with_payload(prev: &[u8; 32], e: &Event, payload: &[u8]) -> [u8; 32] {
    match e.hash_version {
        1 => legacy_chain_hash_with_payload(prev, e, payload),
        EVENT_HASH_VERSION => full_chain_hash(prev, e, payload),
        // Unknown versions must never accidentally verify as a known format.
        // Hashing a version-specific rejection domain gives callers a stable
        // mismatch while store verification reports the unsupported version.
        version => {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"medha.event-chain.unsupported");
            h.update([version]);
            h.update(prev);
            h.finalize().into()
        }
    }
}

/// Compatibility decoder for pre-v2 databases. It is public only so the store
/// can verify the old chain before rewriting it to the complete v2 encoding.
pub fn legacy_chain_hash(prev: &[u8; 32], e: &Event) -> [u8; 32] {
    let payload = e.payload.to_string();
    legacy_chain_hash_with_payload(prev, e, payload.as_bytes())
}

fn legacy_chain_hash_with_payload(prev: &[u8; 32], e: &Event, payload: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(prev);
    h.update(e.kind.as_str().as_bytes());
    h.update(e.session_id.to_string().as_bytes());
    h.update(payload);
    h.update(e.ts.to_le_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

fn full_chain_hash(prev: &[u8; 32], e: &Event, payload: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    fn framed(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    let mut h = Sha256::new();
    h.update(b"medha.event-chain.v2\0");
    h.update([e.hash_version]);
    h.update(prev);
    // Cover the value stored on the event as well as the running chain input.
    // Verification requires them to agree; framing both makes the contract
    // explicit and prevents a future caller from silently omitting the field.
    h.update(e.prev_hash);
    framed(&mut h, e.id.to_string().as_bytes());
    framed(&mut h, e.session_id.to_string().as_bytes());
    match e.parent_id {
        Some(parent) => {
            h.update([1]);
            framed(&mut h, parent.to_string().as_bytes());
        }
        None => h.update([0]),
    }
    framed(&mut h, e.kind.as_str().as_bytes());
    framed(&mut h, payload);
    framed(&mut h, e.trust.as_str().as_bytes());
    framed(&mut h, e.provenance.source.as_bytes());
    h.update(e.ts.to_bits().to_le_bytes());
    h.finalize().into()
}

#[derive(Deserialize)]
struct CompactionSnapshotV1 {
    version: u64,
    messages: Vec<Message>,
    ordered: Vec<ModelMessage>,
}

fn same_snapshot_message(left: &Message, right: &Message) -> bool {
    left.role == right.role
        && left.attachments == right.attachments
        && left.content == right.content
        && left.tool_call_id == right.tool_call_id
        && left.trust == right.trust
        && left.tool_calls.len() == right.tool_calls.len()
        && left
            .tool_calls
            .iter()
            .zip(&right.tool_calls)
            .all(|(a, b)| a.id == b.id && a.tool == b.tool && a.args == b.args)
}

/// The lossy legacy/control view represented by one canonical message. Opaque
/// reasoning and media parts intentionally have no legacy equivalent.
fn snapshot_legacy_views(message: &ModelMessage) -> Vec<Message> {
    if message.role == crate::types::Role::Tool {
        let mut results = Vec::new();
        let mut fallback = String::new();
        for part in &message.parts {
            match part {
                ContentPart::ToolResult(part) => {
                    let mut result = Message::tool_result(&part.tool_call_id, &part.content);
                    result.trust = message.trust;
                    results.push(result);
                }
                ContentPart::Text(part) => fallback.push_str(&part.text),
                _ => {}
            }
        }
        return if results.is_empty() {
            let mut fallback_message = Message::new(crate::types::Role::Tool, fallback);
            fallback_message.trust = message.trust;
            vec![fallback_message]
        } else {
            results
        };
    }

    let mut text = String::new();
    let mut calls = Vec::new();
    for part in &message.parts {
        match part {
            ContentPart::Text(part) => text.push_str(&part.text),
            ContentPart::ToolCall(part) => calls.push(ToolIntent {
                id: part.id.clone(),
                tool: part.tool.clone(),
                args: part.args.clone(),
            }),
            _ => {}
        }
    }
    let mut legacy = if message.role == crate::types::Role::Assistant {
        Message::assistant_calls(text, calls)
    } else {
        Message::new(message.role.clone(), text)
    };
    legacy.trust = message.trust;
    legacy.attachments = message
        .parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Media(media) => Some(media.clone()),
            _ => None,
        })
        .collect();
    vec![legacy]
}

/// A persisted request checkpoint must already be provider-sendable. Repairing
/// a dangling or mismatched tool turn during projection would mean resume no
/// longer reproduces the bytes represented by the checkpoint.
fn snapshot_tool_grammar_is_closed(messages: &[Message]) -> bool {
    use std::collections::{HashSet, VecDeque};

    let mut pending = VecDeque::<&str>::new();
    let mut passed_system_prefix = false;
    for message in messages {
        if message.role == crate::types::Role::System {
            if passed_system_prefix || !pending.is_empty() {
                return false;
            }
            continue;
        }
        passed_system_prefix = true;

        if let Some(expected) = pending.pop_front() {
            if message.role != crate::types::Role::Tool
                || message.tool_call_id.as_deref() != Some(expected)
                || !message.tool_calls.is_empty()
            {
                return false;
            }
            continue;
        }

        if message.role == crate::types::Role::Tool {
            return false;
        }
        if !message.tool_calls.is_empty() {
            if message.role != crate::types::Role::Assistant {
                return false;
            }
            let mut turn_ids = HashSet::new();
            for call in &message.tool_calls {
                if call.id.is_empty() || !turn_ids.insert(call.id.as_str()) {
                    return false;
                }
                pending.push_back(&call.id);
            }
        }
    }
    pending.is_empty()
}

/// Decode one checkpoint as an atomic pair of views. A half-valid snapshot is
/// not a checkpoint: accepting one side while the other falls back makes the
/// context engine and provider consume different histories on resume.
fn compaction_snapshot(payload: &Value) -> Option<CompactionSnapshotV1> {
    let snapshot: CompactionSnapshotV1 =
        serde_json::from_value(payload.get("snapshot")?.clone()).ok()?;
    if snapshot.version != 1
        || snapshot.messages.len() != snapshot.ordered.len()
        || !snapshot_tool_grammar_is_closed(&snapshot.messages)
        || !snapshot
            .messages
            .iter()
            .zip(&snapshot.ordered)
            .all(|(legacy, canonical)| {
                let views = snapshot_legacy_views(canonical);
                matches!(
                    views.as_slice(),
                    [candidate] if same_snapshot_message(candidate, legacy)
                )
            })
    {
        return None;
    }
    Some(snapshot)
}

pub(crate) fn has_valid_compaction_snapshot(payload: &Value) -> bool {
    compaction_snapshot(payload).is_some()
}

fn compacted_legacy_snapshot(payload: &Value) -> Option<Vec<Message>> {
    Some(compaction_snapshot(payload)?.messages)
}

fn compacted_ordered_snapshot(payload: &Value) -> Option<Vec<ModelMessage>> {
    Some(compaction_snapshot(payload)?.ordered)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UserAdmission {
    session_id: Ulid,
    text: String,
    trust: TrustLabel,
    provenance: String,
    attachments: Value,
}

/// Whether this event is a new admitted input. Text equality is irrelevant unless
/// the event names what it retries, and the full admission tuple must match, so a
/// forged retry marker cannot erase a weaker trust label.
fn is_new_user_admission(
    event: &Event,
    admissions: &mut std::collections::HashMap<Ulid, UserAdmission>,
) -> bool {
    let admission = UserAdmission {
        session_id: event.session_id,
        text: event
            .payload
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        trust: event.trust,
        provenance: event.provenance.source.clone(),
        attachments: event
            .payload
            .get("attachments")
            .cloned()
            .unwrap_or_else(|| json!([])),
    };
    let is_exact_retry = event
        .payload
        .get("retry_of")
        .and_then(Value::as_str)
        .and_then(|id| Ulid::from_string(id).ok())
        .and_then(|id| admissions.get(&id))
        .is_some_and(|original| original == &admission);

    // Record even a coalesced retry's identity so an explicit retry chain
    // remains deterministic.
    admissions.insert(event.id, admission);
    !is_exact_retry
}

pub fn project_messages(events: &[Event]) -> Vec<Message> {
    project_messages_impl(events, false)
}

/// Rebuild the exact legacy model-request view. Unlike the public transcript
/// projection, this retains the system sheath stored in a compaction checkpoint
/// so resume cannot silently substitute a different date/persona/instruction
/// set.
pub(crate) fn project_request_messages(events: &[Event]) -> Vec<Message> {
    project_messages_impl(events, true)
}

fn project_messages_impl(events: &[Event], retain_checkpoint_system: bool) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    let mut text = String::new();
    let mut intents: Vec<ToolIntent> = Vec::new();
    let mut assistant_open = false;
    let mut canonical_call_ids = std::collections::HashSet::new();
    let mut user_admissions = std::collections::HashMap::new();

    // Emit the pending assistant turn (text + its tool calls) if one is building.
    fn flush(
        out: &mut Vec<Message>,
        text: &mut String,
        intents: &mut Vec<ToolIntent>,
        open: &mut bool,
    ) {
        if *open {
            out.push(Message::assistant_calls(
                std::mem::take(text),
                std::mem::take(intents),
            ));
            *open = false;
        }
    }

    for e in events {
        match e.kind {
            EventKind::UserMessage => {
                flush(&mut out, &mut text, &mut intents, &mut assistant_open);
                canonical_call_ids.clear();
                let t = e
                    .payload
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if is_new_user_admission(e, &mut user_admissions) {
                    // Carry the recorded label back onto the message so a
                    // resumed session taints exactly as the live one did.
                    let mut message = Message::user(t);
                    message.attachments = serde_json::from_value(
                        e.payload
                            .get("attachments")
                            .cloned()
                            .unwrap_or_else(|| json!([])),
                    )
                    .unwrap_or_default();
                    if e.trust != TrustLabel::User {
                        message.trust = Some(e.trust);
                    }
                    out.push(message);
                }
            }
            EventKind::ModelText => {
                text = e
                    .payload
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                assistant_open = true;
            }
            EventKind::ModelIntent => {
                if let (Some(id), Some(tool)) = (
                    e.payload.get("id").and_then(Value::as_str),
                    e.payload.get("tool").and_then(Value::as_str),
                ) {
                    if canonical_call_ids.contains(id) {
                        continue;
                    }
                    intents.push(ToolIntent {
                        id: id.to_string(),
                        tool: tool.to_string(),
                        args: e.payload.get("args").cloned().unwrap_or(Value::Null),
                    });
                    assistant_open = true;
                }
            }
            EventKind::ModelMessage => {
                if let Ok(message) = serde_json::from_value::<ModelMessage>(e.payload.clone()) {
                    let mut content = String::new();
                    let mut calls = Vec::new();
                    for part in message.parts {
                        match part {
                            ContentPart::Text(part) => content.push_str(&part.text),
                            ContentPart::ToolCall(part) => {
                                canonical_call_ids.insert(part.id.clone());
                                calls.push(ToolIntent {
                                    id: part.id,
                                    tool: part.tool,
                                    args: part.args,
                                });
                            }
                            _ => {}
                        }
                    }
                    // New kernels retain `model.text` for legacy consumers and
                    // append the authoritative ordered message immediately
                    // after it. Coalesce that compatibility pair on replay.
                    if assistant_open && intents.is_empty() && text == content {
                        text.clear();
                        assistant_open = false;
                    } else {
                        flush(&mut out, &mut text, &mut intents, &mut assistant_open);
                    }
                    canonical_call_ids.clear();
                    canonical_call_ids.extend(calls.iter().map(|call| call.id.clone()));
                    let mut legacy = if message.role == crate::types::Role::Assistant {
                        Message::assistant_calls(content, calls)
                    } else {
                        Message::new(message.role, content)
                    };
                    legacy.trust = message.trust;
                    out.push(legacy);
                }
            }
            EventKind::ToolObs => {
                // The assistant turn must precede its results; flush before the
                // first result of the turn.
                flush(&mut out, &mut text, &mut intents, &mut assistant_open);
                let id = e
                    .payload
                    .get("intent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let content = visible_observation_payload(&e.payload).to_string();
                out.push(Message::tool_result(id, content));
                if let Some(carrier) = replayed_tool_media(&e.payload, e.trust) {
                    out.push(carrier);
                }
            }
            EventKind::Compaction => {
                if let Some(mut snapshot) = compacted_legacy_snapshot(&e.payload) {
                    canonical_call_ids.clear();
                    if !retain_checkpoint_system {
                        // Transcript surfaces regenerate their system prompt;
                        // exact model-request replay uses the sibling projector.
                        snapshot.retain(|message| message.role != crate::types::Role::System);
                    }
                    out = snapshot;
                    text.clear();
                    intents.clear();
                    assistant_open = false;
                    continue;
                }
                // A versioned checkpoint is one atomic pair of legacy and
                // canonical views. If either half is malformed, the entire
                // event is inert; its diagnostic `summary` must not fall
                // through to the lossy pre-v1 compatibility path.
                if e.payload.get("snapshot").is_some() {
                    continue;
                }
                if let Some(summary) = e.payload.get("summary").and_then(Value::as_str)
                    && !summary.trim().is_empty()
                {
                    canonical_call_ids.clear();
                    if retain_checkpoint_system {
                        out.retain(|message| message.role == crate::types::Role::System);
                    } else {
                        out.clear();
                    }
                    text.clear();
                    intents.clear();
                    assistant_open = false;
                    out.push(Message::new(crate::types::Role::Assistant, summary));
                }
            }
            _ => {} // reasoning / policy / session — not conversation
        }
    }
    flush(&mut out, &mut text, &mut intents, &mut assistant_open);
    close_dangling_tool_calls(out)
}

/// Rebuild ordered canonical history. Legacy flat events are upgraded into the
/// deterministic compatibility order; `model.message` payloads retain their
/// exact part ordering and opaque provider state.
pub fn project_ordered_messages(events: &[Event]) -> Vec<ModelMessage> {
    project_ordered_messages_impl(events, false)
}

/// Rebuild the exact canonical model-request view, including a checkpoint's
/// system sheath.
pub(crate) fn project_request_ordered_messages(events: &[Event]) -> Vec<ModelMessage> {
    project_ordered_messages_impl(events, true)
}

fn project_ordered_messages_impl(
    events: &[Event],
    retain_checkpoint_system: bool,
) -> Vec<ModelMessage> {
    let mut out = Vec::new();
    let mut assistant_parts = Vec::new();
    let mut canonical_call_ids = std::collections::HashSet::new();
    let mut user_admissions = std::collections::HashMap::new();

    fn flush(out: &mut Vec<ModelMessage>, parts: &mut Vec<ContentPart>) {
        if !parts.is_empty() {
            out.push(ModelMessage {
                role: crate::types::Role::Assistant,
                parts: std::mem::take(parts),
                trust: None,
            });
        }
    }

    for event in events {
        match event.kind {
            EventKind::UserMessage => {
                flush(&mut out, &mut assistant_parts);
                canonical_call_ids.clear();
                let text = event
                    .payload
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if is_new_user_admission(event, &mut user_admissions) {
                    let mut message = Message::user(text);
                    message.attachments = serde_json::from_value(
                        event
                            .payload
                            .get("attachments")
                            .cloned()
                            .unwrap_or_else(|| json!([])),
                    )
                    .unwrap_or_default();
                    if event.trust != TrustLabel::User {
                        message.trust = Some(event.trust);
                    }
                    out.push(message.ordered());
                }
            }
            EventKind::ModelText => {
                assistant_parts.push(ContentPart::Text(TextPart {
                    text: event
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    provider_state: Vec::new(),
                }));
            }
            EventKind::ModelIntent => {
                if let (Some(id), Some(tool)) = (
                    event.payload.get("id").and_then(Value::as_str),
                    event.payload.get("tool").and_then(Value::as_str),
                ) {
                    if canonical_call_ids.contains(id) {
                        continue;
                    }
                    assistant_parts.push(ContentPart::ToolCall(ToolCallPart {
                        id: id.to_string(),
                        tool: tool.to_string(),
                        args: event.payload.get("args").cloned().unwrap_or(Value::Null),
                        provider_state: Vec::new(),
                    }));
                }
            }
            EventKind::ModelMessage => {
                if let Ok(message) = serde_json::from_value::<ModelMessage>(event.payload.clone()) {
                    let pending_text: String = assistant_parts
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::Text(part) => Some(part.text.as_str()),
                            _ => None,
                        })
                        .collect();
                    let only_pending_text = assistant_parts
                        .iter()
                        .all(|part| matches!(part, ContentPart::Text(_)));
                    let canonical_text: String = message
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::Text(part) => Some(part.text.as_str()),
                            _ => None,
                        })
                        .collect();
                    if only_pending_text && pending_text == canonical_text {
                        assistant_parts.clear();
                    } else {
                        flush(&mut out, &mut assistant_parts);
                    }
                    canonical_call_ids = message
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::ToolCall(call) => Some(call.id.clone()),
                            _ => None,
                        })
                        .collect();
                    out.push(message);
                }
            }
            EventKind::ToolObs => {
                flush(&mut out, &mut assistant_parts);
                let id = event
                    .payload
                    .get("intent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let content = visible_observation_payload(&event.payload).to_string();
                out.push(ModelMessage {
                    role: crate::types::Role::Tool,
                    parts: vec![ContentPart::ToolResult(ToolResultPart {
                        tool_call_id: id.to_string(),
                        content,
                        provider_state: Vec::new(),
                    })],
                    trust: None,
                });
                if let Some(carrier) = replayed_tool_media(&event.payload, event.trust) {
                    out.push(carrier.ordered());
                }
            }
            EventKind::Compaction => {
                if let Some(mut snapshot) = compacted_ordered_snapshot(&event.payload) {
                    canonical_call_ids.clear();
                    if !retain_checkpoint_system {
                        snapshot.retain(|message| message.role != crate::types::Role::System);
                    }
                    out = snapshot;
                    assistant_parts.clear();
                    continue;
                }
                if event.payload.get("snapshot").is_some() {
                    continue;
                }
                if let Some(summary) = event.payload.get("summary").and_then(Value::as_str)
                    && !summary.trim().is_empty()
                {
                    canonical_call_ids.clear();
                    if retain_checkpoint_system {
                        out.retain(|message| message.role == crate::types::Role::System);
                    } else {
                        out.clear();
                    }
                    assistant_parts.clear();
                    out.push(Message::new(crate::types::Role::Assistant, summary).ordered());
                }
            }
            _ => {}
        }
    }
    flush(&mut out, &mut assistant_parts);
    close_dangling_ordered_tool_calls(out)
}

fn close_dangling_ordered_tool_calls(messages: Vec<ModelMessage>) -> Vec<ModelMessage> {
    let mut out = Vec::with_capacity(messages.len());
    let mut iter = messages.into_iter().peekable();
    while let Some(message) = iter.next() {
        let calls: Vec<String> = if message.role == crate::types::Role::Assistant {
            message
                .parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::ToolCall(call) => Some(call.id.clone()),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        out.push(message);
        if calls.is_empty() {
            continue;
        }

        let mut answered = std::collections::HashSet::new();
        while iter
            .peek()
            .is_some_and(|next| next.role == crate::types::Role::Tool)
        {
            let result = iter.next().expect("peeked");
            for part in &result.parts {
                if let ContentPart::ToolResult(result) = part {
                    answered.insert(result.tool_call_id.clone());
                }
            }
            out.push(result);
        }
        for id in calls {
            if !answered.contains(&id) {
                out.push(ModelMessage {
                    role: crate::types::Role::Tool,
                    parts: vec![ContentPart::ToolResult(ToolResultPart {
                        tool_call_id: id,
                        content: "[interrupted]".into(),
                        provider_state: Vec::new(),
                    })],
                    trust: None,
                });
            }
        }
    }
    out
}

/// Close unanswered calls after interruption by synthesizing `[interrupted]`
/// results. This preserves provider tool-call grammar on resume.
fn close_dangling_tool_calls(msgs: Vec<Message>) -> Vec<Message> {
    use crate::types::Role;
    let mut out: Vec<Message> = Vec::with_capacity(msgs.len());
    let mut iter = msgs.into_iter().peekable();
    while let Some(m) = iter.next() {
        let calls: Vec<String> = if m.role == Role::Assistant {
            m.tool_calls.iter().map(|c| c.id.clone()).collect()
        } else {
            Vec::new()
        };
        out.push(m);
        if calls.is_empty() {
            continue;
        }
        // Consume this assistant's run of tool results, tracking which ids were answered.
        let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
        while iter.peek().map(|n| n.role == Role::Tool).unwrap_or(false) {
            let tool = iter.next().unwrap();
            if let Some(id) = &tool.tool_call_id {
                answered.insert(id.clone());
            }
            out.push(tool);
        }
        for id in calls {
            if !answered.contains(&id) {
                out.push(Message::tool_result(id, "[interrupted]"));
            }
        }
    }
    out
}

/// A broken link in the event-log hash chain — the log has been tampered with
/// (or corrupted) since it was written.
#[derive(Debug, Clone, thiserror::Error)]
#[error("event log hash chain broken at index {index} (event {event_id})")]
pub struct ChainError {
    pub index: usize,
    pub event_id: Ulid,
}

/// Recompute the chain over `events` and confirm each `prev_hash` matches the
/// running hash, so any altered field or order breaks the next link. The chain is
/// global — pass the full log in append order, not one session's slice.
pub fn verify_chain(events: &[Event]) -> Result<(), ChainError> {
    let mut prev = [0u8; 32];
    for (index, e) in events.iter().enumerate() {
        if e.prev_hash != prev {
            return Err(ChainError {
                index,
                event_id: e.id,
            });
        }
        prev = chain_hash(&prev, e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev() -> Event {
        Event::model_text(&Session::new(), "hello")
    }

    #[test]
    fn context_file_events_are_workspace_trusted_and_round_trip_kinds() {
        let session = Session::new();
        let loaded = Event::context_file(
            &session,
            "sub/AGENTS.md",
            "use quotes-and-hyphens",
            false,
            TrustLabel::Workspace,
        );
        assert_eq!(loaded.kind, EventKind::ContextFileLoaded);
        assert_eq!(loaded.trust, TrustLabel::Workspace);
        assert_eq!(loaded.payload["path"], "sub/AGENTS.md");
        let blocked = Event::context_file(
            &session,
            "CLAUDE.md",
            "[blocked context file]",
            true,
            TrustLabel::Workspace,
        );
        assert_eq!(blocked.kind, EventKind::ContextFileBlocked);
        for kind in [EventKind::ContextFileLoaded, EventKind::ContextFileBlocked] {
            assert_eq!(EventKind::parse(kind.as_str()), Some(kind));
        }
    }

    /// An image a tool produced has to survive a restart: the observation event
    /// carries its reference, and both projectors rebuild the same carrier turn
    /// the live run made.
    #[test]
    fn a_tool_image_is_replayed_as_the_same_carrier_turn() {
        use crate::types::{ContentPart, MediaPart, MediaSource, Role};
        let session = Session::new();
        let mut observation = Observation::ok("call-7", json!({ "path": "shot.png" }));
        observation.media = vec![MediaPart {
            mime_type: "image/png".into(),
            source: MediaSource::Artifact("hash-7".into()),
            label: None,
            provider_state: Vec::new(),
        }];
        let events = vec![
            Event::user_message(&session, "what does the screenshot show?"),
            Event::model_intent(
                &session,
                &ToolIntent {
                    id: "call-7".into(),
                    tool: "image.view".into(),
                    args: json!({ "path": "shot.png" }),
                },
            ),
            Event::tool_obs(&session, &observation, TrustLabel::Tool),
        ];

        let messages = project_messages(&events);
        let carrier = messages.last().expect("a carrier turn follows the result");
        assert!(matches!(carrier.role, Role::User));
        assert_eq!(carrier.content, "[1 image(s) returned by tool call call-7]");
        assert_eq!(carrier.attachments, observation.media);
        assert_eq!(carrier.trust, Some(TrustLabel::Tool));

        let ordered = project_ordered_messages(&events);
        let last = ordered.last().expect("the request projector agrees");
        assert!(matches!(
            last.parts.last(),
            Some(ContentPart::Media(part)) if part.source == MediaSource::Artifact("hash-7".into())
        ));
    }

    #[test]
    fn an_observation_without_images_gains_no_extra_turn() {
        let session = Session::new();
        let events = vec![Event::tool_obs(
            &session,
            &Observation::ok("1", json!({ "entries": [] })),
            TrustLabel::Tool,
        )];
        assert_eq!(project_messages(&events).len(), 1);
        assert_eq!(project_ordered_messages(&events).len(), 1);
    }

    #[test]
    fn projection_rebuilds_conversation_from_the_log() {
        use crate::types::Role;
        let s = Session::new();
        // A turn: user asks → model calls a tool → tool responds → model answers.
        let events = vec![
            Event::user_message(&s, "list the files"),
            Event::model_text(&s, ""),
            Event::model_intent(
                &s,
                &ToolIntent {
                    id: "1".into(),
                    tool: "fs.list".into(),
                    args: json!({ "path": "." }),
                },
            ),
            Event::tool_obs(
                &s,
                &Observation::ok("1", json!({ "entries": ["a.rs"] })),
                TrustLabel::Tool,
            ),
            Event::model_text(&s, "There is one file: a.rs"),
        ];
        let msgs = project_messages(&events);

        // user → assistant(+tool call) → tool result → assistant(final)
        assert_eq!(msgs.len(), 4, "{msgs:?}");
        assert!(matches!(msgs[0].role, Role::User) && msgs[0].content == "list the files");
        assert!(matches!(msgs[1].role, Role::Assistant) && msgs[1].tool_calls.len() == 1);
        assert_eq!(msgs[1].tool_calls[0].tool, "fs.list");
        assert!(matches!(msgs[2].role, Role::Tool) && msgs[2].tool_call_id.as_deref() == Some("1"));
        assert!(msgs[2].content.contains("a.rs"));
        assert!(matches!(msgs[3].role, Role::Assistant) && msgs[3].content.contains("a.rs"));
    }

    #[test]
    fn agent_report_acknowledgements_stay_durable_but_never_reach_replay() {
        let session = Session::new();
        let event = Event::tool_obs(
            &session,
            &Observation::ok(
                "wait-1",
                json!({
                    "reports": [{"summary": "done"}],
                    AGENT_REPORT_ACKS_FIELD: ["dispatch-1"],
                }),
            ),
            TrustLabel::Tool,
        );
        assert_eq!(
            event.payload["payload"][AGENT_REPORT_ACKS_FIELD],
            json!(["dispatch-1"]),
            "the durable outbox acknowledgement was lost"
        );

        let legacy = project_messages(std::slice::from_ref(&event));
        let ordered = project_ordered_messages(std::slice::from_ref(&event));
        assert!(!legacy[0].content.contains(AGENT_REPORT_ACKS_FIELD));
        let ContentPart::ToolResult(result) = &ordered[0].parts[0] else {
            panic!("expected ordered tool result")
        };
        assert!(!result.content.contains(AGENT_REPORT_ACKS_FIELD));
        assert!(result.content.contains("reports"));
    }

    #[test]
    fn resume_consumes_compaction_summary_instead_of_full_history() {
        let s = Session::new();
        let events = vec![
            Event::user_message(&s, "first task"),
            Event::model_text(&s, "did the first thing"),
            Event::user_message(&s, "second task"),
            Event::compaction(&s, 1000, 200, Some("HANDOFF: goal + progress")),
            Event::user_message(&s, "third task"),
        ];
        let msgs = project_messages(&events);
        assert!(
            msgs[0].content.contains("HANDOFF: goal + progress"),
            "summary is the head: {msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| m.content == "first task"),
            "pre-compaction history collapsed"
        );
        assert!(
            msgs.iter().any(|m| m.content == "third task"),
            "post-compaction turn kept"
        );
    }

    #[test]
    fn legacy_compaction_snapshot_replays_byte_equivalent_model_input() {
        use crate::types::Role;

        let session = Session::new();
        let head_call = ToolIntent {
            id: "head-call".into(),
            tool: "fs.read".into(),
            args: json!({"path": "requirements.md"}),
        };
        let tail_call = ToolIntent {
            id: "tail-call".into(),
            tool: "fs.read".into(),
            args: json!({"path": "recent.rs"}),
        };
        // This is the exact live request after Full compaction: protected head,
        // middle summary, then a protected recent tool-call/result pair.
        let live = vec![
            Message::system("SYSTEM"),
            Message::user("original instructions"),
            Message::assistant_calls("I will inspect the requirements.", vec![head_call]),
            Message::tool_result("head-call", "requirements"),
            Message::new(
                Role::Assistant,
                "HANDOFF: completed work and open questions",
            ),
            Message::user("check the latest file"),
            Message::assistant_calls("", vec![tail_call]),
            Message::tool_result("tail-call", "recent contents"),
        ];
        let ordered: Vec<ModelMessage> = live.iter().map(Message::ordered).collect();
        let checkpoint = Event::compaction_snapshot(
            &session,
            12_000,
            2_000,
            Some("HANDOFF: completed work and open questions"),
            &live,
            &ordered,
        );
        let events = vec![
            Event::user_message(&session, "discarded old history"),
            Event::model_text(&session, "also discarded"),
            checkpoint,
        ];

        let mut replayed_input = vec![live[0].clone()];
        replayed_input.extend(project_messages(&events));
        assert_eq!(
            serde_json::to_vec(&replayed_input).unwrap(),
            serde_json::to_vec(&live).unwrap(),
            "resume must send the exact legacy post-compaction request"
        );
        assert_eq!(
            serde_json::to_vec(&project_request_messages(&events)).unwrap(),
            serde_json::to_vec(&live).unwrap(),
            "the kernel request projector must retain the checkpoint system"
        );
        let call_index = replayed_input
            .iter()
            .position(|message| message.tool_calls.iter().any(|call| call.id == "tail-call"))
            .unwrap();
        assert_eq!(replayed_input[call_index + 1].role, Role::Tool);
        assert_eq!(
            replayed_input[call_index + 1].tool_call_id.as_deref(),
            Some("tail-call")
        );
    }

    #[test]
    fn ordered_compaction_snapshot_replays_byte_equivalent_model_input() {
        use crate::types::{ProviderState, ReasoningPart, Role};

        let session = Session::new();
        let state = ProviderState {
            protocol: crate::provider::Protocol::GeminiInteractions,
            kind: "thought-signature".into(),
            value: json!({"signature": "opaque-compaction-state"}),
        };
        let legacy = vec![
            Message::system("SYSTEM"),
            Message::user("original instructions"),
            Message::new(Role::Assistant, "HANDOFF"),
            Message::user("recent request"),
            Message::assistant_calls(
                "",
                vec![ToolIntent {
                    id: "canonical-tail".into(),
                    tool: "fs.read".into(),
                    args: json!({"path": "recent.rs"}),
                }],
            ),
            Message::tool_result("canonical-tail", "recent contents"),
        ];
        let mut live: Vec<ModelMessage> = legacy.iter().map(Message::ordered).collect();
        live[4] = ModelMessage {
            role: Role::Assistant,
            parts: vec![
                ContentPart::Reasoning(ReasoningPart {
                    text: Some("checking the recent file".into()),
                    provider_state: vec![state.clone()],
                }),
                ContentPart::ToolCall(ToolCallPart {
                    id: "canonical-tail".into(),
                    tool: "fs.read".into(),
                    args: json!({"path": "recent.rs"}),
                    provider_state: vec![state.clone()],
                }),
            ],
            trust: None,
        };
        live[5] = ModelMessage {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResultPart {
                tool_call_id: "canonical-tail".into(),
                content: "recent contents".into(),
                provider_state: vec![state],
            })],
            trust: None,
        };
        let checkpoint =
            Event::compaction_snapshot(&session, 12_000, 2_000, Some("HANDOFF"), &legacy, &live);
        assert!(
            !format!("{checkpoint:?}").contains("opaque-compaction-state"),
            "opaque provider state must stay out of diagnostics"
        );
        let events = vec![
            Event::model_message(
                &session,
                &ModelMessage {
                    role: Role::Assistant,
                    parts: vec![ContentPart::Text(TextPart {
                        text: "discarded canonical history".into(),
                        provider_state: Vec::new(),
                    })],
                    trust: None,
                },
            ),
            checkpoint,
        ];

        let mut replayed_input = vec![live[0].clone()];
        replayed_input.extend(project_ordered_messages(&events));
        assert_eq!(
            serde_json::to_vec(&replayed_input).unwrap(),
            serde_json::to_vec(&live).unwrap(),
            "resume must send the exact ordered post-compaction request"
        );
        assert_eq!(
            serde_json::to_vec(&project_request_ordered_messages(&events)).unwrap(),
            serde_json::to_vec(&live).unwrap(),
            "the canonical request projector must retain the checkpoint system"
        );
        assert!(replayed_input.iter().any(ModelMessage::has_provider_state));
        let call_index = replayed_input
            .iter()
            .position(|message| {
                message.parts.iter().any(|part| {
                    matches!(part, ContentPart::ToolCall(call) if call.id == "canonical-tail")
                })
            })
            .unwrap();
        assert_eq!(replayed_input[call_index + 1].role, Role::Tool);
        assert!(matches!(
            replayed_input[call_index + 1].parts.as_slice(),
            [ContentPart::ToolResult(result)] if result.tool_call_id == "canonical-tail"
        ));
    }

    #[test]
    fn compaction_snapshot_rejects_grouped_or_dangling_tool_results_atomically() {
        use crate::types::Role;

        let session = Session::new();
        let calls = vec![
            ToolIntent {
                id: "a".into(),
                tool: "fs.read".into(),
                args: json!({"path": "a"}),
            },
            ToolIntent {
                id: "b".into(),
                tool: "fs.read".into(),
                args: json!({"path": "b"}),
            },
        ];
        let legacy = vec![
            Message::assistant_calls("", calls.clone()),
            Message::tool_result("a", "result a"),
        ];
        let ordered = vec![
            legacy[0].ordered(),
            ModelMessage {
                role: Role::Tool,
                parts: vec![
                    ContentPart::ToolResult(ToolResultPart {
                        tool_call_id: "a".into(),
                        content: "result a".into(),
                        provider_state: Vec::new(),
                    }),
                    ContentPart::ToolResult(ToolResultPart {
                        tool_call_id: "b".into(),
                        content: "result b".into(),
                        provider_state: Vec::new(),
                    }),
                ],
                trust: None,
            },
        ];
        let grouped = Event::compaction_snapshot(
            &session,
            100,
            50,
            Some("must not become a legacy fallback"),
            &legacy,
            &ordered,
        );
        assert!(
            !has_valid_compaction_snapshot(&grouped.payload),
            "one canonical row cannot shadow only one of several tool results"
        );
        assert!(
            project_request_messages(std::slice::from_ref(&grouped)).is_empty(),
            "an invalid versioned checkpoint must be inert, including its summary"
        );
        assert!(project_request_ordered_messages(std::slice::from_ref(&grouped)).is_empty());

        let dangling_legacy = vec![Message::assistant_calls("", calls[..1].to_vec())];
        let dangling_ordered = dangling_legacy
            .iter()
            .map(Message::ordered)
            .collect::<Vec<_>>();
        let dangling = Event::compaction_snapshot(
            &session,
            100,
            50,
            Some("dangling"),
            &dangling_legacy,
            &dangling_ordered,
        );
        assert!(
            !has_valid_compaction_snapshot(&dangling.payload),
            "resume must not repair a checkpoint that claimed to be an exact request"
        );
    }

    #[test]
    fn malformed_checkpoint_after_valid_checkpoint_cannot_drop_system_sheath() {
        let session = Session::new();
        let live = vec![Message::system("SYSTEM"), Message::user("retained")];
        let ordered = live.iter().map(Message::ordered).collect::<Vec<_>>();
        let valid = Event::compaction_snapshot(&session, 100, 50, Some("valid"), &live, &ordered);
        let mut malformed = Event::compaction(&session, 50, 10, Some("DROP EVERYTHING"));
        malformed.payload["snapshot"] = json!({
            "version": 1,
            "messages": [Message::system("FORGED")],
            "ordered": {"not": "an array"}
        });
        let events = vec![
            valid,
            malformed,
            Event::user_message(&session, "after malformed"),
        ];

        let mut expected = live;
        expected.push(Message::user("after malformed"));
        assert_eq!(
            serde_json::to_vec(&project_request_messages(&events)).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_eq!(
            project_request_ordered_messages(&events),
            expected.iter().map(Message::ordered).collect::<Vec<_>>()
        );
        assert_eq!(
            project_request_messages(&events)
                .iter()
                .filter(|message| message.role == crate::types::Role::System)
                .count(),
            1
        );
    }

    #[test]
    fn legacy_summary_after_checkpoint_preserves_the_checkpoint_system() {
        let session = Session::new();
        let live = vec![Message::system("SYSTEM"), Message::user("old")];
        let ordered = live.iter().map(Message::ordered).collect::<Vec<_>>();
        let events = vec![
            Event::compaction_snapshot(&session, 100, 50, Some("first"), &live, &ordered),
            Event::compaction(&session, 50, 10, Some("legacy handoff")),
            Event::user_message(&session, "new"),
        ];
        let expected = vec![
            Message::system("SYSTEM"),
            Message::new(crate::types::Role::Assistant, "legacy handoff"),
            Message::user("new"),
        ];

        assert_eq!(
            serde_json::to_vec(&project_request_messages(&events)).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_eq!(
            project_request_ordered_messages(&events),
            expected.iter().map(Message::ordered).collect::<Vec<_>>()
        );
    }

    #[test]
    fn prune_compaction_without_summary_keeps_history() {
        let s = Session::new();
        let events = vec![
            Event::user_message(&s, "keep me"),
            Event::compaction(&s, 1000, 800, None), // prune-only: no summary
            Event::user_message(&s, "and me"),
        ];
        let msgs = project_messages(&events);
        assert!(
            msgs.iter().any(|m| m.content == "keep me"),
            "prune must not collapse history"
        );
        assert!(msgs.iter().any(|m| m.content == "and me"));
    }

    #[test]
    fn projection_closes_a_tool_call_interrupted_before_its_observation() {
        use crate::types::Role;
        let s = Session::new();
        // A turn cut short: the model asked for a tool (intent logged) but the
        // observation was never logged (Esc/crash mid-tool), and then the user
        // sent another message on resume.
        let events = vec![
            Event::user_message(&s, "read the file"),
            Event::model_text(&s, ""),
            Event::model_intent(
                &s,
                &ToolIntent {
                    id: "x1".into(),
                    tool: "fs.read".into(),
                    args: json!({}),
                },
            ),
            // no tool.observation for x1 — interrupted here
            Event::user_message(&s, "actually never mind, do this instead"),
        ];
        let msgs = project_messages(&events);

        // Every assistant tool_call must be answered by a following tool result,
        // else the provider 400s and resume is bricked.
        for (i, m) in msgs.iter().enumerate() {
            for call in &m.tool_calls {
                let answered = msgs[i + 1..].iter().any(|later| {
                    later.role == Role::Tool && later.tool_call_id.as_deref() == Some(&call.id)
                });
                assert!(answered, "dangling tool_call `{}` after interrupt", call.id);
            }
        }
        // The synthesized result carries the interrupted marker.
        assert!(
            msgs.iter()
                .any(|m| m.role == Role::Tool && m.content.contains("[interrupted]")),
            "expected a synthesized [interrupted] tool result: {msgs:?}"
        );
    }

    #[test]
    fn projection_preserves_legitimate_repeated_text_and_its_trust() {
        use crate::types::Role;
        let s = Session::new();
        let events = vec![
            Event::user_message(&s, "build the feature"),
            Event::user_message(&s, "build the feature"),
            Event::user_input(&s, "build the feature", TrustLabel::Web),
        ];
        let msgs = project_messages(&events);
        let users: Vec<_> = msgs.iter().filter(|m| m.role == Role::User).collect();
        assert_eq!(users.len(), 3, "every admitted input must replay: {msgs:?}");
        assert_eq!(users[0].trust, None);
        assert_eq!(users[1].trust, None);
        assert_eq!(users[2].trust, Some(TrustLabel::Web));

        let ordered = project_ordered_messages(&events);
        assert_eq!(ordered.len(), 3);
        assert_eq!(ordered[0].trust, None);
        assert_eq!(ordered[1].trust, None);
        assert_eq!(ordered[2].trust, Some(TrustLabel::Web));
    }

    #[test]
    fn projection_coalesces_only_an_exact_explicit_retry_identity() {
        let s = Session::new();
        let original = Event::user_input(&s, "same report", TrustLabel::Tool);
        let retry = Event::user_input_retry(&s, "same report", TrustLabel::Tool, original.id);
        let retry_chain = Event::user_input_retry(&s, "same report", TrustLabel::Tool, retry.id);
        let different_trust =
            Event::user_input_retry(&s, "same report", TrustLabel::Web, original.id);
        let mut different_provenance =
            Event::user_input_retry(&s, "same report", TrustLabel::Tool, original.id);
        different_provenance.provenance.source = "connector".into();
        let events = vec![
            original,
            retry,
            retry_chain,
            different_trust,
            different_provenance,
        ];

        let replayed = project_messages(&events);
        assert_eq!(replayed.len(), 3);
        assert_eq!(replayed[0].trust, Some(TrustLabel::Tool));
        assert_eq!(replayed[1].trust, Some(TrustLabel::Web));
        assert_eq!(replayed[2].trust, Some(TrustLabel::Tool));

        let ordered = project_ordered_messages(&events);
        assert_eq!(ordered.len(), 3);
        assert_eq!(ordered[0].trust, Some(TrustLabel::Tool));
        assert_eq!(ordered[1].trust, Some(TrustLabel::Web));
        assert_eq!(ordered[2].trust, Some(TrustLabel::Tool));
    }

    #[test]
    fn compaction_checkpoint_preserves_user_trust_in_both_views() {
        let s = Session::new();
        let legacy = vec![
            Message::system("system"),
            Message::user("untrusted report").carrying(TrustLabel::Web),
        ];
        let ordered = legacy.iter().map(Message::ordered).collect::<Vec<_>>();
        let checkpoint = Event::compaction_snapshot(&s, 1_000, 500, None, &legacy, &ordered);

        let legacy_replay = project_messages(std::slice::from_ref(&checkpoint));
        assert_eq!(legacy_replay.len(), 1);
        assert_eq!(legacy_replay[0].trust, Some(TrustLabel::Web));
        let ordered_replay = project_ordered_messages(&[checkpoint]);
        assert_eq!(ordered_replay.len(), 1);
        assert_eq!(ordered_replay[0].trust, Some(TrustLabel::Web));

        let mut mismatched_ordered = ordered;
        mismatched_ordered[1].trust = None;
        let mismatched =
            Event::compaction_snapshot(&s, 1_000, 500, None, &legacy, &mismatched_ordered);
        assert!(
            !has_valid_compaction_snapshot(&mismatched.payload),
            "a checkpoint must not authenticate two views with different trust"
        );
        assert!(project_request_messages(std::slice::from_ref(&mismatched)).is_empty());
        assert!(project_request_ordered_messages(&[mismatched]).is_empty());
    }

    #[test]
    fn canonical_user_event_keeps_trust_in_legacy_projection() {
        let s = Session::new();
        let canonical = Message::user("connector report")
            .carrying(TrustLabel::Web)
            .ordered();
        let event = Event::model_message(&s, &canonical);

        let replayed = project_messages(&[event]);
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].role, crate::types::Role::User);
        assert_eq!(replayed[0].content, "connector report");
        assert_eq!(replayed[0].trust, Some(TrustLabel::Web));
    }

    #[test]
    fn verify_chain_accepts_intact_and_rejects_tampered() {
        use futures::executor::block_on;
        let log = InMemoryLog::new();
        let s = Session::new();
        let e1 = block_on(log.append(Event::model_text(&s, "one"))).unwrap();
        let e2 = block_on(log.append(Event::model_text(&s, "two"))).unwrap();
        let e3 = block_on(log.append(Event::model_text(&s, "three"))).unwrap();

        // An untouched chain verifies.
        verify_chain(&[e1.clone(), e2.clone(), e3.clone()]).unwrap();

        // Tampering with a middle event's payload breaks the link into it.
        let mut tampered = e2.clone();
        tampered.payload = json!({ "text": "TWO (altered)" });
        let err = verify_chain(&[e1, tampered, e3]).unwrap_err();
        assert_eq!(
            err.index, 2,
            "the break shows up at the event that links off the altered one"
        );
    }

    #[test]
    fn hash_is_full_width_sha256() {
        let h = chain_hash(&[0u8; 32], &ev());
        assert!(
            h[8..].iter().any(|&b| b != 0),
            "upper 24 bytes must be populated by a real 256-bit hash"
        );
    }

    #[test]
    fn tamper_in_any_covered_field_changes_the_hash() {
        let base = ev();
        let h0 = chain_hash(&[0u8; 32], &base);
        // Different prev_hash → different link (chain property).
        assert_ne!(h0, chain_hash(&[1u8; 32], &base));
        let mut altered = base.clone();
        altered.prev_hash[0] ^= 1;
        assert_ne!(h0, chain_hash(&[0u8; 32], &altered));
        // Different payload → different hash.
        let mut altered = base.clone();
        altered.payload = json!({ "text": "HELLO" });
        assert_ne!(h0, chain_hash(&[0u8; 32], &altered));

        let mut altered = base.clone();
        altered.id = Ulid::new();
        assert_ne!(h0, chain_hash(&[0u8; 32], &altered));

        let mut altered = base.clone();
        altered.session_id = Ulid::new();
        assert_ne!(h0, chain_hash(&[0u8; 32], &altered));

        let mut altered = base.clone();
        altered.parent_id = Some(Ulid::new());
        assert_ne!(h0, chain_hash(&[0u8; 32], &altered));

        let mut altered = base.clone();
        altered.kind = EventKind::UserMessage;
        assert_ne!(h0, chain_hash(&[0u8; 32], &altered));

        let mut altered = base.clone();
        altered.trust = TrustLabel::Web;
        assert_ne!(h0, chain_hash(&[0u8; 32], &altered));

        let mut altered = base.clone();
        altered.provenance.source = "automation".into();
        assert_ne!(h0, chain_hash(&[0u8; 32], &altered));

        let mut altered = base.clone();
        altered.ts = f64::from_bits(base.ts.to_bits() ^ 1);
        assert_ne!(h0, chain_hash(&[0u8; 32], &altered));

        let mut altered = base;
        altered.hash_version ^= 1;
        assert_ne!(h0, chain_hash(&[0u8; 32], &altered));
    }

    #[test]
    fn rollback_plan_undoes_post_cut_writes_keeping_earliest_per_path() {
        let s = Session::new();
        // Helper: a write observation carrying path + snapshot, as the kernel logs it.
        let write = |intent: &str, path: &str, snap: Option<&str>| {
            let payload = json!({ "path": path, "written": true, "snapshot": snap });
            Event::tool_obs(&s, &Observation::ok(intent, payload), TrustLabel::Tool)
        };
        let cut = Event::user_message(&s, "second request");
        let cut_id = cut.id;
        let events = vec![
            Event::user_message(&s, "first request"),
            write("a", "src/lib.rs", None), // created before the cut — not undone
            cut,
            write("b", "src/lib.rs", Some("SNAP1")), // first post-cut write to lib.rs
            write("c", "src/main.rs", None),         // main.rs created after cut → delete
            write("d", "src/lib.rs", Some("SNAP2")), // later write to lib.rs → ignored
        ];
        let plan = rollback_plan(&events, cut_id);
        assert_eq!(
            plan,
            vec![
                FileRollback {
                    path: "src/lib.rs".into(),
                    snapshot: Some("SNAP1".into())
                },
                FileRollback {
                    path: "src/main.rs".into(),
                    snapshot: None
                },
            ],
            "earliest post-cut write per path; pre-cut writes untouched"
        );
        // Cut not found → nothing to roll back.
        assert!(rollback_plan(&events, Ulid::new()).is_empty());
    }

    #[test]
    fn rollback_plan_deduplicates_raw_aliases_of_one_physical_path() {
        let s = Session::new();
        let write = |intent: &str, path: &str, snap: &str| {
            let payload = json!({ "path": path, "written": true, "snapshot": snap });
            Event::tool_obs(&s, &Observation::ok(intent, payload), TrustLabel::Tool)
        };
        let cut = Event::user_message(&s, "rewind here");
        let cut_id = cut.id;
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let absolute = format!("{}/src/events.rs", env!("CARGO_MANIFEST_DIR"));
        let relative_alias = "src/./events.rs";
        let events = vec![
            cut,
            write("first", relative_alias, "BEFORE_FIRST_WRITE"),
            write("second", &absolute, "AFTER_FIRST_WRITE"),
        ];

        let plan = rollback_plan_in(&events, cut_id, workspace);
        assert_eq!(plan.len(), 1, "one physical target must be restored once");
        assert_eq!(plan[0].path, relative_alias);
        assert_eq!(plan[0].snapshot.as_deref(), Some("BEFORE_FIRST_WRITE"));
    }

    #[test]
    fn fork_branches_a_prefix_into_a_new_independent_session() {
        use futures::executor::block_on;
        let log = InMemoryLog::new();
        let s = Session::new();
        block_on(log.append(Event::user_message(&s, "one"))).unwrap();
        let cut = block_on(log.append(Event::user_message(&s, "two"))).unwrap();
        block_on(log.append(Event::model_text(&s, "answer to two"))).unwrap();

        // Fork *before* the second user message: the branch keeps only "one".
        let new_id = block_on(log.fork(s.id, cut.id)).unwrap();
        assert_ne!(new_id, s.id, "fork is a new session");

        let branch = block_on(log.events(new_id));
        assert_eq!(branch.len(), 1, "prefix before the cut event only");
        assert_eq!(
            branch[0].payload.get("text").and_then(Value::as_str),
            Some("one")
        );
        assert_eq!(
            branch[0].session_id, new_id,
            "events re-homed onto the branch"
        );

        assert_eq!(block_on(log.events(s.id)).len(), 3);

        let msgs = project_messages(&branch);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "one");

        let original = block_on(log.events(s.id));
        assert!(
            branch[0].ts >= original[0].ts,
            "fork event ts ({}) must be >= the original's ({})",
            branch[0].ts,
            original[0].ts
        );
    }

    #[test]
    fn in_memory_log_chains_each_event_off_the_previous() {
        use futures::executor::block_on;
        let log = InMemoryLog::new();
        let s = Session::new();
        let e1 = block_on(log.append(Event::model_text(&s, "one"))).unwrap();
        let e2 = block_on(log.append(Event::model_text(&s, "two"))).unwrap();
        // First event links off the genesis zero-hash; the second links off the
        // first's computed hash (not off zero).
        assert_eq!(e1.prev_hash, [0u8; 32]);
        assert_eq!(e2.prev_hash, chain_hash(&e1.prev_hash, &e1));
        assert_ne!(e2.prev_hash, [0u8; 32]);
    }

    #[test]
    fn ordered_model_event_replays_provider_state_without_flattening() {
        use crate::types::{ProviderState, ReasoningPart};

        let session = Session::new();
        let state = ProviderState {
            protocol: crate::provider::Protocol::GeminiInteractions,
            kind: "thought-signature".into(),
            value: json!({"signature": "opaque-signed-value"}),
        };
        let message = ModelMessage {
            role: crate::types::Role::Assistant,
            parts: vec![
                ContentPart::Reasoning(ReasoningPart {
                    text: Some("summary".into()),
                    provider_state: vec![state.clone()],
                }),
                ContentPart::ToolCall(ToolCallPart {
                    id: "call-1".into(),
                    tool: "fs.read".into(),
                    args: json!({"path": "a"}),
                    provider_state: vec![state],
                }),
            ],
            trust: None,
        };
        let model_event = Event::model_message(&session, &message);
        assert_eq!(model_event.kind, EventKind::ModelMessage);
        assert!(!format!("{model_event:?}").contains("opaque-signed-value"));
        let observation = Observation::ok("call-1", json!({"content": "a"}));
        let events = vec![
            model_event,
            Event::tool_obs(&session, &observation, TrustLabel::Tool),
        ];

        let projected = project_ordered_messages(&events);
        assert_eq!(projected[0], message);
        let ContentPart::Reasoning(reasoning) = &projected[0].parts[0] else {
            panic!("reasoning part order changed");
        };
        assert_eq!(
            reasoning.provider_state[0].value["signature"],
            "opaque-signed-value"
        );
    }
}
