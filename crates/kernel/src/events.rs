//! Append-only event log — the single source of truth (P3, Vol 3 §3).
//! State is a projection of the log; the kernel is the only writer.

use crate::errors::KernelError;
use crate::types::{
    ContentPart, Decision, Message, ModelMessage, Observation, Session, TextPart, ToolCallPart,
    ToolIntent, ToolResultPart, TrustLabel,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Mutex;
use ulid::Ulid;

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
    Compaction,
    Session,
    /// Full reasoning/thinking content for a turn — logged for complete
    /// transparency/audit (P3/P7), even though it's excluded from the
    /// conversation history sent back to the model.
    ModelReasoning,
    /// A processed interrupt (steer applied / turn cancelled) — audit trail
    /// (Vol 3's Interrupt kind). Steer text ALSO logs as `user.message`, so
    /// projection ignores this kind entirely.
    Interrupt,
    /// A memory write/update/forget/pin (D1). Payload = the `memory` crate's
    /// `MemoryOp`, kept as opaque JSON here — the kernel doesn't parse it.
    MemoryWrite,
    ContextFileLoaded,
    ContextFileBlocked,
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
            EventKind::Compaction => "context.compaction",
            EventKind::Session => "session",
            EventKind::ModelReasoning => "model.reasoning",
            EventKind::Interrupt => "interrupt",
            EventKind::MemoryWrite => "memory.write",
            EventKind::ContextFileLoaded => "context.file_loaded",
            EventKind::ContextFileBlocked => "context.file_blocked",
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
            "context.compaction" => EventKind::Compaction,
            "session" => EventKind::Session,
            "model.reasoning" => EventKind::ModelReasoning,
            "interrupt" => EventKind::Interrupt,
            "memory.write" => EventKind::MemoryWrite,
            "context.file_loaded" => EventKind::ContextFileLoaded,
            "context.file_blocked" => EventKind::ContextFileBlocked,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Provenance {
    pub source: String,
}

/// One typed, hash-chained record. `prev_hash` makes the log tamper-evident
/// (Vol 3 §3) — the SHA-256 link is computed by [`chain_hash`].
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
        } else {
            debug.field("payload", &self.payload);
        }
        debug
            .field("trust", &self.trust)
            .field("provenance", &self.provenance)
            .field("prev_hash", &self.prev_hash)
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
            ts: now_ts(),
        }
    }

    /// The user's message (or steering injection). Logged so the conversation
    /// is fully reconstructable from the log — without it, resume/replay would
    /// have the model's answers but not the prompts that produced them.
    pub fn user_message(s: &Session, text: &str) -> Self {
        Self::new(
            s,
            EventKind::UserMessage,
            json!({ "text": text }),
            TrustLabel::User,
        )
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
    /// so downstream layers can treat fetched content as untrusted (P7).
    pub fn tool_obs(s: &Session, o: &Observation, trust: TrustLabel) -> Self {
        let payload = serde_json::to_value(o).unwrap_or(Value::Null);
        Self::new(s, EventKind::ToolObs, payload, trust)
    }

    /// A memory mutation (D1). `op` is the memory crate's `MemoryOp` JSON —
    /// opaque here; the projection rebuilds from it.
    pub fn memory_write(s: &Session, op: Value) -> Self {
        Self::new(s, EventKind::MemoryWrite, op, TrustLabel::Memory)
    }

    pub fn context_file(s: &Session, path: &str, content: &str, blocked: bool) -> Self {
        Self::new(
            s,
            if blocked {
                EventKind::ContextFileBlocked
            } else {
                EventKind::ContextFileLoaded
            },
            json!({ "path": path, "content": content }),
            TrustLabel::Workspace,
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
        // Persist the summary text so a resumed session reconstructs the exact
        // working set the model saw (K12), not a re-derived one.
        Self::new(
            s,
            EventKind::Compaction,
            json!({ "before_tokens": before_tokens, "after_tokens": after_tokens, "summary": summary }),
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

/// The log interface. Append computes the hash chain; replay/projection read it.
#[async_trait]
pub trait EventLog: Send + Sync {
    async fn append(&self, e: Event) -> Result<Event, KernelError>;
    async fn events(&self, session: Ulid) -> Vec<Event>;
    /// Every session in the log, newest activity first — for the resume picker.
    /// Default: none (in-memory/ephemeral logs need not implement it).
    async fn sessions(&self) -> Vec<SessionMeta> {
        Vec::new()
    }

    /// Fork a session into a new branch that shares its history *before*
    /// `at_event` (§18.4 time-travel). The prefix events are re-appended under a
    /// fresh session id — a new, independently hash-valid chain — so the original
    /// session is never mutated (append-only holds) and the fork can be continued
    /// on its own. Returns the new session id. This is what makes rewind
    /// non-destructive: you branch off a past point instead of erasing the future.
    ///
    /// `at_event` is a *cut before*: the new session contains every event that
    /// preceded it, and none from `at_event` onward. The default impl reconstructs
    /// the prefix via [`Self::events`] + [`Self::append`], so any backend gets a
    /// correct fork for free; a store may override for efficiency.
    async fn fork(&self, session: Ulid, at_event: Ulid) -> Result<Ulid, KernelError> {
        let events = self.events(session).await;
        let idx = cut_index(&events, at_event).ok_or_else(|| {
            KernelError::Log(format!("event {at_event} not in session {session}"))
        })?;
        let new_id = Ulid::new();
        // Stamp clones with fork time, not the original timestamps (K17):
        // the /resume picker sorts sessions by newest event, and a branch
        // carrying old timestamps sinks to the bottom the moment it's made.
        // A tiny monotonic increment preserves intra-fork order for any
        // consumer that sorts by ts (storage order itself is by rowid).
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

/// Position of `at_event` within an ordered event slice — the cut point for
/// rewind/fork. Everything at index `< cut_index` is the retained prefix;
/// everything from the returned index onward is the discarded (or rolled-back)
/// future. `None` if the event isn't in the slice.
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

/// Compute the file-level rollback that returns the workspace to its state at
/// the cut point — i.e. undo every file write logged from `at_event` onward
/// (§18.4). Walks the write-family tool observations (those carrying a `path`
/// plus a `snapshot` field) that occur at/after the cut and, for each path,
/// keeps the EARLIEST one: its `snapshot` is that file's content *before* the
/// first post-cut write, which is exactly its state at the cut. `None` snapshot
/// ⇒ the file didn't exist yet, so rewinding deletes it. One entry per path, so
/// the result is order-independent to apply.
///
/// The event log holds the full tool result (P3), including the snapshot id the
/// sandbox returns on every write — so this reads purely from the log, no
/// separate write journal. If `at_event` isn't found, returns an empty plan
/// (nothing to undo) rather than erroring.
pub fn rollback_plan(events: &[Event], at_event: Ulid) -> Vec<FileRollback> {
    let Some(idx) = cut_index(events, at_event) else {
        return Vec::new();
    };
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
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
        if !seen.insert(path) {
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

/// In-memory log for Phase 0. The SQLite WAL + FTS5 backend (Vol 3 §3) is a
/// drop-in replacement behind this same trait.
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

/// The canonical tamper-evident link: SHA-256 over
/// (prev_hash ‖ kind ‖ session ‖ payload ‖ ts). Any change to a past event
/// breaks every subsequent hash. This is the single source of truth for the
/// chain — persistence backends (the SQLite log) call this exact function so
/// an in-memory and an on-disk log produce identical hashes for the same
/// events (Vol 3 §3). `id`, `parent_id`, and `provenance` are intentionally
/// excluded so the chain covers content and ordering, not storage bookkeeping.
pub fn chain_hash(prev: &[u8; 32], e: &Event) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(prev);
    h.update(e.kind.as_str().as_bytes());
    h.update(e.session_id.to_string().as_bytes());
    h.update(e.payload.to_string().as_bytes());
    h.update(e.ts.to_le_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

/// Reconstruct the conversation as `Vec<Message>` from a session's events — the
/// projection P3 promises ("state is a projection of the log"). A model turn's
/// text + tool intents collapse into one assistant message, followed by its
/// tool-result messages. The system prompt is omitted (regenerated fresh each
/// run); reasoning / policy / compaction events are skipped (scratch/governance,
/// not conversation). This is what a resumed session is rebuilt from.
pub fn project_messages(events: &[Event]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    let mut text = String::new();
    let mut intents: Vec<ToolIntent> = Vec::new();
    let mut assistant_open = false;
    let mut canonical_call_ids = std::collections::HashSet::new();

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
                // An Esc-interrupted turn logs the prompt, then the re-send logs
                // it again with no assistant turn in between (K20) — collapse
                // consecutive identical user turns so resume shows one.
                let duplicate = matches!(
                    out.last(),
                    Some(m) if matches!(m.role, crate::types::Role::User) && m.content == t
                );
                if !duplicate {
                    out.push(Message::user(t));
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
                    out.push(if message.role == crate::types::Role::Assistant {
                        Message::assistant_calls(content, calls)
                    } else {
                        Message::new(message.role, content)
                    });
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
                let content = e
                    .payload
                    .get("payload")
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                out.push(Message::tool_result(id, content));
            }
            EventKind::Compaction => {
                canonical_call_ids.clear();
                // A Full compaction carries a summary: replay the compacted view
                // the live session actually saw — collapse everything so far into
                // the summary — instead of re-inflating the full pre-compaction
                // history (completes K12). Prune-only compactions carry no summary
                // and are skipped (their messages replay verbatim, spill-bounded).
                if let Some(summary) = e.payload.get("summary").and_then(Value::as_str) {
                    if !summary.trim().is_empty() {
                        out.clear();
                        text.clear();
                        intents.clear();
                        assistant_open = false;
                        out.push(Message::system(summary));
                    }
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
    let mut out = Vec::new();
    let mut assistant_parts = Vec::new();
    let mut canonical_call_ids = std::collections::HashSet::new();

    fn flush(out: &mut Vec<ModelMessage>, parts: &mut Vec<ContentPart>) {
        if !parts.is_empty() {
            out.push(ModelMessage {
                role: crate::types::Role::Assistant,
                parts: std::mem::take(parts),
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
                let duplicate = out.last().is_some_and(|message| {
                    message.role == crate::types::Role::User
                        && matches!(
                            message.parts.as_slice(),
                            [ContentPart::Text(part)] if part.text == text
                        )
                });
                if !duplicate {
                    out.push(Message::user(text).ordered());
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
                let content = event
                    .payload
                    .get("payload")
                    .map(Value::to_string)
                    .unwrap_or_default();
                out.push(ModelMessage {
                    role: crate::types::Role::Tool,
                    parts: vec![ContentPart::ToolResult(ToolResultPart {
                        tool_call_id: id.to_string(),
                        content,
                        provider_state: Vec::new(),
                    })],
                });
            }
            EventKind::Compaction => {
                canonical_call_ids.clear();
                if let Some(summary) = event.payload.get("summary").and_then(Value::as_str) {
                    if !summary.trim().is_empty() {
                        out.clear();
                        assistant_parts.clear();
                        out.push(Message::system(summary).ordered());
                    }
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
                });
            }
        }
    }
    out
}

/// Close any tool call left unanswered — the case where a session was
/// interrupted (Esc/crash) *between* logging a `model.tool_intent` and its
/// `tool.observation`. The projected assistant message then carries a
/// `tool_calls` entry with no matching tool result, and every subsequent turn of
/// the resumed session is rejected by the provider (400: an assistant tool_call
/// must be followed by a tool result) — resume is permanently bricked. We
/// synthesize a `[interrupted]` tool result for each dangling call so the
/// history is a valid request again. Placed right after the assistant's real
/// results, before the next message.
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

/// Recompute the hash chain over `events` (in append order) and confirm each
/// event's stored `prev_hash` equals the running hash of everything before it.
/// This actually *enforces* the tamper-evidence the log claims (Vol 3 §3): any
/// altered payload/kind/timestamp/order in event *i* breaks event *i+1*'s link.
/// The chain is global (across sessions), so pass the full log in append order,
/// not a single session's slice.
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
        let loaded =
            Event::context_file(&session, "sub/AGENTS.md", "use quotes-and-hyphens", false);
        assert_eq!(loaded.kind, EventKind::ContextFileLoaded);
        assert_eq!(loaded.trust, TrustLabel::Workspace);
        assert_eq!(loaded.payload["path"], "sub/AGENTS.md");
        let blocked = Event::context_file(&session, "CLAUDE.md", "[blocked context file]", true);
        assert_eq!(blocked.kind, EventKind::ContextFileBlocked);
        for kind in [EventKind::ContextFileLoaded, EventKind::ContextFileBlocked] {
            assert_eq!(EventKind::parse(kind.as_str()), Some(kind));
        }
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
    fn resume_consumes_compaction_summary_instead_of_full_history() {
        let s = Session::new();
        let events = vec![
            Event::user_message(&s, "first task"),
            Event::model_text(&s, "did the first thing"),
            Event::user_message(&s, "second task"),
            // A Full compaction fired here, summarizing everything above.
            Event::compaction(&s, 1000, 200, Some("HANDOFF: goal + progress")),
            Event::user_message(&s, "third task"),
        ];
        let msgs = project_messages(&events);
        // Pre-compaction history collapses into the summary; post-compaction stays.
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
    fn projection_collapses_the_duplicate_prompt_an_interrupt_leaves_behind() {
        use crate::types::Role;
        let s = Session::new();
        // Esc mid-turn: the prompt was logged before the turn, then re-sent —
        // logged again with nothing in between (K20).
        let events = vec![
            Event::user_message(&s, "build the feature"),
            Event::user_message(&s, "build the feature"),
            Event::model_text(&s, "on it"),
            // A *later* identical prompt after an assistant turn is legitimate.
            Event::user_message(&s, "build the feature"),
        ];
        let msgs = project_messages(&events);
        let users: Vec<_> = msgs.iter().filter(|m| m.role == Role::User).collect();
        assert_eq!(
            users.len(),
            2,
            "consecutive duplicate collapsed, later repeat kept: {msgs:?}"
        );
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
        // Regression against the old DefaultHasher placeholder, which only
        // filled the first 8 of 32 bytes (the rest stayed zero).
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
        // Different payload → different hash.
        let mut altered = base.clone();
        altered.payload = json!({ "text": "HELLO" });
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

        // The original session is untouched (append-only preserved).
        assert_eq!(block_on(log.events(s.id)).len(), 3);

        // Projecting the branch yields the single retained user turn.
        let msgs = project_messages(&branch);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "one");

        // K17: fork clones are stamped with fork time, not the originals' —
        // so the fresh branch sorts to the TOP of a newest-first session list.
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
