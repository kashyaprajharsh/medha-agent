//! The multi-turn kernel loop: models propose blocks and the kernel validates,
//! authorizes, verifies, and executes them.

use crate::context::ContextEngine;
use crate::cut_off::GoOn;
use crate::errors::KernelError;
use crate::events::{Event, EventKind, EventLog};
use crate::executor::Executor;
use crate::provider::{InputTokenCount, Provider, TokenAccountingMode, TokenCountQuality};
use crate::request_images::{Sending, Sent};
use crate::sink::StreamSink;
use crate::types::{
    BlastRadius, Block, CompiledContext, ContentPart, Message, ModelMessage, Observation,
    ReasoningPart, Role, Session, TextPart, ToolCallPart, ToolCategory, ToolIntent, TrustLabel,
};
use futures::stream::{self, StreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn context_has_media(context: &CompiledContext) -> bool {
    context.ordered_messages().iter().any(|message| {
        message
            .parts
            .iter()
            .any(|part| matches!(part, ContentPart::Media(_)))
    })
}

/// Default cap on tool calls executed concurrently within one turn.
/// Overridable via `[budget] max_parallel_tools` in `medha.lock` or
/// `MEDHA_MAX_PARALLEL_TOOLS`, or per session via [`Kernel::with_max_parallel_tools`].
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 16;

fn approval_detail(intent: &ToolIntent) -> String {
    let s = |k: &str| intent.args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match intent.tool.as_str() {
        "shell.exec" => format!("$ {}", s("command")),
        // One tool, three shapes: show the operator what this call actually does.
        "edit" if intent.args.get("content").is_some() => {
            format!("write {} ({} bytes)", s("path"), s("content").len())
        }
        "edit" if intent.args.get("edits").is_some() => format!(
            "edit {} ({} changes)",
            s("path"),
            intent.args["edits"].as_array().map_or(0, Vec::len)
        ),
        "edit" => {
            let old: String = s("old_string").chars().take(120).collect();
            let new: String = s("new_string").chars().take(120).collect();
            format!("edit {}\n- {}\n+ {}", s("path"), old, new)
        }
        _ => {
            let a: String = intent.args.to_string().chars().take(200).collect();
            format!("{} {}", intent.tool, a)
        }
    }
}

/// The auto-approve scope key for the human gate: the tool plus its most
/// salient argument, so "always allow" is scoped to *this* action — approving
/// `rm -rf build/` doesn't then auto-approve every future `shell.exec`. Falls
/// back to the bare tool name for tools with no obvious identifying arg.
fn approval_key(intent: &ToolIntent) -> String {
    match salient_arg(intent) {
        Some(a) => format!("{}: {a}", intent.tool),
        None => intent.tool.clone(),
    }
}

/// The argument worth naming when identifying a call — the file, command or URL
/// it acts on. One definition, so an approval key and a live status line never
/// disagree about which call they are describing.
fn salient_arg(intent: &ToolIntent) -> Option<&str> {
    ["command", "path", "url"]
        .iter()
        .find_map(|key| intent.args.get(*key).and_then(|value| value.as_str()))
}

const MAX_HOOK_PAYLOAD_BYTES: usize = 32 * 1024;
const MAX_HOOK_AUDIT_REASON_BYTES: usize = 1024;

fn sensitive_hook_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "authorization",
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "cookie",
        "credential",
        "provider_state",
        "signature",
    ]
    .iter()
    .any(|part| key.contains(part))
}

/// Clone untrusted event data into a bounded, recursively redacted hook view.
/// This is deliberately done by the kernel rather than trusting an extension
/// transport to remember which provider/tool fields can contain credentials.
fn hook_safe_value(value: &serde_json::Value) -> serde_json::Value {
    fn walk(value: &serde_json::Value, remaining: &mut usize, depth: usize) -> serde_json::Value {
        if *remaining == 0 || depth > 16 {
            return serde_json::Value::String("<truncated>".into());
        }
        match value {
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
                *remaining = remaining.saturating_sub(value.to_string().len());
                value.clone()
            }
            serde_json::Value::String(text) => {
                let limit = (*remaining).min(text.len());
                let mut end = limit;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                *remaining = remaining.saturating_sub(end);
                if end == text.len() {
                    serde_json::Value::String(text.clone())
                } else {
                    serde_json::Value::String(format!("{}<truncated>", &text[..end]))
                }
            }
            serde_json::Value::Array(values) => serde_json::Value::Array(
                values
                    .iter()
                    .take(128)
                    .map(|value| walk(value, remaining, depth + 1))
                    .collect(),
            ),
            serde_json::Value::Object(values) => {
                let mut safe = serde_json::Map::new();
                for (key, value) in values.iter().take(128) {
                    if *remaining == 0 {
                        break;
                    }
                    *remaining = remaining.saturating_sub(key.len());
                    safe.insert(
                        key.clone(),
                        if sensitive_hook_key(key) {
                            serde_json::Value::String("<redacted>".into())
                        } else {
                            walk(value, remaining, depth + 1)
                        },
                    );
                }
                serde_json::Value::Object(safe)
            }
        }
    }

    let mut remaining = MAX_HOOK_PAYLOAD_BYTES;
    walk(value, &mut remaining, 0)
}

fn bounded_hook_reason(reason: &str) -> String {
    if reason.len() <= MAX_HOOK_AUDIT_REASON_BYTES {
        return reason.to_string();
    }
    let mut end = MAX_HOOK_AUDIT_REASON_BYTES;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}<truncated>", &reason[..end])
}

fn attach_discovered_context(
    observation: &mut Observation,
    discovered: &crate::context::DiscoveredContext,
) {
    let attachment = serde_json::json!({
        "path": discovered.path,
        "trust": discovered.trust.as_str(),
        "blocked": discovered.blocked,
        "content": discovered.content,
    });
    if let Some(payload) = observation.payload.as_object_mut() {
        payload.insert("project_context".into(), attachment);
    } else {
        let result = std::mem::take(&mut observation.payload);
        observation.payload = serde_json::json!({
            "result": result,
            "project_context": attachment,
        });
    }
}

fn successful_observed_path(observation: &Observation) -> Option<&std::path::Path> {
    if !matches!(observation.status, crate::types::ObsStatus::Ok) {
        return None;
    }
    observation
        .payload
        .get("path")
        .and_then(|value| value.as_str())
        .map(std::path::Path::new)
}

fn same_legacy_message(left: &Message, right: &Message) -> bool {
    left.role == right.role
        && left.attachments == right.attachments
        && left.content == right.content
        && left.trust == right.trust
        && left.tool_call_id == right.tool_call_id
        && left.tool_calls.len() == right.tool_calls.len()
        && left
            .tool_calls
            .iter()
            .zip(&right.tool_calls)
            .all(|(a, b)| a.id == b.id && a.tool == b.tool && a.args == b.args)
}

/// Deliberately lossy control/UI projection. Exact replay always keeps the
/// corresponding `ModelMessage`; this view exists only while the context
/// engine and public session API still accept legacy messages.
fn legacy_views(message: &ModelMessage) -> Vec<Message> {
    match message.role {
        Role::Tool => {
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
            if results.is_empty() {
                let mut legacy = Message::new(Role::Tool, fallback);
                legacy.trust = message.trust;
                vec![legacy]
            } else {
                results
            }
        }
        _ => {
            let text = crate::types::legacy_text(message);
            let mut calls = Vec::new();
            for part in &message.parts {
                if let ContentPart::ToolCall(part) = part {
                    calls.push(ToolIntent {
                        id: part.id.clone(),
                        tool: part.tool.clone(),
                        args: part.args.clone(),
                    });
                }
            }
            let mut legacy = if message.role == Role::Assistant {
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
    }
}

/// Reuse exact canonical messages retained by a compaction result. Matched by
/// occurrence, not value — identical legacy text can carry different opaque
/// provider state.
fn reconcile_ordered(
    compiled: &[Message],
    ordered: &[ModelMessage],
    source_indices: &[Option<usize>],
) -> Vec<ModelMessage> {
    let valid_map = source_indices.len() == compiled.len();
    compiled
        .iter()
        .enumerate()
        .map(|(output_index, legacy)| {
            let retained = valid_map
                .then(|| source_indices[output_index])
                .flatten()
                .and_then(|source_index| ordered.get(source_index))
                .filter(|canonical| {
                    legacy_views(canonical)
                        .iter()
                        .any(|candidate| same_legacy_message(candidate, legacy))
                });
            retained.cloned().unwrap_or_else(|| legacy.ordered())
        })
        .collect()
}

/// The model's turn when only this text of a reply that stopped early is kept.
#[allow(clippy::type_complexity)]
fn kept_text(
    text: String,
) -> (
    Message,
    ModelMessage,
    Vec<ToolIntent>,
    Option<crate::types::Usage>,
    bool,
) {
    let parts = match text.is_empty() {
        true => Vec::new(),
        false => vec![ContentPart::Text(TextPart {
            text: text.clone(),
            provider_state: Vec::new(),
        })],
    };
    let turn = Message::assistant_calls(text, Vec::new());
    (
        turn,
        message_from_stream_parts(parts),
        Vec::new(),
        None,
        false,
    )
}

/// Leave only what was shown of a continued reply: no repeat, nothing still held.
fn only_shown(seam: &Option<crate::seam::Seam>, text: &mut String, message: &mut ModelMessage) {
    if let Some(seam) = seam {
        crate::seam::without_start(text, seam.hidden());
        crate::seam::without_start_of(message, seam.hidden());
    }
}

fn message_from_stream_parts(parts: Vec<ContentPart>) -> ModelMessage {
    ModelMessage {
        role: Role::Assistant,
        parts,
        trust: None,
    }
}

fn strip_tool_calls(message: &ModelMessage) -> ModelMessage {
    ModelMessage {
        role: message.role.clone(),
        parts: message
            .parts
            .iter()
            .filter(|part| !matches!(part, ContentPart::ToolCall(_)))
            .cloned()
            .collect(),
        trust: message.trust,
    }
}

fn hydrate_ordered_log(
    surface_messages: &[Message],
    projected: Vec<ModelMessage>,
) -> Vec<ModelMessage> {
    let mut hydrated: Vec<ModelMessage> = surface_messages
        .iter()
        .take_while(|message| message.role == Role::System)
        .map(Message::ordered)
        .collect();
    // The event projector has already coalesced only explicit retry
    // identities. Equality here cannot distinguish a deliberate repeat (or
    // its provenance), so every projected admission must remain.
    hydrated.extend(projected);
    hydrated
}

fn completed_control_view(
    message: &ModelMessage,
) -> Result<(String, String, Vec<ToolIntent>), crate::provider::ProviderError> {
    if message.role != Role::Assistant {
        return Err(crate::provider::ProviderError::Decode(
            "provider completed message must have the assistant role".into(),
        ));
    }
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut intents = Vec::new();
    let mut call_ids = std::collections::HashSet::new();
    for part in &message.parts {
        match part {
            ContentPart::Text(part) => text.push_str(&part.text),
            ContentPart::Reasoning(part) => {
                if let Some(summary) = &part.text {
                    reasoning.push_str(summary);
                }
            }
            ContentPart::ToolCall(part) => {
                if !call_ids.insert(&part.id) {
                    return Err(crate::provider::ProviderError::invalid_tool_call(
                        &part.tool,
                        &part.id,
                        "duplicate tool-call id in the response",
                    ));
                }
                intents.push(ToolIntent {
                    id: part.id.clone(),
                    tool: part.tool.clone(),
                    args: part.args.clone(),
                });
            }
            ContentPart::ToolResult(_) => {
                return Err(crate::provider::ProviderError::Decode(
                    "provider completed assistant message contained a tool result".into(),
                ));
            }
            ContentPart::Media(_) => {}
        }
    }
    Ok((text, reasoning, intents))
}

fn charge_stream_bytes(
    total: &mut usize,
    additional: usize,
) -> Result<(), crate::provider::ProviderError> {
    *total = total.checked_add(additional).ok_or_else(|| {
        crate::provider::ProviderError::Decode(
            "provider stream exceeded the kernel byte limit".into(),
        )
    })?;
    if *total > MAX_PROVIDER_STREAM_BYTES {
        return Err(crate::provider::ProviderError::Decode(format!(
            "provider stream exceeded the {} byte kernel limit",
            MAX_PROVIDER_STREAM_BYTES
        )));
    }
    Ok(())
}

async fn wait_for_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

/// Why a session loop stopped — so the surface can tell the user (e.g. which
/// budget ceiling was hit, and that it can be resumed) instead of returning
/// silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished with a text-only turn (task complete).
    Finished,
    /// Required completion checks failed or could not run.
    VerificationFailed,
    /// A budget ceiling was reached (the continuation policy can resume).
    Budget(crate::budgets::BudgetStop),
    /// The surface cancelled the turn; in-flight work settled gracefully and
    /// the returned history is consistent (every intent has an observation).
    Interrupted,
    /// A prompt-submit hook refused the new input before any model call. The
    /// reason reaches the surface through `StreamSink::notice` and the log.
    Blocked,
}

#[path = "hook_points.rs"]
mod hook_points;

/// Application-level ownership, independent of the kernel's storage adapter.
pub trait SessionOwner: Send + Sync {
    fn reserve(&self, session: ulid::Ulid) -> Result<SessionClaim, String>;
    fn adopt(&self, claim: SessionClaim);
}

/// Holds a reservation while an asynchronous replay or fork is prepared.
#[derive(Clone)]
pub struct SessionClaim {
    pub session: ulid::Ulid,
    _guard: Arc<dyn Send + Sync>,
}

impl SessionClaim {
    pub fn new(session: ulid::Ulid, guard: Arc<dyn Send + Sync>) -> Self {
        Self {
            session,
            _guard: guard,
        }
    }
}

impl std::fmt::Debug for SessionClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionClaim")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

pub struct Kernel<P: Provider, L: EventLog> {
    pub provider: Arc<P>,
    pub log: Arc<L>,
    pub executor: Arc<dyn Executor>,
    pub context: Arc<dyn ContextEngine>,
    pub artifacts: Arc<dyn crate::artifacts::ArtifactStore>,
    pub policy: Arc<dyn crate::policy::Policy>,
    pub gate: Arc<dyn crate::gate::HumanGate>,
    pub verifier: Arc<dyn crate::verify::Verifier>,
    hooks: Arc<dyn crate::hooks::HookRunner>,
    /// Reads images for a route whose own model cannot. Without one, such a
    /// route gets a note in each image's place.
    vision: Option<Arc<dyn crate::vision::VisionDescriber>>,
    /// Makes an image smaller when a server refuses it at full size.
    shrinker: Option<Arc<dyn crate::ImageShrinker>>,
    /// This server took images only once they were smaller; later turns start there.
    shrinks_images: AtomicBool,
    /// The longest image side, in pixels, this server is known to take.
    image_side: std::sync::atomic::AtomicU32,
    progressive_context: Option<Arc<dyn crate::context::ProgressiveContext>>,
    max_parallel_tools: usize,
    /// Times a turn waits for a server that is away, after all else is used up.
    server_waits: u32,
    /// (per result, per round) limits set in config; each one unset is worked
    /// out from the model's context window.
    result_limits: (Option<usize>, Option<usize>),
    /// Removed from every tool result before the model reads it.
    secrets: Arc<crate::redact::Secrets>,
    pricing: Option<crate::types::Pricing>,
    /// The model `pricing` was set for; any other is priced from its list price.
    priced_model: Option<String>,
    /// Serializes human-gate prompts: parallel tool dispatch must not pop
    /// several approval cards at once.
    ///
    /// Shared with every kernel derived from this one, because the operator is
    /// shared too. A per-session lock only orders one session's own cards, so a
    /// tree of agents would each hold their own lane and race the single surface
    /// they all prompt through.
    gate_serial: Arc<futures::lock::Mutex<()>>,
    /// Orders overlapping changes across root and child sessions. A place is held
    /// from the change through its saved result, so replay sees them in order.
    lanes: Arc<crate::write_lanes::WriteLanes>,
    leases: Arc<crate::write_lanes::SharedLeases>,
    settle_grace: std::time::Duration,
    /// Sessions whose start hooks already ran in this process, shared with
    /// derived kernels so a resumed tree fires them once.
    started_sessions: Arc<std::sync::Mutex<std::collections::HashSet<ulid::Ulid>>>,
    /// Derived for a sub-agent: agent start/stop hooks describe it instead of
    /// session and prompt hooks.
    sub_agent: bool,
    /// Ownership outlives the tool graph and adapters' destructured startup data.
    session_owner: Option<Arc<dyn SessionOwner>>,
}

/// Tool-result payloads larger than this spill to the artifact store and are
/// replaced in-context by a head and a `read_artifact` reference. Public
/// because the paging tool must size its pages to survive this threshold: a
/// page that spills costs a round trip instead of saving one.
pub const SPILL_THRESHOLD: usize = 40_000;
/// The most one round of tool results may put in front of the model together.
const ROUND_LIMIT: usize = 120_000;
/// How much of a stored result the model sees in place of it: its start, where
/// errors usually begin, and a larger end, where a run says how it finished.
const SPILL_PREVIEW: usize = 2_000;

/// A stored result in brief: its first and last lines, how much was left out,
/// and how to read the rest. The whole text stays in the store, readable later.
fn spilled_preview(content: &str, hash: &str) -> String {
    let head_budget = SPILL_PREVIEW * 2 / 5;
    let mut head = head_budget.min(content.len());
    while !content.is_char_boundary(head) {
        head -= 1;
    }
    if let Some(newline) = content[..head]
        .rfind('\n')
        .filter(|at| *at > head_budget / 2)
    {
        head = newline + 1;
    }
    let mut tail = content
        .len()
        .saturating_sub(SPILL_PREVIEW - head_budget)
        .max(head);
    while !content.is_char_boundary(tail) {
        tail += 1;
    }
    if let Some(newline) = content[tail..]
        .find('\n')
        .filter(|at| *at < SPILL_PREVIEW / 4)
    {
        tail += newline + 1;
    }
    format!(
        "{}\n[… {} of {} bytes not shown, bytes {}–{}. The whole result is stored: read \
         hash=\"{hash}\" with offset (a byte) and length to see any part of it.]\n{}",
        content[..head].trim_end_matches('\n'),
        tail - head,
        content.len(),
        head,
        tail,
        &content[tail..]
    )
}

/// Absolute per-turn ingestion limits. These are deliberately independent of
/// provider/model limits: a broken or adversarial stream must not be able to
/// grow the kernel's in-memory transcript forever.
const MAX_TOOL_INTENTS_PER_TURN: usize = 64;
const MAX_PROVIDER_STREAM_BLOCKS: usize = 262_144;
const MAX_PROVIDER_STREAM_BYTES: usize = 8 * 1024 * 1024;

/// How many times a turn's model stream is retried on a transient provider
/// failure (429 / 5xx / network drop) before giving up.
const MAX_TURN_RETRIES: u32 = 5;
/// How many times a stream that went silent is reopened. Each try waits out the
/// whole idle limit and resends the request, so a provider that is truly
/// stuck costs three waits, not six.
const MAX_STALL_RETRIES: u32 = 2;
const MAX_TOOL_CALL_CORRECTIONS: u32 = 2;

/// First backoff nap between stream retries.
const RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(500);

/// Ceiling on one backoff nap. A rate limit needs tens of seconds to clear, so
/// a sub-second curve spends the entire retry budget inside the window that was
/// refusing us and reports failure before the limit has lifted.
const RETRY_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
/// Bound measure → compact → remeasure so a pathological compressor cannot
/// rewrite the same turn indefinitely.
const MAX_COMPACTION_PASSES: u32 = 3;

/// One change's place among writers: taken once it is approved, kept until its
/// result is saved, so nothing waits on a person's answer.
struct Lane {
    key: String,
    cancel: tokio_util::sync::CancellationToken,
    deadline: Option<tokio::time::Instant>,
    held: std::sync::Mutex<Option<(crate::write_lanes::Held, crate::write_lanes::LeaseShare)>>,
}

/// After a cancel, how long an in-flight tool gets to settle before its future
/// is dropped and an `[interrupted]` observation is synthesized. Dropping is
/// safe here: process trees die with the future (group reaper), and the
/// synthesized observation keeps the intent→observation invariant intact.
const TOOL_SETTLE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Capped exponential backoff with ±25% jitter: 500ms, 1s, 2s, 4s, 8s, …
///
/// The jitter decorrelates the sessions in one agent tree. Children spawned
/// together hit their rate limit together, and an unjittered curve then has
/// them retry in lockstep and be refused in lockstep until the budget is gone.
fn retry_backoff(attempt: u32) -> std::time::Duration {
    let base = RETRY_BASE
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(6))
        .min(RETRY_MAX_BACKOFF);
    let spread = (base.as_millis() / 4) as u64;
    if spread == 0 {
        return base;
    }
    // Centred on `base`, so the average interval stays on the curve.
    let offset = jitter_nanos() % (spread * 2 + 1);
    base.saturating_add(std::time::Duration::from_millis(offset))
        .saturating_sub(std::time::Duration::from_millis(spread))
        .min(RETRY_MAX_BACKOFF)
}

/// How a wait inside a turn ended.
enum Waited {
    Out,
    /// The person stopped the turn.
    Stopped,
    /// The person typed a message: it is not kept waiting.
    Spoken,
}

/// Wait `wait` out, saying the turn is alive every few seconds so no watcher
/// takes the silence for a dead one. A stop or a typed message ends it at once.
async fn waited_out(
    wait: std::time::Duration,
    cancel: &tokio_util::sync::CancellationToken,
    mut heard: Option<crate::interrupts::Activity>,
    sink: &dyn StreamSink,
) -> Waited {
    let until = tokio::time::Instant::now() + wait;
    loop {
        sink.phase_ticked();
        let left = until.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Waited::Out;
        }
        let spoken = async {
            let arrived = match heard.as_mut() {
                Some(heard) => heard.changed().await.is_ok(),
                None => false,
            };
            if !arrived {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(left.min(std::time::Duration::from_secs(5))) => {}
            _ = cancel.cancelled() => return Waited::Stopped,
            // A stop is announced on the same channel; the token tells them apart.
            _ = spoken => return match cancel.is_cancelled() {
                true => Waited::Stopped,
                false => Waited::Spoken,
            },
        }
    }
}

/// A cheap source of spread for [`retry_backoff`]. Jitter needs decorrelation,
/// not unpredictability, so the clock serves and no dependency is needed.
fn jitter_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| u64::from(since.subsec_nanos()))
        .unwrap_or(0)
}

impl<P: Provider, L: EventLog> Kernel<P, L> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<P>,
        log: Arc<L>,
        executor: Arc<dyn Executor>,
        context: Arc<dyn ContextEngine>,
        artifacts: Arc<dyn crate::artifacts::ArtifactStore>,
        policy: Arc<dyn crate::policy::Policy>,
        gate: Arc<dyn crate::gate::HumanGate>,
        verifier: Arc<dyn crate::verify::Verifier>,
    ) -> Self {
        Self {
            provider,
            log,
            executor,
            context,
            artifacts,
            policy,
            gate,
            verifier,
            hooks: Arc::new(crate::hooks::NoHooks),
            vision: None,
            shrinker: None,
            shrinks_images: Default::default(),
            image_side: crate::request_images::SIDE.into(),
            progressive_context: None,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            server_waits: crate::recovery::WAITS,
            result_limits: (None, None),
            secrets: Arc::default(),
            pricing: None,
            priced_model: None,
            gate_serial: Arc::new(futures::lock::Mutex::new(())),
            lanes: Arc::default(),
            leases: Arc::default(),
            settle_grace: TOOL_SETTLE_GRACE,
            started_sessions: Arc::default(),
            sub_agent: false,
            session_owner: None,
        }
    }

    /// A kernel for a sub-session: same provider, log, artifacts, policy and
    /// verifier, but its own executor, context engine and gate. Pricing,
    /// parallelism and the settle window are inherited, not rebuilt.
    pub fn derive(
        &self,
        executor: Arc<dyn Executor>,
        context: Arc<dyn ContextEngine>,
        gate: Arc<dyn crate::gate::HumanGate>,
    ) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            log: Arc::clone(&self.log),
            executor,
            context,
            artifacts: Arc::clone(&self.artifacts),
            policy: Arc::clone(&self.policy),
            gate,
            verifier: Arc::clone(&self.verifier),
            hooks: Arc::clone(&self.hooks),
            vision: self.vision.clone(),
            shrinker: self.shrinker.clone(),
            shrinks_images: Default::default(),
            image_side: crate::request_images::SIDE.into(),
            progressive_context: self.progressive_context.clone(),
            max_parallel_tools: self.max_parallel_tools,
            server_waits: self.server_waits,
            result_limits: self.result_limits,
            secrets: Arc::clone(&self.secrets),
            pricing: self.pricing,
            priced_model: self.priced_model.clone(),
            // Shared, not rebuilt: see the field's own note. A fresh lane here
            // is what let three children pop three cards at one surface.
            gate_serial: Arc::clone(&self.gate_serial),
            lanes: Arc::clone(&self.lanes),
            leases: Arc::clone(&self.leases),
            settle_grace: self.settle_grace,
            started_sessions: Arc::clone(&self.started_sessions),
            sub_agent: true,
            session_owner: self.session_owner.clone(),
        }
    }

    /// Whether this route can carry images itself: the wire contract must
    /// support them and the model must not declare that it cannot see.
    fn route_reads_images(&self) -> Result<bool, KernelError> {
        match self.provider.image_input_mode() {
            crate::ImageInputMode::Text => Ok(false),
            crate::ImageInputMode::Native => {
                if self.provider.protocol().carries_images() {
                    Ok(true)
                } else {
                    Err(KernelError::Provider(format!(
                        "image_input=native cannot be used with the {} protocol",
                        self.provider.protocol().as_str()
                    )))
                }
            }
            crate::ImageInputMode::Auto => Ok(self.provider.protocol().carries_images()
                && self.provider.image_support() != crate::ImageSupport::Unsupported),
        }
    }

    /// Auxiliary model that reads images for a route that cannot.
    pub fn with_vision(mut self, vision: Arc<dyn crate::vision::VisionDescriber>) -> Self {
        self.vision = Some(vision);
        self
    }

    pub fn with_image_shrinker(mut self, shrinker: Arc<dyn crate::ImageShrinker>) -> Self {
        self.shrinker = Some(shrinker);
        self
    }

    /// What a model call costs now: the operator's own rate while the model it
    /// was set for is answering, else the list price of whichever model is.
    fn price(&self) -> Option<crate::types::Pricing> {
        let own_model = self.priced_model.as_deref() == Some(&*self.provider.context_identity());
        let own = self.pricing.filter(|_| own_model);
        own.or_else(|| self.provider.list_price())
    }

    /// Set the operator's own rate, so the governor meters real dollars. It is
    /// a rate for the model in use now, not for one the chat changes to later.
    pub fn with_pricing(mut self, pricing: Option<crate::types::Pricing>) -> Self {
        self.priced_model = pricing.map(|_| self.provider.context_identity());
        self.pricing = pricing;
        self
    }

    /// Replace the deterministic verifier for a derived context. Writer
    /// sub-agents disable the parent's, which would check the wrong tree.
    pub fn with_verifier(mut self, verifier: Arc<dyn crate::verify::Verifier>) -> Self {
        self.verifier = verifier;
        self
    }

    pub fn with_progressive_context(
        mut self,
        progressive_context: Arc<dyn crate::context::ProgressiveContext>,
    ) -> Self {
        self.progressive_context = Some(progressive_context);
        self
    }

    /// Records that `session` already started in an earlier process of the same chat.
    pub fn continue_session(&self, session: ulid::Ulid) {
        if let Ok(mut started) = self.started_sessions.lock() {
            started.insert(session);
        }
    }

    /// Install the host-side hook runner. Hooks can narrow or escalate a tool
    /// decision, but this boundary exposes no way to register a model tool.
    /// Applies hook enable/disable decisions to this running process.
    pub fn reload_hooks(&self) -> Vec<String> {
        self.hooks.reload()
    }

    pub fn with_hooks(mut self, hooks: Arc<dyn crate::hooks::HookRunner>) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn with_session_owner(mut self, owner: Arc<dyn SessionOwner>) -> Self {
        self.session_owner = Some(owner);
        self
    }

    pub fn session_owner(&self) -> Option<Arc<dyn SessionOwner>> {
        self.session_owner.clone()
    }

    pub fn reserve_session(&self, session: ulid::Ulid) -> Result<Option<SessionClaim>, String> {
        self.session_owner
            .as_ref()
            .map(|owner| owner.reserve(session))
            .transpose()
    }

    pub fn adopt_session(&self, claim: Option<SessionClaim>) {
        if let (Some(owner), Some(claim)) = (&self.session_owner, claim) {
            owner.adopt(claim);
        }
    }

    /// Override the post-cancel tool settle window (tests use a short one).
    pub fn with_settle_grace(mut self, grace: std::time::Duration) -> Self {
        self.settle_grace = grace;
        self
    }

    /// The reply as it is saved, with the files it names that this run changed.
    async fn reply_text(&self, session: &Session, text: &str, since: f64) -> Event {
        let mut event = Event::model_text(session, text);
        let outputs = self.executor.named_outputs(text, since).await;
        if !outputs.is_empty() {
            event.payload["outputs"] = serde_json::Value::Array(outputs);
        }
        event
    }

    /// Common tail for a graceful cancel: hand back steers that never reached
    /// a turn boundary (typed text must not vanish), log the interrupt, and
    /// return the settled history.
    async fn finish_interrupted(
        &self,
        session: &Session,
        messages: Vec<Message>,
        q: &mut crate::interrupts::InterruptQueue,
        sink: &dyn StreamSink,
    ) -> Result<(Vec<Message>, StopReason), KernelError> {
        Self::return_unapplied_steers(q, sink);
        self.log
            .append(Event::interrupt(session, "cancel", None))
            .await
            .ok();
        Ok((messages, StopReason::Interrupted))
    }

    /// Hand back any steers still queued at a session exit (Finished / budget
    /// stop / cancel) — a steer that raced the final turn must reach the
    /// surface, never evaporate.
    fn return_unapplied_steers(q: &mut crate::interrupts::InterruptQueue, sink: &dyn StreamSink) {
        let leftover: Vec<String> = q.drain_steers().into_iter().map(|(text, _)| text).collect();
        if !leftover.is_empty() {
            sink.steers_returned(&leftover);
        }
    }

    /// Store a result too large to send and return its brief in its place. The
    /// full payload is still in the event log, so nothing is lost; only the
    /// live context shrinks. Callers decide what is too large.
    async fn maybe_spill(&self, content: String) -> String {
        match Arc::clone(&self.artifacts)
            .put_async(content.as_bytes().to_vec())
            .await
        {
            Ok(hash) => spilled_preview(&content, &hash),
            Err(_) => content, // spill failed → keep full rather than lose data
        }
    }

    /// Secrets to remove from every tool result before the model reads it.
    pub fn with_secrets(mut self, secrets: crate::redact::Secrets) -> Self {
        self.secrets = Arc::new(secrets);
        self
    }

    /// Set limits for what one tool result, and one round of them, may put in
    /// front of the model; each `None` is worked out from its context window.
    pub fn with_result_limits(
        mut self,
        per_result: Option<usize>,
        per_round: Option<usize>,
    ) -> Self {
        self.result_limits = (per_result.map(|n| n.max(1)), per_round.map(|n| n.max(1)));
        self
    }

    /// What one tool result, and one round of them, may put in front of the
    /// model, in bytes: a share of its context window, within bounds. A model
    /// with an unknown window gets the upper bounds. Bytes stand in for
    /// tokens at four to one; this is a budget, not the exact fit, which the
    /// context engine counts before every request.
    fn result_limits(&self) -> (usize, usize) {
        let window = self
            .provider
            .context_window()
            .map(|tokens| tokens as usize * 4);
        let share = |percent: usize, (low, high): (usize, usize)| {
            window.map_or(high, |bytes| (bytes * percent / 100).clamp(low, high))
        };
        let (per_result, per_round) = self.result_limits;
        (
            per_result.unwrap_or_else(|| share(15, (8_000, SPILL_THRESHOLD))),
            per_round.unwrap_or_else(|| share(30, (16_000, ROUND_LIMIT))),
        )
    }

    /// Settle what the model reads for a result before it is saved: the tool's
    /// own view, or the result itself, with anything the kernel added to it.
    /// When that is more than one result may take, or more than this round has
    /// left, the whole is stored and the model reads its start and end and
    /// where it is kept. Returns that place. What is decided here is saved
    /// with the result, so a reopened chat reads exactly the same.
    async fn fit_for_model(&self, obs: &mut Observation, room: &mut usize) -> Option<String> {
        let mut viewed = obs.model_text.is_some();
        let mut text = match obs.model_text.take() {
            Some(mut view) => {
                for key in ["adjusted", "project_context"] {
                    match obs.payload.get(key) {
                        Some(serde_json::Value::String(note)) => {
                            view.push_str(&format!("\n{key}: {note}"))
                        }
                        Some(value) => view.push_str(&format!("\n{key}: {value}")),
                        None => {}
                    }
                }
                view
            }
            None => {
                let mut visible = obs.payload.clone();
                crate::events::strip_private_observation_fields(&mut visible);
                visible.to_string()
            }
        };
        // The record keeps what the tool returned; the model reads it without
        // secrets, and so does anything stored for it to page.
        if let std::borrow::Cow::Owned(clean) = self.secrets.redact(&text) {
            text = clean;
            viewed = true;
        }
        let per_result = self.result_limits().0;
        // A result no bigger than a preview would only grow by being stored.
        let small = text.len() <= SPILL_PREVIEW * 3 / 2;
        if small || (text.len() <= per_result && text.len() <= *room) {
            *room = room.saturating_sub(text.len());
            obs.model_text = viewed.then_some(text);
            return None;
        }
        match Arc::clone(&self.artifacts)
            .put_async(text.as_bytes().to_vec())
            .await
        {
            Ok(hash) => {
                let preview = spilled_preview(&text, &hash);
                *room = room.saturating_sub(preview.len());
                obs.model_text = Some(preview);
                Some(hash)
            }
            // Nowhere to keep it: the model reads it whole rather than lose it.
            Err(_) => {
                *room = room.saturating_sub(text.len());
                obs.model_text = viewed.then_some(text);
                None
            }
        }
    }

    /// Apply the live-context spill policy to both request representations —
    /// rewriting only the legacy view left the provider-facing request unbounded.
    /// Tool-call identity and opaque state are preserved; only the body is spilled.
    async fn spill_hydrated_tool_results(
        &self,
        messages: &mut [Message],
        ordered_messages: &mut [ModelMessage],
    ) {
        // Results saved with their fit already decided are within this; older
        // ones, and any too large for a smaller model since, are fitted here.
        let limit = self.result_limits().0;
        for message in messages {
            if message.role != Role::Tool {
                continue;
            }
            // Results saved before secrets were removed have them removed here.
            if let std::borrow::Cow::Owned(clean) = self.secrets.redact(&message.content) {
                message.content = clean;
            }
            if message.content.len() > limit {
                message.content = self.maybe_spill(std::mem::take(&mut message.content)).await;
            }
        }
        for message in ordered_messages {
            if message.role != Role::Tool {
                continue;
            }
            for part in &mut message.parts {
                if let ContentPart::ToolResult(result) = part
                    && let std::borrow::Cow::Owned(clean) = self.secrets.redact(&result.content)
                {
                    result.content = clean;
                }
                match part {
                    ContentPart::ToolResult(result) if result.content.len() > limit => {
                        result.content =
                            self.maybe_spill(std::mem::take(&mut result.content)).await;
                    }
                    ContentPart::Text(text) if text.text.len() > limit => {
                        text.text = self.maybe_spill(std::mem::take(&mut text.text)).await;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Override the per-turn concurrency cap.
    /// How many times a turn waits for a model server that is away; 0 never waits.
    pub fn with_server_waits(mut self, waits: u32) -> Self {
        self.server_waits = waits;
        self
    }

    pub fn with_max_parallel_tools(mut self, n: usize) -> Self {
        self.max_parallel_tools = n.clamp(1, MAX_TOOL_INTENTS_PER_TURN);
        self
    }

    /// Execute one already-admitted intent while preserving the kernel's
    /// cancellation invariant: a call either settles with its real result or
    /// gets one synthesized interrupted observation. Calls that have not
    /// started when cancellation arrives are never dispatched.
    #[allow(clippy::too_many_arguments)]
    async fn execute_admitted(
        &self,
        session: &Session,
        intent: ToolIntent,
        web_tainted: bool,
        cancel: tokio_util::sync::CancellationToken,
        wall_deadline: Option<tokio::time::Instant>,
        settle_deadline: Arc<std::sync::OnceLock<tokio::time::Instant>>,
        lane: Option<&Lane>,
        sink: &dyn StreamSink,
    ) -> (
        String,
        String,
        Observation,
        Option<crate::context::DiscoveredContext>,
    ) {
        let deadline_elapsed =
            wall_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline);
        let obs = if deadline_elapsed {
            Observation::error(
                &intent.id,
                "[interrupted] task wall-clock deadline elapsed before tool execution started",
            )
        } else if cancel.is_cancelled() {
            Observation::error(
                &intent.id,
                "[interrupted] cancelled by user before tool execution started",
            )
        } else {
            // The dispatch future is never dropped by cancellation itself: the
            // tool gets TOOL_SETTLE_GRACE to finish and keep its real
            // observation. Only after the grace is it dropped and replaced by
            // a synthetic result, preserving intent → observation.
            let fut = self.dispatch_one(session, &intent, web_tainted, &cancel, lane, sink);
            tokio::pin!(fut);
            tokio::select! {
                biased;
                _ = wait_for_deadline(wall_deadline) => {
                    Observation::error(
                        &intent.id,
                        "[interrupted] task wall-clock deadline elapsed during tool execution",
                    )
                }
                obs = &mut fut => obs,
                _ = cancel.cancelled() => {
                    let shared_settle_deadline = *settle_deadline
                        .get_or_init(|| tokio::time::Instant::now() + self.settle_grace);
                    let settle_deadline = wall_deadline
                        .map(|deadline| deadline.min(shared_settle_deadline))
                        .unwrap_or(shared_settle_deadline);
                    match tokio::time::timeout_at(settle_deadline, &mut fut).await {
                        Ok(obs) => obs,
                        Err(_) => Observation::error(
                            &intent.id,
                            "[interrupted] cancelled by user; tool did not settle within the grace window",
                        ),
                    }
                }
            }
        };
        // Context discovery is a consequence of a real successful path touch,
        // never of merely asking for a path. Requiring the tool's settled
        // payload to echo the path excludes denied/schema-invalid/failed calls
        // and tools whose `path` argument was not actually used.
        let discovered = match (&self.progressive_context, successful_observed_path(&obs)) {
            (Some(loader), Some(path)) => {
                tokio::select! {
                    biased;
                    _ = wait_for_deadline(wall_deadline) => None,
                    _ = cancel.cancelled() => None,
                    discovered = loader.discover(path) => discovered,
                }
            }
            _ => None,
        };
        (intent.id, intent.tool, obs, discovered)
    }

    /// Run a batch of read-only calls to completion, then hand back every result.
    ///
    /// Draining before persisting anything is load-bearing, not tidiness. The
    /// obvious shape — persist each result as the batch yields it — deadlocks:
    /// `persist_admitted` awaits an append, the event log admits appends through
    /// one fair queue, and the batch's remaining futures are already waiting in
    /// that queue for their own policy decisions. The queue hands the next permit
    /// to a batch future, but nothing will poll it: `Buffered` is polled only by
    /// the loop that has just left it to await an append of its own. The turn
    /// stops there, with a prefix of decisions written, no observations, and
    /// nothing running — a live session sat like that for minutes.
    ///
    /// So the batch is polled to exhaustion first, and appends happen afterwards
    /// when no other future is holding a place in the queue. Reads have no
    /// mutation guard to span, so nothing is weakened by the delay; every
    /// admitted intent still reaches its observation before the next request.
    #[allow(clippy::too_many_arguments)]
    async fn settle_reads(
        &self,
        session: &Session,
        intents: Vec<ToolIntent>,
        unavailable: &mut std::collections::HashMap<String, Observation>,
        web_tainted: bool,
        cancel: &tokio_util::sync::CancellationToken,
        wall_deadline: Option<tokio::time::Instant>,
        settle_deadline: &Arc<std::sync::OnceLock<tokio::time::Instant>>,
        sink: &dyn StreamSink,
    ) -> Vec<(
        String,
        String,
        Observation,
        Option<crate::context::DiscoveredContext>,
    )> {
        stream::iter(intents)
            .map(|intent| {
                let settled = unavailable.remove(&intent.id);
                let cancel = cancel.clone();
                let settle_deadline = Arc::clone(settle_deadline);
                async move {
                    match settled {
                        Some(obs) => (intent.id, intent.tool, obs, None),
                        None => {
                            self.execute_admitted(
                                session,
                                intent,
                                web_tainted,
                                cancel,
                                wall_deadline,
                                settle_deadline,
                                None,
                                sink,
                            )
                            .await
                        }
                    }
                }
            })
            .buffered(self.max_parallel_tools)
            .collect()
            .await
    }

    /// Make one settled execution durable and feed its observation back. The
    /// caller holds any mutation guard across this and [`Self::execute_admitted`],
    /// closing the side-effect → event-log gap.
    #[allow(clippy::too_many_arguments)]
    async fn persist_admitted(
        &self,
        session: &Session,
        id: String,
        tool: String,
        mut obs: Observation,
        discovered: Option<crate::context::DiscoveredContext>,
        adjusted: &std::collections::HashMap<String, String>,
        room: &mut usize,
        window_events: &mut Vec<ulid::Ulid>,
        window_taint: &mut TrustLabel,
        web_tainted: &mut bool,
        ordered_messages: &mut Vec<ModelMessage>,
        messages: &mut Vec<Message>,
        sink: &dyn StreamSink,
    ) -> Result<(), KernelError> {
        // Label web-tool output as untrusted content: a fetched page must
        // not be treated like a local file read. A tool relaying content it did
        // not produce declares that content's label, and the weaker wins.
        // An External tool answers from outside this machine's trust boundary —
        // an MCP server is at least as untrusted as a fetched page.
        let trust = match (
            self.executor.category(&tool),
            self.executor.blast_radius(&tool),
        ) {
            (Some(ToolCategory::Web), _) | (_, Some(BlastRadius::External)) => TrustLabel::Web,
            _ => TrustLabel::Tool,
        };
        let trust = obs
            .relayed_trust
            .map_or(trust, |relayed| trust.min(relayed));
        // The model reads its result, so it also reads what Medha changed in
        // the call, and can send the right types itself next time.
        if let Some(note) = adjusted.get(&id)
            && let Some(payload) = obs.payload.as_object_mut()
        {
            payload.insert("adjusted".into(), serde_json::json!(note));
        }
        if let Some(discovered) = discovered {
            let context_event = self
                .log
                .append(Event::context_file(
                    session,
                    &discovered.path,
                    &discovered.content,
                    discovered.blocked,
                    discovered.trust,
                ))
                .await?;
            window_events.push(context_event.id);
            *window_taint = window_taint.min(discovered.trust);
            attach_discovered_context(&mut obs, &discovered);
        }
        // Persist applied memory before its observation so replay cannot miss it.
        let applied = if matches!(obs.status, crate::types::ObsStatus::Ok) && is_memory_tool(&tool)
        {
            obs.payload
                .as_object_mut()
                .and_then(|payload| payload.remove("applied"))
        } else {
            None
        };
        if let Some(op) = applied.filter(|op| op.is_object()) {
            self.log.append(Event::memory_write(session, op)).await?;
        }
        let stored = self.fit_for_model(&mut obs, room).await;
        let mut event = Event::observed_tool(session, &obs, trust, &tool);
        if let Some(hash) = &stored {
            event.payload["stored"] = serde_json::json!(hash);
        }
        let event = self.log.append(event).await?;
        let screen = event.payload.get(crate::events::TOOL_SCREEN_FIELD).cloned();
        crate::events::strip_private_observation_fields(&mut obs.payload);
        window_events.push(event.id);
        *window_taint = window_taint.min(trust);
        // Once untrusted web content lands, taint the following provider turn
        // so consequential actions derived from it get escalated.
        if matches!(trust, TrustLabel::Web) {
            *web_tainted = true;
        }
        let ok = matches!(obs.status, crate::types::ObsStatus::Ok);
        sink.tool_result_saved(event.id, &id, &tool, ok, &obs.payload, stored.as_deref());
        if let Some(screen) = &screen {
            sink.tool_screen(&id, screen);
        }
        let message =
            Message::tool_result(&id, crate::events::observation_for_model(&event.payload));
        ordered_messages.push(message.ordered());
        messages.push(message);
        if !obs.media.is_empty() {
            let carrier = crate::events::tool_media_message(&id, std::mem::take(&mut obs.media))
                .carrying(trust);
            ordered_messages.push(carrier.ordered());
            messages.push(carrier);
        }
        Ok(())
    }

    /// Count and validate the exact prepared request. Adaptive profiles may
    /// continue when no trustworthy counter is available; strict profiles
    /// require both an authoritative source and an exact fingerprint match.
    async fn validated_preflight(
        &self,
        prepared: &crate::provider::PreparedModelRequest,
        control: &crate::context::CompileControl,
    ) -> Result<Option<InputTokenCount>, KernelError> {
        let strict = self.provider.token_accounting_mode() == TokenAccountingMode::Strict;
        let count = self.provider.count_input_tokens(prepared);
        tokio::pin!(count);
        let counted = match control.deadline() {
            Some(deadline) => {
                tokio::select! {
                    biased;
                    _ = control.cancellation_token().cancelled() => {
                        return Err(KernelError::Interrupted);
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        return Err(KernelError::Budget(crate::budgets::BudgetStop::Wall));
                    }
                    result = &mut count => result,
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = control.cancellation_token().cancelled() => {
                        return Err(KernelError::Interrupted);
                    }
                    result = &mut count => result,
                }
            }
        };
        match counted {
            Ok(Some(count)) if count.request_fingerprint != prepared.request_fingerprint => {
                if strict {
                    Err(KernelError::Provider(
                        "strict token accounting rejected a stale request fingerprint".into(),
                    ))
                } else {
                    Ok(None)
                }
            }
            Ok(Some(count))
                if strict && count.quality != TokenCountQuality::Authoritative =>
            {
                Err(KernelError::Provider(format!(
                    "strict token accounting requires an authoritative preflight counter; profile returned {:?}",
                    count.quality
                )))
            }
            Ok(Some(count)) => Ok(Some(count)),
            Ok(None) if strict => Err(KernelError::Provider(
                "strict token accounting requires an authoritative preflight counter for this profile"
                    .into(),
            )),
            Ok(None) => Ok(None),
            Err(error) if strict => Err(KernelError::Provider(error.to_string())),
            Err(_) => Ok(None),
        }
    }

    /// Run a session to completion: stream, execute tool calls, feed results back
    /// until the model finishes or `max_turns` is hit. Context is recompiled each turn.
    #[tracing::instrument(level = "info", skip_all, fields(session = %session.id))]
    pub async fn run_session(
        &self,
        session: &Session,
        messages: Vec<Message>,
        budget: crate::budgets::Budget,
        sink: &dyn StreamSink,
        interrupts: Option<crate::interrupts::InterruptQueue>,
    ) -> Result<(Vec<Message>, StopReason), KernelError> {
        // A turn always keeps its own reservation, even if a surface switches
        // the idle conversation owner later. Refuse before writing any event.
        let _claim = self.reserve_session(session.id).map_err(KernelError::Log)?;
        let outcome = self
            .run_session_inner(session, messages, budget, sink, interrupts)
            .await;
        if let Err(error) = &outcome
            && !matches!(
                error,
                KernelError::Log(_) | KernelError::Interrupted | KernelError::Budget(_)
            )
        {
            let reason: String = error.to_string().chars().take(1024).collect();
            let mut event = Event::user_input(
                session,
                &format!(
                    "[runtime: turn failed] {reason}\nThe task did not finish. Previously recorded tool results \
                 remain valid; use them when continuing instead of restarting completed work."
                ),
                TrustLabel::Tool,
            );
            event.payload["turn_failed"] = serde_json::json!(true);
            self.log.append(event).await?;
        }
        outcome
    }

    async fn run_session_inner(
        &self,
        session: &Session,
        mut messages: Vec<Message>,
        budget: crate::budgets::Budget,
        sink: &dyn StreamSink,
        mut interrupts: Option<crate::interrupts::InterruptQueue>,
    ) -> Result<(Vec<Message>, StopReason), KernelError> {
        // `None` (headless) → a token that never trips; every cancel path is dead.
        let cancel = interrupts.as_ref().map(|q| q.token()).unwrap_or_default();
        // A sub-agent or a headless run has nobody pausing between its turns.
        let request_owner = crate::RequestScope {
            conversation: session.id.to_string(),
            unattended: self.sub_agent || interrupts.is_none(),
        };
        // A turn that fell back to another model does not decide the next one,
        // unless the user's own is still resting after a limit.
        let resting = match self.sub_agent {
            true => None,
            false => self.provider.restore_primary(),
        };
        if let Some((was, now)) = &resting {
            sink.notice(&format!(
                "{was} is still resting after its limit. Using {now} for this message."
            ));
        }
        let started = crate::events::now_ts();
        let mut images_left_out_said = false;
        let (mut retired_said, mut unreadable_said) = (0, false);
        // Replies that stopped early in this run and were asked to go on.
        let mut cut_offs = 0u32;
        // What such a reply has written so far, so its next piece does not repeat it.
        let mut join = crate::seam::Join::default();
        // Times this run waited for a server that was only away for now.
        let mut waits = 0u32;
        // Empty replies: whether the one nudge was used, the tries made since the
        // last real reply, and whether the last empty one produced no output at all.
        let (mut empty_nudged, mut empty_tries, mut empty_was_silent) = (false, 0u32, false);
        // The last real reply was text followed only by tidying-up tools.
        let mut tidied_after_text = false;
        // The model that failed and the one answering for it, once one stood in.
        let mut stood_in: Option<(String, String)> = resting;
        // Where a refusal left this run's images; later requests start there.
        let mut image_floor = match self.shrinks_images.load(Ordering::Relaxed) {
            true => Sending::Shrunk,
            false => Sending::Whole,
        };
        let planning = session.autonomy == crate::types::AutonomyLevel::Plan;
        // Tools describe themselves by this, so it is said before they are listed.
        self.executor.images_reach_model(
            self.vision.is_some() || self.route_reads_images().unwrap_or(false),
        );
        let mut gov = crate::budgets::Governor::new(budget);
        // Every trailing user message is new, not just the last: a surface can
        // append a typed prompt and then an agent report in one turn.
        // Memory taint window: event ids since the last real user message and the
        // lowest trust among them. Kernel-owned; the model can never assert it.
        let mut window_events: Vec<ulid::Ulid> = Vec::new();
        let mut window_taint = TrustLabel::User;
        // Skip what the log already ends with: a retry would append the run
        // twice, and the projection only collapses adjacent identical turns.
        let history_started = std::time::Instant::now();
        let prior_events = self.log.checked_events(session.id).await?;
        let prior_tools = crate::tool_snapshot::ToolSnapshot::restore(&prior_events);
        let mut tool_snapshot = crate::tool_snapshot::ToolSnapshot::refresh(
            prior_tools.as_ref(),
            self.executor.as_ref(),
            false,
        )
        .await;
        let mut specs = tool_snapshot.specs.clone();
        self.context.note_tools(&specs);
        // A surface restored from the log may end with user-channel failure
        // feedback. That whole prefix is already admitted, even across usage,
        // hook and compaction events; only its new suffix is a new prompt.
        let projected = crate::events::project_messages(&prior_events);
        let system_len = messages
            .iter()
            .take_while(|m| m.role == Role::System)
            .count();
        let admitted = (!projected.is_empty()
            && messages[system_len..].len() >= projected.len()
            && messages[system_len..]
                .iter()
                .zip(&projected)
                .all(|(a, b)| same_legacy_message(a, b)))
        .then_some(system_len + projected.len());
        let already = logged_tail(&prior_events);
        // Retried input is already durable but still belongs to this evidence
        // window. Skipping its append must not also erase its provenance or
        // upgrade a Web/Tool report back to User.
        window_events.extend(already.iter().map(|input| input.id));
        for input in &already {
            window_taint = window_taint.min(input.trust);
        }
        let mut logged_cursor = 0;
        let fresh = admitted.unwrap_or_else(|| unlogged_tail(&messages));
        let new_image_submission = messages[fresh..]
            .iter()
            .any(|message| !message.attachments.is_empty());
        let mut image_route_reported = false;
        for message in &mut messages[fresh..] {
            // Match the durable suffix in order and with its trust label.
            // Membership matching reordered duplicate lines, while comparing
            // text alone could treat a Tool/Web report as a trusted retry.
            let trust = message.trust.unwrap_or(TrustLabel::User);
            if admitted.is_none()
                && already.get(logged_cursor).is_some_and(|input| {
                    crate::turn_instruction::without_prefix(
                        &input.content,
                        input.instruction_prefix.as_deref(),
                    ) == crate::turn_instruction::without_prefix(
                        &message.content,
                        message.instruction_prefix.as_deref(),
                    ) && input.trust == trust
                        && input.attachments == serde_json::json!(message.attachments)
                })
            {
                message.content = already[logged_cursor].content.clone();
                message.instruction_prefix = already[logged_cursor].instruction_prefix.clone();
                logged_cursor += 1;
                continue;
            }
            crate::turn_instruction::apply(message, planning);
            let e = self
                .log
                .append(Event::user_input_message(session, message))
                .await?;
            window_events.push(e.id);
            // A report carries the weakest label its agent touched; injected
            // content must not enter as if the user had typed it.
            if let Some(trust) = message.trust {
                window_taint = window_taint.min(trust);
            }
        }
        let prompt = messages[fresh..]
            .iter()
            .filter(|message| {
                message.role == crate::types::Role::User
                    && message.trust.is_none_or(|trust| trust == TrustLabel::User)
            })
            .map(|message| {
                crate::turn_instruction::without_prefix(
                    &message.content,
                    message.instruction_prefix.as_deref(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut hook_events = self
            .session_start_hooks(session, !prior_events.is_empty(), &cancel, &mut messages)
            .await?;
        match self
            .prompt_submit_hooks(session, &prompt, &cancel, &mut messages)
            .await?
        {
            hook_points::PromptGate::Proceed(events) => hook_events.extend(events),
            hook_points::PromptGate::Blocked(reason) => {
                if let Some(q) = interrupts.as_mut() {
                    Self::return_unapplied_steers(q, sink);
                }
                sink.notice(&format!("prompt blocked by a plugin hook: {reason}"));
                return Ok((messages, StopReason::Blocked));
            }
        }
        if !hook_events.is_empty() {
            window_events.extend(hook_events);
            window_taint = window_taint.min(TrustLabel::Tool);
        }
        let mut hook_continuations = 0u8;
        if prior_tools.as_ref().map(|snapshot| snapshot.value()) != Some(tool_snapshot.value()) {
            self.log
                .append(Event::tool_snapshot(session, &tool_snapshot))
                .await?;
        }
        let mut ordered_messages: Vec<ModelMessage> =
            messages.iter().map(Message::ordered).collect();
        let mut logged_events = self.log.checked_events(session.id).await?;
        tracing::info!(
            elapsed_ms = history_started.elapsed().as_millis() as u64,
            "session history prepared"
        );
        let checkpoint = logged_events.iter().rev().find(|event| {
            event.kind == EventKind::Compaction
                && crate::events::has_valid_compaction_snapshot(&event.payload)
        });
        let has_checkpoint = checkpoint.is_some();
        let mut checkpoint_policy = checkpoint
            .and_then(|event| event.payload.get("policy_version"))
            .and_then(serde_json::Value::as_u64);
        let mut check_checkpoint_policy = has_checkpoint;
        let mut history_scope = None;
        let mut restored_usage = false;
        let usage_anchor = logged_events
            .iter()
            .rev()
            .take_while(|event| event.kind != EventKind::Compaction)
            .find(|event| event.kind == EventKind::ContextUsage)
            .and_then(|event| {
                serde_json::from_value::<crate::usage_reporting::UsageAnchor>(event.payload.clone())
                    .ok()
            });
        if has_checkpoint {
            // A compaction event is a full request checkpoint: replace both views
            // wholesale, or the system sheath duplicates and replay is not exact.
            messages = crate::events::project_request_messages(&logged_events);
            ordered_messages = crate::events::project_request_ordered_messages(&logged_events);
        } else if logged_events
            .iter()
            .any(|event| event.kind == EventKind::ModelMessage)
        {
            let projected = crate::events::project_ordered_messages(&logged_events);
            ordered_messages = hydrate_ordered_log(&messages, projected);
        }
        let mut retained_summary = logged_events.iter().rev().find_map(|event| {
            if event.kind != EventKind::Compaction
                || !crate::events::has_valid_compaction_snapshot(&event.payload)
            {
                return None;
            }
            event
                .payload
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .filter(|summary| {
                    messages.iter().any(|message| {
                        message.role == Role::Assistant && message.content == *summary
                    })
                })
                .map(str::to_owned)
        });
        // Spill after hydration — checkpoint replay would silently undo an earlier
        // spill. Both views are rewritten independently.
        self.spill_hydrated_tool_results(&mut messages, &mut ordered_messages)
            .await;
        // Flips true once a web-labeled observation enters this request, so a
        // later consequential action derived from it escalates. Request-scoped.
        let mut web_tainted = messages
            .iter()
            .any(|message| message.trust == Some(TrustLabel::Web));
        'turns: loop {
            // Turn boundary: honor a pending cancel first (queued steers go
            // BACK to the surface, not into a turn that won't run), then
            // inject queued steers as user messages.
            if let Some(q) = interrupts.as_mut() {
                if q.cancel_requested() {
                    return self.finish_interrupted(session, messages, q, sink).await;
                }
                for (s, trust) in q.drain_steers() {
                    let e = self
                        .log
                        .append(Event::user_input(session, &s, trust))
                        .await?;
                    // Fresh user input starts a new memory-evidence window.
                    window_events.clear();
                    window_events.push(e.id);
                    logged_events.push(e);
                    history_scope = None;
                    // A sub-agent's report arrives on this queue too, and it is
                    // worth what the agent touched, not what the operator says.
                    window_taint = trust;
                    if matches!(trust, TrustLabel::Web) {
                        web_tainted = true;
                    }
                    self.log
                        .append(Event::interrupt(session, "steer", Some(&s)))
                        .await
                        .ok();
                    sink.steered(&s);
                    let mut message = Message::user(s);
                    if trust != TrustLabel::User {
                        message.trust = Some(trust);
                    }
                    ordered_messages.push(message.ordered());
                    messages.push(message);
                }
            }

            // Budget gate: stop gracefully before a turn if any ceiling is hit (I4).
            if let Some(stop) = gov.check() {
                if let Some(q) = interrupts.as_mut() {
                    Self::return_unapplied_steers(q, sink);
                }
                return Ok((messages, StopReason::Budget(stop)));
            }
            gov.record_turn();
            let compile_control =
                crate::context::CompileControl::new(cancel.clone(), gov.deadline());

            // Every provider call—including calls after tool results—runs the
            // full prepare → count → compile loop. If compaction changes the
            // candidate, the request is rebuilt and re-counted before sending.
            let mut overflow_retried = false;
            // Set when this call's reply stopped early and the model is to go on.
            let mut go_on: Option<GoOn> = None;
            // How this request sends its images, and what the last try carried.
            let mut sending = image_floor;
            let mut sent = Sent::default();
            // The server's first words for a refusal the images may have caused.
            let mut image_refusal: Option<String> = None;
            // Refused one small image, and never named size or damage as why.
            let (mut small_refused, mut unfit_named) = (false, false);
            let mut tool_call_corrections = 0;
            let mut compaction_passes = 0u32;
            let (turn, request_tools) = 'model_call: loop {
                let (prepared, prepared_input_tokens, reserved_output_tokens) = loop {
                    let limits = self.provider.model_limits();
                    let input_limit = limits
                        .input_allowance(self.provider.requested_output_tokens())
                        .map(|tokens| tokens.min(u64::from(u32::MAX)) as u32);
                    let mut candidate = CompiledContext {
                        model: String::new(),
                        messages: messages.clone(),
                        ordered: Some(ordered_messages.clone()),
                        tools: specs.clone(),
                    };
                    // Images either go to the model as pixels or reach it as a
                    // described, clearly-labelled substitute. They are never
                    // dropped on the way to a model that cannot see.
                    if sending != Sending::AsText && self.route_reads_images()? {
                        sent = crate::request_images::resolve_media(
                            &mut candidate,
                            Arc::clone(&self.artifacts),
                            sending,
                            self.shrinker.as_ref(),
                            self.image_side.load(Ordering::Relaxed),
                        )
                        .await;
                        if sending != Sending::OneSmall && sent.retired > retired_said {
                            retired_said = sent.retired;
                            sink.notice(&format!(
                                "{} older image(s) from tools are no longer sent to the model, to \
                                 keep requests within limits. The model was told.",
                                sent.retired
                            ));
                        }
                        if sent.unreadable > 0 && !unreadable_said {
                            unreadable_said = true;
                            sink.notice(&format!(
                                "{} image(s) could not be read back and were left out. The model \
                                 was told.",
                                sent.unreadable
                            ));
                        }
                    } else {
                        let done = crate::vision::describe_media(
                            &mut candidate,
                            Arc::clone(&self.artifacts),
                            self.vision.as_deref(),
                        )
                        .await;
                        if let (Some(describer), true) = (&self.vision, done.described > 0) {
                            sink.notice(&format!(
                                "{} image(s) described by {} — this model has no image input",
                                done.described,
                                describer.model()
                            ));
                        }
                        // Said once a run: every request of it carries the same images.
                        if done.not_shown > 0 && !images_left_out_said {
                            images_left_out_said = true;
                            sink.notice(&match (&self.vision, &done.failure) {
                                (Some(describer), Some(error)) => format!(
                                    "{} image(s) left out: this model cannot read images and {} \
                                     could not describe them ({error}). The model was told.",
                                    done.not_shown,
                                    describer.model()
                                ),
                                _ => format!(
                                    "{} image(s) left out: this model cannot read images. The \
                                     model was told. To have images described, name a vision \
                                     model under [auxiliary] vision in config.toml.",
                                    done.not_shown
                                ),
                            });
                        }
                    }
                    let prepared = self
                        .provider
                        .prepare_request_in(&candidate, &request_owner)
                        .map_err(|error| KernelError::Provider(error.to_string()))?;

                    let request_scope = format!(
                        "{}:{}:{}:{limits:?}",
                        session.id,
                        self.provider.context_identity(),
                        prepared.model,
                    );
                    self.context.begin_request(&request_scope);
                    if history_scope.as_ref() != Some(&request_scope) {
                        self.context.restore_history(&logged_events, input_limit);
                        history_scope = Some(request_scope);
                    }
                    if !restored_usage {
                        restored_usage = true;
                        self.context.restore_usage(&candidate, 0);
                        if let Some(anchor) = &usage_anchor
                            && let Some(prefix) = anchor.matching_context(
                                self.provider.as_ref(),
                                &candidate,
                                &request_owner,
                            )
                        {
                            self.context.restore_usage(&prefix, anchor.prompt_tokens);
                        }
                    }

                    if let Some(summary) = &retained_summary {
                        self.context.restore_summary(summary);
                    }

                    self.context.clear_preflight();
                    let preflight = match self
                        .validated_preflight(&prepared, &compile_control)
                        .await
                    {
                        Ok(count) => count,
                        Err(KernelError::Interrupted) => {
                            if let Some(q) = interrupts.as_mut() {
                                return self.finish_interrupted(session, messages, q, sink).await;
                            }
                            return Ok((messages, StopReason::Interrupted));
                        }
                        Err(KernelError::Budget(stop)) => {
                            if let Some(q) = interrupts.as_mut() {
                                Self::return_unapplied_steers(q, sink);
                            }
                            return Ok((messages, StopReason::Budget(stop)));
                        }
                        Err(error) => return Err(error),
                    };
                    if let Some(count) = &preflight {
                        self.context.update_preflight(count);
                    }

                    if check_checkpoint_policy {
                        check_checkpoint_policy = false;
                        if self.context.compaction_policy_version().is_some()
                            && checkpoint_policy != self.context.compaction_policy_version()
                            && self
                                .context
                                .checkpoint_needs_refresh(&messages, input_limit)
                        {
                            self.context.force_next_compaction();
                        }
                    }

                    let compaction_planned =
                        self.context.compaction_planned(&messages, input_limit);
                    if compaction_planned {
                        self.observe_hook(
                            session,
                            medha_extension_api::HookPoint::PreCompaction,
                            serde_json::json!({
                                "message_count": messages.len(),
                                "input_limit": input_limit,
                            }),
                        )
                        .await;
                    }

                    if compaction_planned {
                        sink.compacting(true);
                    }
                    let compiled = self
                        .context
                        .compile_controlled(&messages, input_limit, &compile_control)
                        .await;
                    if compaction_planned {
                        sink.compacting(false);
                    }
                    if let Some(failure) = self.context.take_summary_failure() {
                        sink.notice(&failure);
                        if let Err(error) = self
                            .log
                            .append(Event::summary_failed(session, &failure))
                            .await
                        {
                            tracing::warn!(%error, "could not persist the summary failure");
                        }
                    }
                    let compiled = match compiled {
                        Ok(compiled) => compiled,
                        Err(crate::context::ContextCompileError::Cancelled) => {
                            if let Some(q) = interrupts.as_mut() {
                                return self.finish_interrupted(session, messages, q, sink).await;
                            }
                            return Ok((messages, StopReason::Interrupted));
                        }
                        Err(crate::context::ContextCompileError::Deadline) => {
                            if let Some(q) = interrupts.as_mut() {
                                Self::return_unapplied_steers(q, sink);
                            }
                            return Ok((
                                messages,
                                StopReason::Budget(crate::budgets::BudgetStop::Wall),
                            ));
                        }
                        Err(error @ crate::context::ContextCompileError::Summary(_)) => {
                            if let Some(q) = interrupts.as_mut() {
                                Self::return_unapplied_steers(q, sink);
                            }
                            return Err(KernelError::Provider(error.to_string()));
                        }
                    };
                    let pressure = self.context.pressure().unwrap_or_else(|| {
                        crate::ContextPressure::new(
                            preflight
                                .as_ref()
                                .map_or(u64::from(compiled.before_tokens), |count| count.tokens),
                            input_limit,
                            preflight
                                .as_ref()
                                .map_or(crate::TokenCountQuality::LocalEstimate, |count| {
                                    count.quality
                                }),
                        )
                    });
                    sink.context_pressure(pressure);
                    // A compaction result must be checkpointed and recounted,
                    // even if the compiler still considers it too large. Never
                    // discard useful reductions or send an unrecounted body.
                    let counted_overflow = preflight
                        .as_ref()
                        .zip(input_limit)
                        .is_some_and(|(count, limit)| count.tokens >= u64::from(limit));
                    if !compiled.compacted && (compiled.overflow || counted_overflow) {
                        if let Some(q) = interrupts.as_mut() {
                            Self::return_unapplied_steers(q, sink);
                        }
                        return Ok((
                            messages,
                            StopReason::Budget(crate::budgets::BudgetStop::ContextOverflow),
                        ));
                    }
                    if !compiled.compacted {
                        self.context.note_request(&candidate);
                        let input_tokens = preflight.as_ref().map(|count| count.tokens);
                        let output_tokens = self
                            .provider
                            .requested_output_tokens()
                            .or(limits.max_output_tokens)
                            .or_else(|| {
                                limits
                                    .max_combined_tokens
                                    .zip(input_tokens)
                                    .map(|(combined, input)| combined.saturating_sub(input))
                            });
                        break (prepared, input_tokens, output_tokens);
                    }

                    compaction_passes += 1;
                    if compaction_passes > MAX_COMPACTION_PASSES {
                        if let Some(q) = interrupts.as_mut() {
                            Self::return_unapplied_steers(q, sink);
                        }
                        return Ok((
                            messages,
                            StopReason::Budget(crate::budgets::BudgetStop::ContextOverflow),
                        ));
                    }
                    sink.compaction(
                        compiled.before_tokens,
                        compiled.after_tokens,
                        compiled.summarized,
                        compiled.summary.as_deref(),
                    );
                    // Reconcile before logging so the event checkpoints both
                    // exact views the next provider request will use. In
                    // particular, canonical messages retain opaque protocol
                    // replay state that cannot be recovered from `Message`.
                    let compacted_ordered = reconcile_ordered(
                        &compiled.messages,
                        &ordered_messages,
                        &compiled.source_indices,
                    );
                    if compiled.summarized || !has_checkpoint {
                        checkpoint_policy = self.context.compaction_policy_version();
                    }
                    if compiled.summarized {
                        tool_snapshot = crate::tool_snapshot::ToolSnapshot::refresh(
                            Some(&tool_snapshot),
                            self.executor.as_ref(),
                            true,
                        )
                        .await;
                        specs = tool_snapshot.specs.clone();
                        self.context.note_tools(&specs);
                    }
                    let mut checkpoint = Event::compaction_snapshot(
                        session,
                        compiled.before_tokens,
                        compiled.after_tokens,
                        compiled.summary.as_deref(),
                        &compiled.messages,
                        &compacted_ordered,
                    );
                    if let Some(version) = checkpoint_policy {
                        checkpoint.payload["policy_version"] = serde_json::json!(version);
                    }
                    checkpoint.payload["prune_rearm_tokens"] =
                        serde_json::json!(self.context.prune_rearm_tokens());
                    checkpoint.payload["tools"] = tool_snapshot.value();
                    self.log.append(checkpoint).await?;
                    self.post_compaction_hooks(
                        session,
                        compiled.before_tokens,
                        compiled.after_tokens,
                        compiled.summarized,
                        &cancel,
                    )
                    .await;
                    // The durable log keeps both originals and this canonical
                    // checkpoint; the active view is now the only candidate
                    // that may be prepared and sent.
                    if let Some(summary) = &compiled.summary {
                        retained_summary = Some(summary.clone());
                    }
                    ordered_messages = compacted_ordered;
                    messages = compiled.messages;
                    // With no real limit there is no next preflight ceiling to
                    // re-check. The synthetic recovery pass must itself prove
                    // enough reduction; checkpoint its work, then stop if not.
                    if compiled.overflow && input_limit.is_none() {
                        if let Some(q) = interrupts.as_mut() {
                            Self::return_unapplied_steers(q, sink);
                        }
                        return Ok((
                            messages,
                            StopReason::Budget(crate::budgets::BudgetStop::ContextOverflow),
                        ));
                    }
                };

                let wall_deadline = gov.deadline();
                match self
                    .run_turn(
                        session,
                        &prepared,
                        prepared_input_tokens,
                        reserved_output_tokens,
                        &mut gov,
                        sink,
                        &cancel,
                        wall_deadline,
                        &mut join,
                    )
                    .await
                {
                    Ok(t) => {
                        if image_refusal.take().is_some() {
                            image_floor = sending;
                            match sending {
                                // Size and damage ruled out: it takes no images.
                                Sending::AsText if small_refused && !unfit_named => {
                                    self.provider
                                        .observe_image_support(crate::ImageSupport::Unsupported);
                                    sink.notice(
                                        "This model's server refused a request with one small \
                                         image and accepted it without one. Images are left out \
                                         for this model from now on.",
                                    );
                                }
                                Sending::AsText => sink.notice(
                                    "The server refused this request's images, so they are left \
                                     out for the rest of this turn. The model was told.",
                                ),
                                Sending::OneSmall => sink.notice(
                                    "The server refused this request's images. Only the newest \
                                     is sent, made small, for the rest of this turn.",
                                ),
                                Sending::Shrunk => {
                                    self.shrinks_images.store(true, Ordering::Relaxed);
                                    sink.notice(
                                        "The server refused the images at full size. Large \
                                         images are sent smaller from now on in this chat.",
                                    );
                                }
                                Sending::Whole => {}
                            }
                        }
                        if context_has_media(&prepared.context) {
                            if self.provider.image_input_mode() == crate::ImageInputMode::Auto {
                                self.provider
                                    .observe_image_support(crate::ImageSupport::Supported);
                            }
                            if new_image_submission && !image_route_reported {
                                sink.notice("attached image sent natively to the selected model");
                                image_route_reported = true;
                            }
                        }
                        // Validate against the catalogue of the request that produced
                        // this response, not a fresh (possibly changed) executor list.
                        let tools: Vec<String> = prepared
                            .context
                            .tools
                            .iter()
                            .map(|spec| spec.name.clone())
                            .collect();
                        break (t, tools);
                    }
                    Err(KernelError::InvalidToolCall(error))
                        if tool_call_corrections < MAX_TOOL_CALL_CORRECTIONS =>
                    {
                        tool_call_corrections += 1;
                        // Keep invalid calls out of canonical history. Only bounded
                        // diagnostic data is fed back, never raw model arguments.
                        let detail = serde_json::json!({
                            "tool": error.tool.chars().take(128).collect::<String>(),
                            "call_id": error.call_id.chars().take(128).collect::<String>(),
                            "error": error.reason.chars().take(512).collect::<String>(),
                        });
                        let feedback = format!(
                            "[tool-call validation] Your response contained an invalid tool call: {detail}. \
                             No tools from that response executed. Regenerate all intended calls with \
                             valid tool names and complete JSON objects matching their schemas."
                        );
                        let mut event = Event::user_input(session, &feedback, TrustLabel::Tool);
                        event.payload["tool_call_correction"] = detail;
                        self.log.append(event).await?;
                        let message = Message::user(feedback).carrying(TrustLabel::Tool);
                        ordered_messages.push(message.ordered());
                        messages.push(message);
                        sink.notice(&format!(
                            "Invalid tool call. Asking the model to correct it ({tool_call_corrections}/{MAX_TOOL_CALL_CORRECTIONS})…"
                        ));
                        // Rebuild and re-count: feedback changes the request body.
                        continue 'model_call;
                    }
                    // The server's own refusal outranks what a catalogue says of
                    // the model in general: this deployment takes no images.
                    Err(KernelError::UnsupportedImage)
                        if sending != Sending::AsText
                            && self.provider.image_input_mode() == crate::ImageInputMode::Auto
                            && self.provider.image_support()
                                != crate::ImageSupport::Unsupported =>
                    {
                        sending = Sending::AsText;
                        self.provider
                            .observe_image_support(crate::ImageSupport::Unsupported);
                        sink.notice(
                            "This model's server says it takes no images. They reach it as text \
                             from now on.",
                        );
                        continue 'model_call;
                    }
                    // Not known to be the images' doing: send less of them each
                    // try. Only a request that then passes shows what was wrong.
                    Err(e @ (KernelError::ImageSuspected(_) | KernelError::ImageUnfit(_)))
                        if sending != Sending::AsText =>
                    {
                        unfit_named |= matches!(e, KernelError::ImageUnfit(_));
                        small_refused |= sent.one_small();
                        let refusal = e.to_string();
                        // A side the server names holds for every later request to it.
                        let named = crate::request_images::side_limit(&refusal);
                        let side = self.image_side.fetch_min(named, Ordering::Relaxed);
                        sending =
                            sending.after_refusal(sent, self.shrinker.is_some(), side.min(named));
                        image_refusal.get_or_insert(refusal);
                        continue 'model_call;
                    }
                    // A text reply cut off at its limit is kept as the model's turn;
                    // whether it is then continued is decided once it is saved.
                    // One piece of text repeated until the limit: more of it is all a
                    // continuation would bring, so the reply is dropped, not saved.
                    Err(KernelError::Truncated(text)) if crate::repetition::looping(&text) => {
                        return Err(KernelError::Provider(crate::cut_off::REPEATING.into()));
                    }
                    Err(KernelError::Truncated(text)) if !text.is_empty() => {
                        go_on = Some(GoOn::Cut(prepared_input_tokens));
                        let tools = prepared.context.tools.iter();
                        break (
                            kept_text(text),
                            tools.map(|spec| spec.name.clone()).collect(),
                        );
                    }
                    // What was written before the connection died is the model's
                    // turn too: kept, and continued from, not paid for again.
                    Err(KernelError::Dropped(text)) => {
                        go_on = Some(GoOn::Dropped(prepared_input_tokens));
                        let tools = prepared.context.tools.iter();
                        break (
                            kept_text(text),
                            tools.map(|spec| spec.name.clone()).collect(),
                        );
                    }
                    Err(KernelError::Truncated(_)) => {
                        return Err(KernelError::Provider(
                            crate::cut_off::NOTHING_WRITTEN.into(),
                        ));
                    }
                    // The text before the half-written call is kept as the model's turn,
                    // and the model is then told to do the work in smaller calls.
                    Err(KernelError::CallStalled { text, tools, said }) => {
                        if cut_offs + 1 >= crate::cut_off::ROUNDS {
                            return Err(KernelError::Provider(said));
                        }
                        go_on = Some(GoOn::Stalled(tools));
                        let tools = prepared.context.tools.iter();
                        break (
                            kept_text(text),
                            tools.map(|spec| spec.name.clone()).collect(),
                        );
                    }
                    // The request is rebuilt for the next model the user listed;
                    // with none left, the provider's own error is the answer.
                    Err(KernelError::ModelUnavailable {
                        model,
                        why,
                        said,
                        shown,
                        rest,
                        back,
                    }) => {
                        let Some((was, now)) = self.provider.fall_back(&model, rest) else {
                            // Nobody to hand over to, and the server is only away
                            // for now: wait for it, a few times, then try again.
                            let Some(asked) = back.filter(|_| waits < self.server_waits) else {
                                return Err(KernelError::Provider(said));
                            };
                            waits += 1;
                            let wait =
                                crate::recovery::wait_for_server(waits, asked, jitter_nanos());
                            if shown {
                                sink.restarted();
                            }
                            sink.notice(&format!(
                                "The model's server is still not answering. Trying again in \
                                 {}s ({waits}/{}). Stop the turn to give up.",
                                wait.as_secs(),
                                self.server_waits
                            ));
                            let heard = interrupts.as_ref().map(|q| q.activity());
                            match waited_out(wait, &cancel, heard, sink).await {
                                Waited::Out => continue 'model_call,
                                // Back to where a typed message is taken in.
                                Waited::Spoken => continue 'turns,
                                Waited::Stopped => {}
                            }
                            if let Some(q) = interrupts.as_mut() {
                                return self.finish_interrupted(session, messages, q, sink).await;
                            }
                            return Ok((messages, StopReason::Interrupted));
                        };
                        // What the failed model had begun to show is taken back.
                        if shown {
                            sink.restarted();
                        }
                        let again = match rest {
                            crate::recovery::Rest::NextMessage => "your next message tries",
                            crate::recovery::Rest::Limited(_) => {
                                "once its limit has had time to lift, a message tries"
                            }
                        };
                        sink.notice(&format!(
                            "{was} could not be used: {why}. Using {now} for the rest of this \
                             turn; {again} {was} again."
                        ));
                        stood_in = Some((was, now));
                        continue 'model_call;
                    }
                    Err(KernelError::ContextOverflow { reported_limit }) if !overflow_retried => {
                        overflow_retried = true;
                        if let Some(limit) = reported_limit {
                            self.provider.update_context_limit(limit);
                        }
                        self.context.force_next_compaction();
                        continue 'model_call;
                    }
                    Err(KernelError::ContextOverflow { .. }) => {
                        // Already retried once and still over — stop gracefully.
                        if let Some(q) = interrupts.as_mut() {
                            Self::return_unapplied_steers(q, sink);
                        }
                        return Ok((
                            messages,
                            StopReason::Budget(crate::budgets::BudgetStop::ContextOverflow),
                        ));
                    }
                    Err(KernelError::Interrupted) => {
                        if let Some(q) = interrupts.as_mut() {
                            return self.finish_interrupted(session, messages, q, sink).await;
                        }
                        return Ok((messages, StopReason::Interrupted));
                    }
                    Err(KernelError::Budget(stop)) => {
                        if let Some(q) = interrupts.as_mut() {
                            Self::return_unapplied_steers(q, sink);
                        }
                        return Ok((messages, StopReason::Budget(stop)));
                    }
                    // Refused without its images too: they were not why, so the
                    // first refusal is the one to report, and nothing is learned.
                    Err(e) => {
                        return Err(match image_refusal.take() {
                            Some(refusal) => KernelError::Provider(refusal),
                            None => e,
                        });
                    }
                }
            };
            // Before this round's tools run: could an image reach the model at
            // all? What a refusal just taught is already counted.
            self.executor.images_reach_model(
                self.vision.is_some() || self.route_reads_images().unwrap_or(false),
            );
            let (assistant, canonical, intents, usage, turn_interrupted) = turn;
            if let Some(u) = usage {
                self.context.update_usage(u.prompt_tokens, u.total_tokens);
            }
            if let Some(p) = self.price() {
                sink.cost(gov.cost_usd(), p.indicative);
            }
            // Interrupted mid-stream, or cancelled between the stream ending
            // and dispatch: keep visible content but drop un-admitted calls.
            // Persist the exact ordered remainder only after that decision, so
            // replay can never contain a call which dispatch never admitted.
            if turn_interrupted || cancel.is_cancelled() {
                let canonical = strip_tool_calls(&canonical);
                if !assistant.content.is_empty() {
                    let mut reply = self.reply_text(session, &assistant.content, started).await;
                    if let Some((was, now)) = &stood_in {
                        reply.payload["stand_in"] = serde_json::json!({ "model": now, "for": was });
                    }
                    let saved = self.log.append(reply).await?;
                    sink.text_saved(saved.id);
                }
                // An empty cancelled turn can create invalid adjacent roles on replay.
                if !canonical.parts.is_empty() {
                    self.log
                        .append(Event::model_message(session, &canonical))
                        .await?;
                    ordered_messages.push(canonical);
                    // Keep compatibility and canonical histories aligned.
                    messages.push(Message::assistant_calls(assistant.content, Vec::new()));
                } else if !assistant.content.is_empty() {
                    // Preserve compatibility text omitted from the canonical view.
                    messages.push(Message::assistant_calls(assistant.content, Vec::new()));
                }
                if matches!(gov.check(), Some(crate::budgets::BudgetStop::Wall)) {
                    if let Some(q) = interrupts.as_mut() {
                        Self::return_unapplied_steers(q, sink);
                    }
                    return Ok((
                        messages,
                        StopReason::Budget(crate::budgets::BudgetStop::Wall),
                    ));
                }
                if let Some(q) = interrupts.as_mut() {
                    return self.finish_interrupted(session, messages, q, sink).await;
                }
                return Ok((messages, StopReason::Interrupted));
            }
            // No words and no action: not an answer, and never saved as one.
            let empty =
                assistant.content.trim().is_empty() && intents.is_empty() && go_on.is_none();
            let thought = canonical
                .parts
                .iter()
                .any(|part| matches!(part, ContentPart::Reasoning(_)));
            // A piece that only wrote the end of the last one again adds nothing:
            // the reply was already whole, and the turn ends as it is.
            let said_again = empty && join.cut > 0;
            // The text so far is carried only while the reply is still going on.
            match &go_on {
                Some(_) => join.written.push_str(&assistant.content),
                None if !empty || said_again => join.written.clear(),
                None => {}
            }
            // A sub-agent sharing this connection may have moved it to a stand-in:
            // the answer is still marked, and the person is still told.
            if let (None, Some((was, now))) = (&stood_in, self.provider.standing_in()) {
                sink.notice(&format!(
                    "{was} could not be used, so {now} is answering for the rest of this turn."
                ));
                stood_in = Some((was, now));
            }
            if !assistant.content.is_empty() && !empty {
                let mut reply = self.reply_text(session, &assistant.content, started).await;
                if let Some((was, now)) = &stood_in {
                    reply.payload["stand_in"] = serde_json::json!({ "model": now, "for": was });
                }
                let saved = self.log.append(reply).await?;
                sink.text_saved(saved.id);
            }
            // A stream that died before any text leaves no turn of the model's to save.
            let nothing_kept =
                matches!(go_on, Some(GoOn::Stalled(_))) && canonical.parts.is_empty();
            if !empty && !nothing_kept {
                // Thinking this server takes back is part of the next request's size.
                if thought && self.provider.replays_reasoning() {
                    let thinking = canonical.parts.iter().filter_map(|part| match part {
                        ContentPart::Reasoning(part) => part.text.as_deref(),
                        _ => None,
                    });
                    self.context
                        .note_thinking_sent_back(&thinking.collect::<String>());
                }
                // Text followed only by tidying up is a finished answer.
                tidied_after_text = !assistant.content.trim().is_empty()
                    && !intents.is_empty()
                    && intents
                        .iter()
                        .all(|call| crate::empty_reply::tidies_up(&call.tool));
                self.log
                    .append(Event::model_message(session, &canonical))
                    .await?;
                ordered_messages.push(canonical);
                messages.push(assistant);
            }

            // A reply that stopped early is saved above; the model is asked to go
            // on from it, unless it keeps stopping or the chat has no room left.
            if let Some(why) = go_on.take() {
                cut_offs += 1;
                let window = self.provider.context_window().map(u64::from);
                let rounds = crate::cut_off::ROUNDS - 1;
                let no_room = |prompt: Option<u64>| {
                    prompt.zip(window).is_some_and(|(prompt, window)| {
                        prompt.saturating_add(crate::cut_off::HEADROOM) >= window
                    })
                };
                let mut stopped = "cut off";
                let (asked, telling, full) = match why {
                    GoOn::Cut(prompt) => (
                        crate::cut_off::CONTINUE.to_owned(),
                        format!(
                            "The reply was cut off at the model's output limit. Asking it to \
                             continue ({cut_offs}/{rounds})…"
                        ),
                        no_room(prompt),
                    ),
                    GoOn::Dropped(prompt) => {
                        stopped = "interrupted";
                        (
                            crate::cut_off::RESUME.to_owned(),
                            format!(
                                "The connection dropped while the model was writing. What it \
                                 wrote is kept; asking it to continue ({cut_offs}/{rounds})…"
                            ),
                            no_room(prompt),
                        )
                    }
                    GoOn::Stalled(tools) => {
                        let limits = self.provider.model_limits();
                        let allowance = self
                            .provider
                            .requested_output_tokens()
                            .or(limits.max_output_tokens);
                        (
                            crate::cut_off::split_advice(&tools, allowance),
                            format!(
                                "The connection kept dropping while the model wrote a large \
                                 action. Asking it to work in smaller steps ({cut_offs}/{rounds})…"
                            ),
                            false,
                        )
                    }
                };
                if cut_offs < crate::cut_off::ROUNDS && !full {
                    let mut event = Event::user_input(session, &asked, TrustLabel::Tool);
                    event.payload["length_continuation"] = serde_json::json!(cut_offs);
                    self.log.append(event).await?;
                    let nudge = Message::user(asked).carrying(TrustLabel::Tool);
                    ordered_messages.push(nudge.ordered());
                    messages.push(nudge);
                    sink.notice(&telling);
                    continue;
                }
                sink.notice(&match full {
                    true => format!(
                        "The reply was {stopped}, and the chat already fills the model's \
                         window, so it was not continued. What was written is kept."
                    ),
                    false => format!(
                        "The reply was still {stopped} after being continued. What was \
                         written is kept."
                    ),
                });
            }

            // An empty reply after an answer and its tidying up ends the turn as
            // it is. Any other is nudged or asked for again, then another model
            // is tried, and only then is the person told.
            if empty && !tidied_after_text && !said_again {
                let after_tools = messages.last().is_some_and(|m| m.role == Role::Tool);
                if after_tools && !empty_nudged {
                    empty_nudged = true;
                    let asked = crate::empty_reply::NUDGE;
                    let mut event = Event::user_input(session, asked, TrustLabel::Tool);
                    event.payload["empty_reply"] = serde_json::json!(true);
                    self.log.append(event).await?;
                    let nudge = Message::user(asked).carrying(TrustLabel::Tool);
                    ordered_messages.push(nudge.ordered());
                    messages.push(nudge);
                    sink.notice(
                        "The model answered its tools' results with nothing. Asking it to carry on…",
                    );
                    continue;
                }
                // Twice in a row with no output at all: a third try would be the same.
                let silent = !thought && usage.is_none_or(|used| used.completion_tokens == 0);
                let certain = silent && empty_was_silent;
                empty_was_silent = silent;
                let cost = self
                    .price()
                    .zip(usage)
                    .map(|(price, used)| price.cost(&used));
                let tries = match cost {
                    Some(cost) if cost >= crate::empty_reply::COSTLY_USD => 1,
                    _ => crate::empty_reply::TRIES,
                };
                if empty_tries < tries && !certain {
                    empty_tries += 1;
                    // A model that thought is on a server that answers: no wait helps it.
                    let wait = match thought {
                        true => std::time::Duration::ZERO,
                        false => crate::empty_reply::wait(empty_tries, jitter_nanos()),
                    };
                    sink.notice(&match thought {
                        true => format!(
                            "The model thought but wrote no answer. Asking again \
                             ({empty_tries}/{tries})…"
                        ),
                        false => format!(
                            "The model answered with nothing. Asking again in {}s \
                             ({empty_tries}/{tries})…",
                            wait.as_secs()
                        ),
                    });
                    let heard = interrupts.as_ref().map(|q| q.activity());
                    if !matches!(
                        waited_out(wait, &cancel, heard, sink).await,
                        Waited::Stopped
                    ) {
                        continue;
                    }
                    if let Some(q) = interrupts.as_mut() {
                        return self.finish_interrupted(session, messages, q, sink).await;
                    }
                    return Ok((messages, StopReason::Interrupted));
                }
                let rest = crate::recovery::Rest::NextMessage;
                let Some((was, now)) = self.provider.fall_back("", rest) else {
                    return Err(KernelError::Provider(crate::empty_reply::GAVE_UP.into()));
                };
                sink.notice(&format!(
                    "{was} kept answering with nothing. Using {now} for the rest of this \
                     turn; your next message tries {was} again."
                ));
                stood_in = Some((was, now));
                (empty_tries, empty_was_silent) = (0, false);
                continue;
            }
            if !empty {
                (empty_tries, empty_was_silent) = (0, false);
                // Each round of tools may be followed by one nudge of its own.
                empty_nudged &= intents.is_empty();
            }

            let finishing = intents.is_empty();

            // Model-supplied trust/confidence/provenance is stripped and replaced
            // with taint-window values, which stop at this turn's dispatch.
            let mut intents = intents;
            // Arguments of the wrong JSON type are repaired against the tool's
            // own schema before policy, approval and dispatch see them. The
            // saved reply above keeps what the model actually wrote.
            let mut adjusted = std::collections::HashMap::new();
            // What this round's results may still put in front of the model.
            let mut room = self.result_limits().1;
            for it in &mut intents {
                let Some(spec) = specs.iter().find(|spec| spec.name == it.tool) else {
                    continue;
                };
                let repairs = crate::arg_repair::repair(&mut it.args, &spec.schema);
                if let Some(note) = crate::arg_repair::note(&repairs) {
                    let paths: Vec<&str> =
                        repairs.iter().map(|repair| repair.path.as_str()).collect();
                    tracing::info!(tool = %it.tool, arguments = ?paths, "tool call arguments repaired");
                    adjusted.insert(it.id.clone(), note);
                }
            }
            for it in &mut intents {
                if is_memory_tool(&it.tool) {
                    enrich_memory_intent(&mut it.args, window_taint, &window_events, session.id);
                }
            }

            // Dispatch admission: intents are logged HERE — after the cancel
            // check, immediately before execution — so a logged intent always
            // gets an observation (real or synthesized). Replay order per id is
            // intent → policy.decision → observation (unavailable tools skip policy).
            for it in &intents {
                let mut event = Event::model_intent(session, it);
                if let Some(note) = adjusted.get(&it.id) {
                    event.payload["adjusted"] = serde_json::json!(note);
                }
                self.log.append(event).await?;
            }
            // Notify the surface of the calls before they run (live feedback).
            for it in &intents {
                sink.tool_call_with_id(&it.id, &it.tool, &it.args);
            }
            // Provider adapters have already mapped wire names to canonical names.
            // An undeclared call is invalid model output, not a policy denial: it
            // skips mutation scheduling, hooks, approval and execution, but settles
            // in its call-order slot — some chat templates pair results by position.
            let mut unavailable = std::collections::HashMap::new();
            if intents.iter().any(|i| !request_tools.contains(&i.tool)) {
                let wire = crate::portable_tool_name_map(&request_tools);
                let mut available: Vec<String> = wire.keys().cloned().collect();
                available.sort();
                for intent in intents.iter().filter(|i| !request_tools.contains(&i.tool)) {
                    let next = match crate::types::suggested_tool(&intent.tool, &wire) {
                        Some(tool) => format!("Use `{tool}` for this instead."),
                        None => "Choose an available tool.".to_string(),
                    };
                    let mut observation = Observation::error(
                        &intent.id,
                        format!(
                            "Tool '{}' is unavailable in this request. {next} Approval cannot \
                             enable this tool; do not retry this name.",
                            intent.tool
                        ),
                    );
                    observation.payload["error_code"] = serde_json::json!("tool_unavailable");
                    observation.payload["requested_tool"] = serde_json::json!(intent.tool);
                    observation.payload["available_tools"] = serde_json::json!(available);
                    unavailable.insert(intent.id.clone(), observation);
                }
            }
            // Blast radius, not tool name, determines whether verification runs.
            let modified_files = intents.iter().any(|i| {
                !unavailable.contains_key(&i.id)
                    && matches!(
                        self.executor.blast_radius(&i.tool),
                        Some(BlastRadius::ReversibleLocal | BlastRadius::IrreversibleLocal)
                    )
            });

            // Reads run together; this turn's changes run one by one, in order. If
            // two commit B→A while the log records A→B, replay disagrees with live
            // state. What a change must not overlap comes from the executor.
            let dispatch_cancel = cancel.clone();
            let dispatch_wall_deadline = gov.deadline();
            let dispatch_settle_deadline = Arc::new(std::sync::OnceLock::new());
            // Every intent in this batch was proposed from the same prior model
            // input. A web result persisted early below taints the *next* turn,
            // not sibling calls the model had already emitted without seeing it.
            let dispatch_web_tainted = web_tainted;
            let mut read_batch = Vec::new();
            for intent in intents {
                if !unavailable.contains_key(&intent.id)
                    && let Some(mutation_key) = self.executor.mutation_key(&intent)
                {
                    if !read_batch.is_empty() {
                        let settled = self
                            .settle_reads(
                                session,
                                std::mem::take(&mut read_batch),
                                &mut unavailable,
                                dispatch_web_tainted,
                                &dispatch_cancel,
                                dispatch_wall_deadline,
                                &dispatch_settle_deadline,
                                sink,
                            )
                            .await;
                        for (id, tool, obs, discovered) in settled {
                            self.persist_admitted(
                                session,
                                id,
                                tool,
                                obs,
                                discovered,
                                &adjusted,
                                &mut room,
                                &mut window_events,
                                &mut window_taint,
                                &mut web_tainted,
                                &mut ordered_messages,
                                &mut messages,
                                sink,
                            )
                            .await?;
                        }
                    }
                    // The change takes its lane once approved, inside, and keeps it
                    // until its result is saved below; a later read never waits on it.
                    let lane = Lane {
                        key: mutation_key,
                        cancel: dispatch_cancel.clone(),
                        deadline: dispatch_wall_deadline,
                        held: Default::default(),
                    };
                    let (id, tool, obs, discovered) = self
                        .execute_admitted(
                            session,
                            intent,
                            dispatch_web_tainted,
                            dispatch_cancel.clone(),
                            dispatch_wall_deadline,
                            Arc::clone(&dispatch_settle_deadline),
                            Some(&lane),
                            sink,
                        )
                        .await;
                    self.persist_admitted(
                        session,
                        id,
                        tool,
                        obs,
                        discovered,
                        &adjusted,
                        &mut room,
                        &mut window_events,
                        &mut window_taint,
                        &mut web_tainted,
                        &mut ordered_messages,
                        &mut messages,
                        sink,
                    )
                    .await?;
                    drop(lane);
                } else {
                    read_batch.push(intent);
                }
            }
            if !read_batch.is_empty() {
                let settled = self
                    .settle_reads(
                        session,
                        read_batch,
                        &mut unavailable,
                        dispatch_web_tainted,
                        &dispatch_cancel,
                        dispatch_wall_deadline,
                        &dispatch_settle_deadline,
                        sink,
                    )
                    .await;
                for (id, tool, obs, discovered) in settled {
                    self.persist_admitted(
                        session,
                        id,
                        tool,
                        obs,
                        discovered,
                        &adjusted,
                        &mut room,
                        &mut window_events,
                        &mut window_taint,
                        &mut web_tainted,
                        &mut ordered_messages,
                        &mut messages,
                        sink,
                    )
                    .await?;
                }
            }

            // Cancelled during dispatch: every admitted intent has settled
            // (real or synthesized observation, logged above) — stop here.
            // The verifier is skipped deliberately: the user asked to stop,
            // and a build/test run can be long.
            if matches!(gov.check(), Some(crate::budgets::BudgetStop::Wall)) {
                if let Some(q) = interrupts.as_mut() {
                    Self::return_unapplied_steers(q, sink);
                }
                return Ok((
                    messages,
                    StopReason::Budget(crate::budgets::BudgetStop::Wall),
                ));
            }
            if cancel.is_cancelled() {
                if let Some(q) = interrupts.as_mut() {
                    return self.finish_interrupted(session, messages, q, sink).await;
                }
                return Ok((messages, StopReason::Interrupted));
            }

            let mut completion_verified = false;
            // Required checks run again at completion, including resumed sessions
            // and text-only claims. Plan never launches a verification command.
            if !planning && (modified_files || (finishing && self.verifier.required())) {
                let verification = tokio::select! {
                    biased;
                    _ = wait_for_deadline(dispatch_wall_deadline) => {
                        if let Some(q) = interrupts.as_mut() {
                            Self::return_unapplied_steers(q, sink);
                        }
                        return Ok((
                            messages,
                            StopReason::Budget(crate::budgets::BudgetStop::Wall),
                        ));
                    }
                    _ = cancel.cancelled() => None,
                    report = self.verifier.check(&cancel) => report,
                };
                // Cancellation can arrive while the verifier is running. Its
                // process tree has now settled; stop instead of injecting a
                // synthetic verifier failure into a turn the user cancelled.
                if cancel.is_cancelled() {
                    if let Some(q) = interrupts.as_mut() {
                        return self.finish_interrupted(session, messages, q, sink).await;
                    }
                    return Ok((messages, StopReason::Interrupted));
                }
                let verification = verification.or_else(|| self.verifier.required().then(|| crate::verify::VerifyReport {
                    ok: false,
                    summary: "required verification returned no result".into(),
                    output: "Configure a working verification command before completing this task.".into(),
                }));
                if let Some(rep) = verification {
                    completion_verified = rep.ok;
                    sink.verify(rep.ok, &rep.summary);
                    let mut tail: Vec<&str> = rep.output.lines().rev().take(40).collect();
                    tail.reverse();
                    let feedback = format!(
                        "[verifier] {} — {}\n{}",
                        if rep.ok { "PASS" } else { "FAIL" },
                        rep.summary,
                        tail.join("\n")
                    );
                    // Verifier output is tool-produced, and can contain arbitrary
                    // build-script/test output. Labelling it as User launders that
                    // text into the most-trusted instruction channel.
                    let mut event = Event::user_input(session, &feedback, TrustLabel::Tool);
                    event.payload["verifier"] = serde_json::json!({
                        "ok": rep.ok,
                        "summary": rep.summary,
                        "output": tail.join("\n"),
                    });
                    self.log.append(event).await?;
                    let message = Message::user(feedback).carrying(TrustLabel::Tool);
                    ordered_messages.push(message.ordered());
                    messages.push(message);
                }
            }
            if finishing
                && self
                    .task_completion_hooks(
                        session,
                        hook_points::CompletionHookState {
                            verified: completion_verified,
                            continuations: hook_continuations,
                            origin: window_events.first().copied(),
                        },
                        &cancel,
                        &mut messages,
                        &mut ordered_messages,
                    )
                    .await?
            {
                hook_continuations += 1;
                window_taint = window_taint.min(TrustLabel::Tool);
                continue;
            }
            if finishing {
                if let Some(q) = interrupts.as_mut() {
                    Self::return_unapplied_steers(q, sink);
                }
                let reason = if !planning && self.verifier.required() && !completion_verified {
                    StopReason::VerificationFailed
                } else {
                    StopReason::Finished
                };
                return Ok((messages, reason));
            }
        }
    }

    /// One turn: stream the model and collect a legacy control view plus the exact
    /// canonical assistant message. Transient failures retry with capped backoff;
    /// after output, only a sink that can explicitly rewind is retried. Context
    /// overflow is surfaced to the caller to compact. Other errors are fatal.
    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        &self,
        session: &Session,
        prepared: &crate::provider::PreparedModelRequest,
        mut prepared_input_tokens: Option<u64>,
        mut reserved_output_tokens: Option<u64>,
        governor: &mut crate::budgets::Governor,
        sink: &dyn StreamSink,
        cancel: &tokio_util::sync::CancellationToken,
        wall_deadline: Option<tokio::time::Instant>,
        join: &mut crate::seam::Join,
    ) -> Result<
        (
            Message,
            ModelMessage,
            Vec<ToolIntent>,
            Option<crate::types::Usage>,
            bool,
        ),
        KernelError,
    > {
        let mut attempt = 0u32;
        let mut stalls = 0u32;
        let mut cut_call_retries = 0u32;
        let mut thinking_off_tried = false;
        let (mut reasoning_reset, mut schemas_plain) = (false, false);
        // A rate-limited key was already given its one more try.
        let mut limit_retried = false;
        let mut output_limit_retried = false;
        let mut request = prepared.clone();
        // The model does not change inside one call, so neither does its price.
        let price = self.price();
        let (text, reasoning, intents, canonical, usage, interrupted) = loop {
            let reservation = governor
                .reserve_model(prepared_input_tokens, reserved_output_tokens, price)
                .map_err(KernelError::Budget)?;
            sink.phase(crate::progress::Phase::Generating);
            self.observe_hook_with(
                session,
                medha_extension_api::HookPoint::PreModel,
                serde_json::json!({"model": &request.model, "attempt": attempt}),
                cancel,
            )
            .await;
            let mut cut = crate::cut_off::CutOff::default();
            let outcome = self
                .stream_turn(&request, sink, cancel, wall_deadline, &mut cut, join)
                .await;
            self.observe_hook_with(
                session,
                medha_extension_api::HookPoint::PostModel,
                serde_json::json!({
                    "model": &request.model,
                    "attempt": attempt,
                    "outcome": match &outcome {
                        Ok((_, _, _, _, _, true)) => "interrupted",
                        Ok(_) => "completed",
                        Err(_) => "failed",
                    },
                }),
                cancel,
            )
            .await;
            match outcome {
                Ok(data) => {
                    reservation
                        .reconcile(data.4, price)
                        .map_err(KernelError::Budget)?;
                    break data;
                }
                Err((e, emitted)) => {
                    // No authoritative usage survived this attempt. The request
                    // may nevertheless have reached the provider, so consume
                    // its full worst-case reservation.
                    reservation
                        .reconcile(None, price)
                        .map_err(KernelError::Budget)?;
                    if matches!(e, crate::provider::ProviderError::Truncated) {
                        if !cut.tool_call {
                            // Thinking used the whole allowance and nothing was
                            // written: asked once more, this time without thinking.
                            let quiet = crate::provider::ReasoningConfig {
                                enabled: Some(false),
                                effort: None,
                            };
                            let again = (cut.text.is_empty()
                                && !thinking_off_tried
                                && (!emitted || sink.supports_restart()))
                            .then(|| self.provider.with_request_reasoning(&request, &quiet));
                            if let Some(Ok(Some(adjusted))) = again {
                                thinking_off_tried = true;
                                let control = crate::context::CompileControl::new(
                                    cancel.clone(),
                                    wall_deadline,
                                );
                                let recount = self.validated_preflight(&adjusted, &control).await?;
                                prepared_input_tokens = recount.as_ref().map(|count| count.tokens);
                                request = adjusted;
                                if emitted {
                                    sink.restarted();
                                }
                                sink.notice(
                                    "The model used its whole output allowance on thinking and \
                                     wrote nothing. Asking once more with thinking off…",
                                );
                                continue;
                            }
                            return Err(KernelError::Truncated(cut.text));
                        }
                        // The same call again with more room; a screen that
                        // cannot take back what it showed is not sent it twice.
                        let limits = self.provider.model_limits();
                        let room = crate::cut_off::more_room(
                            self.provider.requested_output_tokens(),
                            reserved_output_tokens,
                            cut_call_retries + 1,
                            limits.max_output_tokens,
                        );
                        let retry = cut_call_retries < crate::cut_off::ROUNDS
                            && (!emitted || sink.supports_restart());
                        let adjusted = match retry {
                            true => self
                                .provider
                                .with_output_limit(&request, room)
                                .map_err(|error| KernelError::Provider(error.to_string()))?,
                            false => None,
                        };
                        let Some(adjusted) = adjusted else {
                            return Err(KernelError::Provider(
                                crate::cut_off::CALL_STAYED_CUT.into(),
                            ));
                        };
                        cut_call_retries += 1;
                        let control =
                            crate::context::CompileControl::new(cancel.clone(), wall_deadline);
                        let recount = self.validated_preflight(&adjusted, &control).await?;
                        prepared_input_tokens = recount.as_ref().map(|count| count.tokens);
                        reserved_output_tokens = Some(room);
                        request = adjusted;
                        if emitted {
                            sink.restarted();
                        }
                        sink.notice(&format!(
                            "The model's tool call was cut off at its output limit. Retrying \
                             with more room ({cut_call_retries}/{})…",
                            crate::cut_off::ROUNDS
                        ));
                        continue;
                    }
                    match e.classify() {
                        crate::provider::ProviderFailure::InputContextOverflow {
                            reported_limit,
                        } => {
                            return Err(KernelError::ContextOverflow { reported_limit });
                        }
                        crate::provider::ProviderFailure::OutputLimit {
                            available_output: Some(available),
                        } if !emitted && !output_limit_retried && available > 0 => {
                            // Keep a tiny margin for providers whose diagnostic
                            // value is inclusive/rounded. This changes only the
                            // failed call's output cap; history is untouched.
                            let safe = available.saturating_sub(64).max(1);
                            if let Some(adjusted) = self
                                .provider
                                .with_output_limit(&request, safe)
                                .map_err(|error| KernelError::Provider(error.to_string()))?
                            {
                                // `max_tokens` participates in the request
                                // fingerprint. Re-count the adjusted request so
                                // strict mode never sends a body different from
                                // the one its counter authorized.
                                let control = crate::context::CompileControl::new(
                                    cancel.clone(),
                                    wall_deadline,
                                );
                                let adjusted_count =
                                    self.validated_preflight(&adjusted, &control).await?;
                                prepared_input_tokens =
                                    adjusted_count.as_ref().map(|count| count.tokens);
                                reserved_output_tokens = Some(safe);
                                request = adjusted;
                                output_limit_retried = true;
                                continue;
                            }
                        }
                        crate::provider::ProviderFailure::OutputLimit { .. } => {
                            return Err(KernelError::Provider(format!(
                                "the provider rejected the output-token allowance; check the profile's max_output_tokens. Original rejection: {e}"
                            )));
                        }
                        // With images aboard, they are the first thing to send less of.
                        crate::provider::ProviderFailure::PayloadTooLarge
                        | crate::provider::ProviderFailure::ImageUnfit
                            if !emitted && context_has_media(&request.context) =>
                        {
                            return Err(KernelError::ImageUnfit(e.to_string()));
                        }
                        crate::provider::ProviderFailure::ImageUnfit => {}
                        crate::provider::ProviderFailure::PayloadTooLarge => {
                            return Err(KernelError::Provider(
                                "the provider rejected the HTTP payload size; reduce retained media or byte-heavy tool results"
                                    .into(),
                            ));
                        }
                        crate::provider::ProviderFailure::UnsupportedImage
                            if !emitted
                                && self.provider.image_input_mode()
                                    == crate::ImageInputMode::Auto
                                && self.provider.image_support()
                                    != crate::ImageSupport::Unsupported
                                && context_has_media(&request.context) =>
                        {
                            return Err(KernelError::UnsupportedImage);
                        }
                        crate::provider::ProviderFailure::UnsupportedImage => {}
                        // Refusals are worded too many ways to list: test it without the image.
                        crate::provider::ProviderFailure::Fatal
                            if !emitted
                                && matches!(
                                    e,
                                    crate::provider::ProviderError::Status(400 | 415 | 422, _)
                                )
                                && self.provider.image_input_mode()
                                    == crate::ImageInputMode::Auto
                                && self.provider.image_support()
                                    != crate::ImageSupport::Unsupported
                                && context_has_media(&request.context) =>
                        {
                            return Err(KernelError::ImageSuspected(e.to_string()));
                        }
                        crate::provider::ProviderFailure::InvalidToolCall => {
                            if emitted && sink.supports_restart() {
                                sink.restarted();
                            }
                            if let crate::provider::ProviderError::InvalidToolCall(error) = e {
                                return Err(KernelError::InvalidToolCall(error));
                            }
                        }
                        crate::provider::ProviderFailure::Transient
                        | crate::provider::ProviderFailure::Fatal => {}
                    }
                    // A setting or a tool description this server will not take
                    // is left out, once, and stays out for the rest of the chat.
                    let adjusted = match e.recovery().first {
                        crate::recovery::First::DefaultReasoning if !reasoning_reset => {
                            reasoning_reset = true;
                            let own = crate::provider::ReasoningConfig::default();
                            let adjusted = self.provider.with_request_reasoning(&request, &own);
                            adjusted.ok().flatten().inspect(|_| {
                                let _ = self.provider.set_reasoning(own);
                                sink.notice(
                                    "This server refused the reasoning setting. Using its own \
                                     default for the rest of this chat…",
                                );
                            })
                        }
                        crate::recovery::First::PlainSchemas if !schemas_plain => {
                            schemas_plain = true;
                            let adjusted = self.provider.with_plain_schemas(&request);
                            adjusted.ok().flatten().inspect(|_| {
                                sink.notice(
                                    "This server could not use the pattern rules in the tool \
                                     descriptions. Sending them without those rules…",
                                );
                            })
                        }
                        _ => None,
                    };
                    if let Some(adjusted) = adjusted.filter(|_| !emitted) {
                        let control =
                            crate::context::CompileControl::new(cancel.clone(), wall_deadline);
                        let recount = self.validated_preflight(&adjusted, &control).await?;
                        prepared_input_tokens = recount.as_ref().map(|count| count.tokens);
                        request = adjusted;
                        continue;
                    }
                    // A stream that died with text already on screen: the text is
                    // kept and continued from, never sent for again from the start.
                    if e.is_retryable() && !cut.text.trim().is_empty() {
                        if !cut.tool_call {
                            return Err(KernelError::Dropped(cut.text));
                        }
                        // A half-written call cannot stay on screen; the text can.
                        if sink.supports_restart() {
                            sink.restarted();
                            sink.text(&cut.text);
                            return Err(KernelError::CallStalled {
                                text: cut.text,
                                tools: cut.tools,
                                said: e.to_string(),
                            });
                        }
                    }
                    // Stalls have their own, smaller allowance inside the
                    // turn's overall one, so earlier dropped connections
                    // neither use it up nor are counted as stalls.
                    let stalled = e.is_stream_stall();
                    // A key the server turned away is swapped for the next saved
                    // one before another model is thought of. A rate limit is
                    // first given one more try on the same key.
                    let said = match &e {
                        crate::provider::ProviderError::Status(_, said)
                        | crate::provider::ProviderError::Throttled { body: said, .. } => {
                            said.to_ascii_lowercase()
                        }
                        _ => String::new(),
                    };
                    let key_fault = match &e {
                        crate::provider::ProviderError::Status(status, _)
                        | crate::provider::ProviderError::Throttled { status, .. }
                            if matches!(status, 401 | 402 | 403 | 429) && !emitted =>
                        {
                            // A limit further upstream, or a server overloaded for
                            // everyone, is not the key's: resting it would cost a good key.
                            let healthy = match status {
                                429 => crate::rejections::not_the_keys_limit(&said),
                                // A block page in front of the server never saw the key.
                                403 => crate::rejections::blocked_on_the_way(&said),
                                _ => false,
                            };
                            (!healthy).then_some(*status)
                        }
                        _ => None,
                    };
                    let once_more = key_fault == Some(429)
                        && !limit_retried
                        && !crate::rejections::plan_limit(&said)
                        && self.provider.another_key();
                    let swapped = key_fault.filter(|_| !once_more).and_then(|status| {
                        Some((status, self.provider.next_key(status, e.retry_after())?))
                    });
                    if let Some((status, (at, of))) = swapped {
                        limit_retried = false;
                        let why = match status {
                            402 => "has no credit",
                            429 => "is rate limited",
                            _ => "was refused",
                        };
                        sink.notice(&format!(
                            "The key in use {why}. Using your next key for this server \
                             ({at} of {of})…"
                        ));
                        continue;
                    }
                    limit_retried |= once_more;
                    // With another model listed, a rate limit is not waited out
                    // and a server that cannot be reached is tried only twice more.
                    let patience = match e.recovery().hand_over_after {
                        _ if once_more => attempt + 1,
                        Some(tries) if self.provider.stand_in_left() => tries,
                        _ => MAX_TURN_RETRIES,
                    };
                    // Some failures are given fewer tries than the rest, the first counted.
                    let most = e.recovery().tries.map_or(patience, |tries| tries - 1);
                    let patience = patience.min(most);
                    if e.is_retryable()
                        && attempt < patience.min(MAX_TURN_RETRIES)
                        && (!stalled || stalls < MAX_STALL_RETRIES)
                    {
                        attempt += 1;
                        stalls += u32::from(stalled);
                        // A stream that died after streaming part of a reply is
                        // the common shape of a transient failure, so refusing
                        // to retry once anything was emitted would strand
                        // exactly the turns most worth saving. The sink drops
                        // its partial render instead, and the reply arrives once.
                        if emitted {
                            if !sink.supports_restart() {
                                return Err(KernelError::Provider(format!(
                                    "{e}; the stream stopped after partial output and this output surface cannot rewind it safely"
                                )));
                            }
                            sink.restarted();
                        }
                        // A retry is progress, not silence. Without this an
                        // inactivity bound would kill the turns that are
                        // recovering from exactly the stall it exists to catch.
                        sink.phase_ticked();
                        // The provider's own number when it gave one; retrying
                        // sooner than asked just earns another refusal.
                        let wait = e.retry_after().unwrap_or_else(|| retry_backoff(attempt));
                        sink.notice(&if stalled {
                            // Says what happened and what Medha is doing about it.
                            let said = e.to_string();
                            let said = said.strip_prefix("the model").unwrap_or(&said);
                            format!("The model{said}. Reconnecting ({stalls}/{MAX_STALL_RETRIES})…")
                        } else {
                            format!(
                                "Model request failed. Retrying ({attempt}/{MAX_TURN_RETRIES})…"
                            )
                        });
                        // The backoff nap races the cancel token too — Esc
                        // during a retry wait must stop the turn, not queue
                        // another attempt.
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => continue,
                            _ = cancel.cancelled() => {
                                break (
                                    String::new(),
                                    String::new(),
                                    Vec::new(),
                                    message_from_stream_parts(Vec::new()),
                                    None,
                                    true,
                                );
                            }
                            _ = async {
                                if let Some(deadline) = wall_deadline {
                                    tokio::time::sleep_until(deadline).await;
                                } else {
                                    std::future::pending::<()>().await;
                                }
                            } => {
                                break (
                                    String::new(),
                                    String::new(),
                                    Vec::new(),
                                    message_from_stream_parts(Vec::new()),
                                    None,
                                    true,
                                );
                            }
                        }
                    }
                    let said = match stalled && stalls > 0 {
                        true => format!(
                            "{e}, {} times this turn; Medha stopped reconnecting",
                            stalls + 1
                        ),
                        false => e.to_string(),
                    };
                    // A block under the provider's content rules is said in plain words.
                    let declined = e.recovery().stand_in == Some("it declined to answer");
                    let said = match declined {
                        true => format!("{} Its words: {said}", crate::rejections::BLOCKED),
                        false => said,
                    };
                    // A stream that keeps dying under one tool call: the screen drops
                    // the half-written call, and the model is told to send less at once.
                    if e.is_retryable() && cut.tool_call && sink.supports_restart() {
                        sink.restarted();
                        if !cut.text.is_empty() {
                            sink.text(&cut.text);
                        }
                        return Err(KernelError::CallStalled {
                            text: cut.text,
                            tools: cut.tools,
                            said,
                        });
                    }
                    // No retry is left, or none would help: another model may answer.
                    let stand_in = e.recovery().stand_in;
                    if let Some(why) = stand_in.filter(|_| !emitted || sink.supports_restart()) {
                        return Err(KernelError::ModelUnavailable {
                            model: request.model.clone(),
                            why,
                            said,
                            shown: emitted,
                            rest: e.rest(),
                            back: e.recovery().comes_back.then(|| e.retry_after()),
                        });
                    }
                    return Err(KernelError::Provider(said));
                }
            }
        };
        // `run_session` appends the canonical message only after dispatch admission.
        if !reasoning.is_empty() {
            self.log
                .append(Event::model_reasoning(session, &reasoning))
                .await?;
        }
        if let Some(usage) = usage.filter(|usage| usage.prompt_tokens > 0) {
            let anchor = crate::usage_reporting::UsageAnchor::capture(
                self.provider.context_identity(),
                &request,
                &usage,
                price,
            );
            if let Err(error) = self
                .log
                .append(Event::context_usage(session, &anchor))
                .await
            {
                tracing::warn!(%error, "could not persist context usage calibration");
            }
        }
        Ok((
            Message::assistant_calls(text, intents.clone()),
            canonical,
            intents,
            usage,
            interrupted,
        ))
    }

    /// Establish and consume one model stream, emitting deltas to the sink as
    /// they arrive. Returns the compatibility view and exact completed message.
    #[allow(clippy::type_complexity)]
    async fn stream_turn(
        &self,
        prepared: &crate::provider::PreparedModelRequest,
        sink: &dyn StreamSink,
        cancel: &tokio_util::sync::CancellationToken,
        wall_deadline: Option<tokio::time::Instant>,
        cut: &mut crate::cut_off::CutOff,
        join: &mut crate::seam::Join,
    ) -> Result<
        (
            String,
            String,
            Vec<ToolIntent>,
            ModelMessage,
            Option<crate::types::Usage>,
            bool,
        ),
        (crate::provider::ProviderError, bool),
    > {
        // A reply that goes on from an earlier piece has its start held back
        // until it is known not to repeat that piece's end.
        join.cut = 0;
        let mut seam = (!join.written.is_empty()).then(|| crate::seam::Seam::after(&join.written));
        // Connection and prompt processing must remain cancellable before the
        // first stream byte arrives.
        let request_started = std::time::Instant::now();
        let mut usage_report = crate::usage_reporting::AttemptUsage::new(sink, prepared);
        let mut stream = tokio::select! {
            s = self.provider.stream_prepared(prepared) => s.map_err(|e| (e, false))?,
            _ = cancel.cancelled() => {
                return Ok((
                    String::new(),
                    String::new(),
                    Vec::new(),
                    message_from_stream_parts(Vec::new()),
                    None,
                    true,
                ));
            }
            _ = wait_for_deadline(wall_deadline) => {
                return Ok((
                    String::new(),
                    String::new(),
                    Vec::new(),
                    message_from_stream_parts(Vec::new()),
                    None,
                    true,
                ));
            }
        };
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut intents: Vec<ToolIntent> = Vec::new();
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut completed: Option<ModelMessage> = None;
        let mut usage: Option<crate::types::Usage> = None;
        let mut emitted = false;
        let mut tool_started = false;
        // Names of the tool calls the model has begun, for advice if the stream dies.
        let mut called: Vec<String> = Vec::new();
        let mut stream_blocks = 0usize;
        let mut stream_bytes = 0usize;
        // Calls whose arguments have started to arrive. When the first piece
        // lands shows whether a provider streams a call or sends it whole.
        let mut arguments_started = std::collections::HashSet::new();
        loop {
            let block = tokio::select! {
                block = stream.next() => block,
                _ = cancel.cancelled() => {
                    // Cancelled mid-stream: keep what streamed (the user saw
                    // it), drop un-dispatched intents — they were never
                    // admitted, so nothing in the log dangles.
                    let mut canonical = completed
                        .as_ref()
                        .map(strip_tool_calls)
                        .unwrap_or_else(|| strip_tool_calls(&message_from_stream_parts(parts)));
                    only_shown(&seam, &mut text, &mut canonical);
                    return Ok((text, reasoning, Vec::new(), canonical, usage, true));
                }
                _ = wait_for_deadline(wall_deadline) => {
                    let mut canonical = completed
                        .as_ref()
                        .map(strip_tool_calls)
                        .unwrap_or_else(|| strip_tool_calls(&message_from_stream_parts(parts)));
                    only_shown(&seam, &mut text, &mut canonical);
                    return Ok((text, reasoning, Vec::new(), canonical, usage, true));
                }
            };
            let Some(block) = block else { break };
            // Every block is proof the connection is alive. Without this a slow
            // reply and a dead one share one phase and one clock.
            sink.phase_ticked();
            stream_blocks = stream_blocks.saturating_add(1);
            if stream_blocks > MAX_PROVIDER_STREAM_BLOCKS {
                return Err((
                    crate::provider::ProviderError::Decode(format!(
                        "provider stream exceeded the {} block kernel limit",
                        MAX_PROVIDER_STREAM_BLOCKS
                    )),
                    emitted,
                ));
            }
            match block {
                Ok(Block::Text(t)) => {
                    if text.is_empty() && !t.is_empty() {
                        tracing::info!(
                            elapsed_ms = request_started.elapsed().as_millis() as u64,
                            "model first text delta"
                        );
                    }
                    charge_stream_bytes(&mut stream_bytes, t.len())
                        .map_err(|error| (error, emitted))?;
                    match seam.as_mut().map(|seam| seam.feed(&t)) {
                        // Still held: nothing has reached the screen yet.
                        Some(shown) if shown.is_empty() => {}
                        Some(shown) => {
                            emitted = true;
                            sink.text(&shown);
                        }
                        None => {
                            emitted = true;
                            sink.text(&t);
                        }
                    }
                    text.push_str(&t);
                    match parts.last_mut() {
                        Some(ContentPart::Text(part)) => part.text.push_str(&t),
                        _ => parts.push(ContentPart::Text(TextPart {
                            text: t,
                            provider_state: Vec::new(),
                        })),
                    }
                }
                Ok(Block::Reasoning(r)) => {
                    if reasoning.is_empty() && !r.is_empty() {
                        tracing::info!(
                            elapsed_ms = request_started.elapsed().as_millis() as u64,
                            "model first reasoning delta"
                        );
                    }
                    charge_stream_bytes(&mut stream_bytes, r.len())
                        .map_err(|error| (error, emitted))?;
                    emitted = true;
                    sink.reasoning(&r);
                    reasoning.push_str(&r);
                    match parts.last_mut() {
                        Some(ContentPart::Reasoning(part)) => {
                            part.text.get_or_insert_with(String::new).push_str(&r)
                        }
                        _ => parts.push(ContentPart::Reasoning(ReasoningPart {
                            text: Some(r),
                            provider_state: Vec::new(),
                        })),
                    }
                }
                Ok(Block::ToolStarted { name, target }) => {
                    emitted = true;
                    tool_started = true;
                    if !called.contains(&name) {
                        called.push(name.clone());
                    }
                    sink.tool_started(&name, target.as_deref());
                }
                // Passed straight on and kept nowhere: the finished call below is
                // what is counted against the stream's size, admitted and logged.
                Ok(Block::ToolInput { id, name, delta }) => {
                    if arguments_started.len() < MAX_TOOL_INTENTS_PER_TURN
                        && arguments_started.insert(id.clone())
                    {
                        tracing::info!(
                            elapsed_ms = request_started.elapsed().as_millis() as u64,
                            tool = %name,
                            "model first tool argument delta"
                        );
                    }
                    emitted = true;
                    sink.tool_input(&id, &name, &delta);
                }
                Ok(Block::ToolIntent(it)) => {
                    if intents.len() >= MAX_TOOL_INTENTS_PER_TURN {
                        return Err((
                            crate::provider::ProviderError::Decode(format!(
                                "provider emitted more than {} tool calls in one turn",
                                MAX_TOOL_INTENTS_PER_TURN
                            )),
                            emitted,
                        ));
                    }
                    let intent_bytes = serde_json::to_vec(&it)
                        .map_err(|error| {
                            (
                                crate::provider::ProviderError::Decode(error.to_string()),
                                emitted,
                            )
                        })?
                        .len();
                    charge_stream_bytes(&mut stream_bytes, intent_bytes)
                        .map_err(|error| (error, emitted))?;
                    emitted = true;
                    parts.push(ContentPart::ToolCall(ToolCallPart {
                        id: it.id.clone(),
                        tool: it.tool.clone(),
                        args: it.args.clone(),
                        provider_state: Vec::new(),
                    }));
                    intents.push(it);
                }
                Ok(Block::Usage(u)) => {
                    usage_report.observe(u);
                    usage = Some(u);
                }
                Ok(Block::CompletedMessage(message)) => {
                    if completed.is_some() {
                        return Err((
                            crate::provider::ProviderError::Decode(
                                "provider emitted more than one completed message".into(),
                            ),
                            emitted,
                        ));
                    }
                    if let Err(error) = completed_control_view(&message) {
                        return Err((error, emitted));
                    }
                    let message_bytes = serde_json::to_vec(&message)
                        .map_err(|error| {
                            (
                                crate::provider::ProviderError::Decode(error.to_string()),
                                emitted,
                            )
                        })?
                        .len();
                    charge_stream_bytes(&mut stream_bytes, message_bytes)
                        .map_err(|error| (error, emitted))?;
                    completed = Some(message);
                }
                Err(e) => {
                    // What a reply had written when it stopped decides how it is recovered.
                    cut.tool_call =
                        tool_started || !intents.is_empty() || !arguments_started.is_empty();
                    cut.text = std::mem::take(&mut text);
                    // A start that was held or cut was never shown, and is not kept.
                    if let Some(seam) = &seam {
                        crate::seam::without_start(&mut cut.text, seam.hidden());
                    }
                    cut.tools = std::mem::take(&mut called);
                    return Err((e, emitted));
                }
            }
        }
        let mut canonical = completed.unwrap_or_else(|| message_from_stream_parts(parts));
        let (canonical_text, canonical_reasoning, canonical_intents) =
            completed_control_view(&canonical).map_err(|error| (error, emitted))?;
        if canonical_intents.len() > MAX_TOOL_INTENTS_PER_TURN {
            return Err((
                crate::provider::ProviderError::Decode(format!(
                    "provider completed message contained more than {} tool calls",
                    MAX_TOOL_INTENTS_PER_TURN
                )),
                emitted,
            ));
        }
        if !text.is_empty() && text != canonical_text {
            return Err((
                crate::provider::ProviderError::Decode(
                    "provider text deltas disagree with its completed message".into(),
                ),
                emitted,
            ));
        }
        if !reasoning.is_empty() && reasoning != canonical_reasoning {
            return Err((
                crate::provider::ProviderError::Decode(
                    "provider reasoning deltas disagree with its completed message".into(),
                ),
                emitted,
            ));
        }
        let intents_match = intents.len() == canonical_intents.len()
            && intents.iter().zip(&canonical_intents).all(|(left, right)| {
                left.id == right.id && left.tool == right.tool && left.args == right.args
            });
        if !intents.is_empty() && !intents_match {
            return Err((
                crate::provider::ProviderError::Decode(
                    "provider tool-call blocks disagree with its completed message".into(),
                ),
                emitted,
            ));
        }
        if text.is_empty() && !canonical_text.is_empty() {
            // A reply that arrived whole is read for a repeat like any other.
            match seam.as_mut().map(|seam| seam.feed(&canonical_text)) {
                Some(shown) if shown.is_empty() => {}
                Some(shown) => sink.text(&shown),
                None => sink.text(&canonical_text),
            }
            text = canonical_text;
        }
        // The reply is over: what was held is shown, less a repeat, and the
        // repeat is left out of what is saved as well.
        if let Some(seam) = seam.as_mut() {
            let held = seam.finish();
            if !held.is_empty() {
                sink.text(&held);
            }
            join.cut = seam.cut;
            crate::seam::without_start(&mut text, seam.cut);
            crate::seam::without_start_of(&mut canonical, seam.cut);
        }
        if reasoning.is_empty() && !canonical_reasoning.is_empty() {
            sink.reasoning(&canonical_reasoning);
            reasoning = canonical_reasoning;
        }
        if intents.is_empty() {
            intents = canonical_intents;
        }
        Ok((text, reasoning, intents, canonical, usage, false))
    }

    /// Take `lane` unless this call holds it already; an error is for the model.
    async fn enter_lane(&self, lane: &Lane) -> Result<(), String> {
        if lane.held.lock().unwrap().is_some() {
            return Ok(());
        }
        let entering = async {
            let held = self.lanes.enter(&lane.key).await;
            let lease = self.leases.share(self.log.as_ref(), &lane.key).await;
            lease.map(|lease| (held, lease))
        };
        let entered = tokio::select! {
            biased;
            _ = wait_for_deadline(lane.deadline) => return Err(
                "[interrupted] task wall-clock deadline elapsed while waiting for the mutation lane".into(),
            ),
            _ = lane.cancel.cancelled() => return Err(
                "[interrupted] cancelled before the mutation lane became available".into(),
            ),
            entered = entering => entered,
        };
        let held = entered.map_err(|error| {
            format!(
                "state change was not started because its mutation lease could not be \
                 acquired: {error}"
            )
        })?;
        *lane.held.lock().unwrap() = Some(held);
        Ok(())
    }

    async fn execute_with_effect_outbox(
        &self,
        session: &Session,
        intent: &ToolIntent,
        lane: Option<&Lane>,
    ) -> Observation {
        if let Some(lane) = lane
            && let Err(reason) = self.enter_lane(lane).await
        {
            return Observation::error(&intent.id, reason);
        }
        if let Some(mutation_key) = self.executor.mutation_key(intent)
            && let Err(error) = self
                .log
                .append(Event::tool_effect_prepared(session, intent, &mutation_key))
                .await
        {
            return Observation::error(
                &intent.id,
                format!(
                    "state change was not started because its durable execution record \
                     could not be written: {error}"
                ),
            );
        }
        self.executor.execute(intent).await
    }

    async fn invoke_hook(
        &self,
        session: &Session,
        point: medha_extension_api::HookPoint,
        trust: TrustLabel,
        payload: serde_json::Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<crate::hooks::HookBatch, String> {
        self.invoke_hook_lineage(
            session,
            point,
            trust,
            payload,
            crate::hooks::HookCausation::default(),
            cancel,
        )
        .await
    }

    async fn invoke_hook_lineage(
        &self,
        session: &Session,
        point: medha_extension_api::HookPoint,
        trust: TrustLabel,
        payload: serde_json::Value,
        causation: crate::hooks::HookCausation,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<crate::hooks::HookBatch, String> {
        let mut request =
            crate::hooks::HookRequest::new(session.id.to_string(), point, trust, payload);
        request.causation_id = causation.id;
        request.depth = causation.depth;
        let mut batch = self.hooks.invoke(&request, cancel).await;
        if !matches!(batch.directive, crate::hooks::HookDirective::Continue)
            && batch.audits.is_empty()
        {
            return Err("hook runner returned an unaudited decision".into());
        }
        for audit in &mut batch.audits {
            audit.event_id.clone_from(&request.event_id);
            audit.point = point;
            audit.reason = audit.reason.as_deref().map(bounded_hook_reason);
            self.log
                .append(Event::hook(session, audit))
                .await
                .map_err(|error| format!("hook decision could not be recorded: {error}"))?;
        }
        Ok(batch)
    }

    async fn apply_post_tool_hook(
        &self,
        session: &Session,
        intent: &ToolIntent,
        mut observation: Observation,
        web_tainted: bool,
        cancel: &tokio_util::sync::CancellationToken,
        sink: &dyn StreamSink,
    ) -> Observation {
        let payload = serde_json::json!({
            "intent_id": intent.id,
            "tool": intent.tool,
            "status": observation.status,
            "result": hook_safe_value(&observation.payload),
        });
        let trust = if web_tainted {
            TrustLabel::Web
        } else {
            TrustLabel::Tool
        };
        let failed = observation.status == crate::types::ObsStatus::Error;
        let mut points = vec![medha_extension_api::HookPoint::PostTool];
        if failed {
            points.push(medha_extension_api::HookPoint::ToolFailure);
        }
        let mut contexts = Vec::new();
        let mut problem = None;
        for point in points {
            match self
                .invoke_hook(session, point, trust, payload.clone(), cancel)
                .await
            {
                Ok(batch) => {
                    for notice in &batch.notices {
                        sink.notice(notice);
                    }
                    contexts.extend(batch.contexts);
                    if let crate::hooks::HookDirective::Deny(reason)
                    | crate::hooks::HookDirective::RequestApproval(reason) = batch.directive
                    {
                        problem.get_or_insert(reason);
                    }
                }
                Err(error) => {
                    problem.get_or_insert(error);
                }
            }
        }
        let file_changed = observation.status == crate::types::ObsStatus::Ok
            && matches!(intent.tool.as_str(), "write" | "edit");
        Self::attach_hook_contexts(&mut observation, &contexts);
        if let Some(problem) = problem {
            let prior_status = observation.status.clone();
            let prior_payload = std::mem::take(&mut observation.payload);
            observation.status = crate::types::ObsStatus::Error;
            observation.payload = serde_json::json!({
                "error": format!(
                    "tool completed, but its required post-tool hook did not settle safely: {problem}"
                ),
                "tool_status": prior_status,
                "tool_result": prior_payload,
            });
        }
        if file_changed {
            self.observe_hook(
                session,
                medha_extension_api::HookPoint::FileChange,
                serde_json::json!({
                    "intent_id": intent.id,
                    "tool": intent.tool,
                    "args": hook_safe_value(&intent.args),
                }),
            )
            .await;
        }
        observation
    }

    async fn dispatch_one(
        &self,
        session: &Session,
        intent: &ToolIntent,
        web_tainted: bool,
        cancel: &tokio_util::sync::CancellationToken,
        lane: Option<&Lane>,
        sink: &dyn StreamSink,
    ) -> Observation {
        let radius = self.executor.call_radius(intent);
        // Enforce before policy/human approval so custom AllowAll policies and
        // remembered approvals cannot turn Plan into an editing session.
        let raw = if session.autonomy == crate::types::AutonomyLevel::Plan
            && (radius != Some(BlastRadius::Read) || self.executor.mutation_key(intent).is_some())
        {
            crate::types::Decision::Deny {
                reason: "plan mode permits read-only tools; switch mode to implement".into(),
            }
        } else {
            self.policy.authorize(session.autonomy, intent, radius)
        };
        // Trust-flow escalations must never be auto-approved.
        let raw_permissive = matches!(raw, crate::types::Decision::Allow);
        let mut decision =
            escalate_for_trust_flow(raw, radius, web_tainted, self.executor.containment());
        let trust_escalated = raw_permissive && matches!(decision, crate::types::Decision::Human);
        let mut hook_approval_reason = None;
        if !matches!(decision, crate::types::Decision::Deny { .. }) {
            let trust = if web_tainted {
                TrustLabel::Web
            } else {
                TrustLabel::System
            };
            let payload = serde_json::json!({
                "intent_id": intent.id,
                "tool": intent.tool,
                "blast_radius": radius,
                "args": hook_safe_value(&intent.args),
            });
            let hooks = match self
                .invoke_hook(
                    session,
                    medha_extension_api::HookPoint::PreTool,
                    trust,
                    payload,
                    cancel,
                )
                .await
            {
                Ok(hooks) => hooks,
                Err(error) => {
                    return Observation::error(
                        &intent.id,
                        format!(
                            "tool execution was denied because its pre-tool hook was not auditable: {error}"
                        ),
                    );
                }
            };
            for notice in &hooks.notices {
                sink.notice(notice);
            }
            match hooks.directive {
                crate::hooks::HookDirective::Continue => {}
                crate::hooks::HookDirective::Deny(reason) => {
                    decision = crate::types::Decision::Deny { reason };
                }
                crate::hooks::HookDirective::RequestApproval(reason) => {
                    hook_approval_reason = Some(reason);
                    decision = crate::types::Decision::Human;
                }
            }
        }
        if cancel.is_cancelled() {
            return Observation::error(
                &intent.id,
                "[interrupted] cancelled while running pre-tool hooks",
            );
        }
        let escalated = trust_escalated || hook_approval_reason.is_some();
        if let Err(error) = self
            .log
            .append(Event::policy(session, intent, &decision))
            .await
        {
            return Observation::error(
                &intent.id,
                format!(
                    "tool execution was denied because its policy decision could not be \
                     durably recorded: {error}"
                ),
            );
        }
        if !matches!(decision, crate::types::Decision::Deny { .. }) {
            let access = match self.executor.missing_access(intent) {
                Ok(access) => access,
                Err(error) => return Observation::error(&intent.id, error),
            };
            if !access.is_empty() {
                // This card reviews both the command and its missing capabilities,
                // so a policy Human decision does not create a second prompt.
                let access_escalated = escalated
                    || (access.network
                        && web_tainted
                        && matches!(
                            escalate_for_trust_flow(
                                crate::types::Decision::Allow,
                                radius,
                                true,
                                crate::types::Containment::OsFsJail,
                            ),
                            crate::types::Decision::Human
                        ));
                sink.phase(crate::progress::Phase::AwaitingApproval {
                    action: approval_key(intent),
                });
                let answer = {
                    let _one_gate = self.gate_serial.lock().await;
                    let mut detail = self
                        .executor
                        .preview(intent)
                        .await
                        .unwrap_or_else(|| approval_detail(intent));
                    detail.push_str("\n\nAdditional access required before running:");
                    if access.outside_sandbox {
                        detail.push_str(
                            "\n- Run outside Medha's sandbox, this one time: the command can \
                             read and change anything your user account can",
                        );
                    }
                    if access.network {
                        detail.push_str("\n- Network access");
                    }
                    for path in &access.read_paths {
                        detail.push_str(&format!(
                            "\n- Read {} (including subdirectories)",
                            path.display()
                        ));
                    }
                    for path in &access.write_paths {
                        detail.push_str(&format!(
                            "\n- Read/write {} (including subdirectories)",
                            path.display()
                        ));
                    }
                    if access_escalated {
                        detail.push_str("\nThis command handled untrusted content; approve this invocation only.");
                    }
                    if let Some(reason) = &hook_approval_reason {
                        detail.push_str("\nA pre-tool hook requires approval: ");
                        detail.push_str(reason);
                    }
                    self.executor
                        .grant_access(&access, &detail, access_escalated)
                        .await
                };
                self.observe_hook(
                    session,
                    medha_extension_api::HookPoint::ApprovalDecision,
                    serde_json::json!({
                        "intent_id": intent.id,
                        "tool": intent.tool,
                        "kind": "command_access",
                        "decision": answer.as_ref().map_or("error".to_string(), |value| format!("{value:?}").to_lowercase()),
                    }),
                )
                .await;
                match answer {
                    Err(error) => return Observation::error(&intent.id, error),
                    Ok(crate::NetworkDecision::Deny) => {
                        return Observation::denial(
                            &intent.id,
                            "requested command access was denied; command was not run",
                        );
                    }
                    Ok(_) => {
                        self.running_tool(intent, sink);
                        let observation = crate::execution_access_scope(
                            access,
                            self.execute_with_effect_outbox(session, intent, lane),
                        )
                        .await;
                        return self
                            .apply_post_tool_hook(
                                session,
                                intent,
                                observation,
                                web_tainted,
                                cancel,
                                sink,
                            )
                            .await;
                    }
                }
            }
        }
        match decision {
            crate::types::Decision::Deny { reason } => Observation::denial(&intent.id, reason),
            crate::types::Decision::Human => {
                // Published before the wait for the lane, not after: an agent
                // queued behind another agent's card is still blocked on a
                // person, and a watcher must be able to say so.
                sink.phase(crate::progress::Phase::AwaitingApproval {
                    action: approval_key(intent),
                });
                // Hold the lock through the answer, then drop it before execution so an
                // approved slow tool doesn't block the next card.
                let answer = {
                    let _one_gate = self.gate_serial.lock().await;
                    let mut detail = self
                        .executor
                        .preview(intent)
                        .await
                        .unwrap_or_else(|| approval_detail(intent));
                    if let Some(reason) = &hook_approval_reason {
                        detail.push_str("\n\nA pre-tool hook requires approval: ");
                        detail.push_str(reason);
                    }
                    let action = approval_key(intent);
                    self.gate.confirm(&action, Some(&detail), escalated).await
                };
                self.observe_hook(
                    session,
                    medha_extension_api::HookPoint::ApprovalDecision,
                    serde_json::json!({
                        "intent_id": intent.id,
                        "tool": intent.tool,
                        "kind": "tool",
                        "decision": format!("{answer:?}").to_lowercase(),
                    }),
                )
                .await;
                let approved = answer.approved();
                if approved {
                    self.running_tool(intent, sink);
                    let observation = self
                        .execute_with_net_retry(session, intent, web_tainted, radius, lane)
                        .await;
                    self.apply_post_tool_hook(
                        session,
                        intent,
                        observation,
                        web_tainted,
                        cancel,
                        sink,
                    )
                    .await
                } else {
                    Observation::denial(&intent.id, self.gate.denial_reason().to_string())
                }
            }
            crate::types::Decision::Allow => {
                self.running_tool(intent, sink);
                let observation = self
                    .execute_with_net_retry(session, intent, web_tainted, radius, lane)
                    .await;
                self.apply_post_tool_hook(session, intent, observation, web_tainted, cancel, sink)
                    .await
            }
        }
    }

    /// Publish which tool is running, and on what. A watcher showing "in tool"
    /// with no target cannot tell a wedged read from a long build.
    fn running_tool(&self, intent: &ToolIntent, sink: &dyn StreamSink) {
        sink.phase(crate::progress::Phase::InTool {
            tool: intent.tool.clone(),
            target: salient_arg(intent).map(str::to_string),
        });
    }

    /// Run the intent; if it failed because the sandbox denied network, offer a
    /// grant and retry. The first run under net-deny could not exfiltrate, so the
    /// grant admits a *second* action: trust-flow is re-evaluated as if the box's
    /// network were already open (a literal [`Containment::OsFsJail`]), and a
    /// web-tainted consequential action is gated exactly as it would be without
    /// confinement. The retry calls [`Self::execute_with_effect_outbox`] directly
    /// rather than recursing, so a real DNS outage matching the same signature
    /// cannot re-prompt.
    async fn execute_with_net_retry(
        &self,
        session: &Session,
        intent: &ToolIntent,
        web_tainted: bool,
        radius: Option<BlastRadius>,
        lane: Option<&Lane>,
    ) -> Observation {
        let obs = self.execute_with_effect_outbox(session, intent, lane).await;
        if !obs.net_denied || !self.executor.allows_network_retry(intent) {
            return obs;
        }
        // The first run is over; other changes need not wait on this answer.
        if let Some(lane) = lane {
            lane.held.lock().unwrap().take();
        }
        // Trust-flow escalation only, not the policy engine: argv is unchanged, so
        // policy would repeat the verdict that already admitted this intent. Given
        // `Allow` the result can only widen to `Human`, never narrow to `Deny`.
        let escalated = matches!(
            escalate_for_trust_flow(
                crate::types::Decision::Allow,
                radius,
                web_tainted,
                crate::types::Containment::OsFsJail,
            ),
            crate::types::Decision::Human
        );
        let decision = {
            let _one_gate = self.gate_serial.lock().await;
            let detail = net_grant_detail(intent, web_tainted, &obs);
            self.executor.grant_network(Some(&detail), escalated).await
        };
        self.observe_hook(
            session,
            medha_extension_api::HookPoint::ApprovalDecision,
            serde_json::json!({
                "intent_id": intent.id,
                "tool": intent.tool,
                "kind": "network_retry",
                "decision": format!("{decision:?}").to_lowercase(),
            }),
        )
        .await;
        match decision {
            crate::NetworkDecision::Deny => obs,
            crate::NetworkDecision::Once => {
                crate::network_once_scope(self.execute_with_effect_outbox(session, intent, lane))
                    .await
            }
            crate::NetworkDecision::Session | crate::NetworkDecision::Persistent => {
                // The grant already flipped the shared network flag; the retry
                // and every later command now reach the network.
                self.execute_with_effect_outbox(session, intent, lane).await
            }
        }
    }
}

/// Card detail for a network-grant prompt. Names the sandbox policy so the user
/// is not left debugging a bare DNS error, and flags exfiltration risk when the
/// command touched web content and does something consequential.
///
/// A command that merely ran out of time under a net-denying box gets separate
/// wording: nothing there proves the network was the cause, and a card that
/// asserts one about `sleep 90` teaches the user to stop reading these.
fn net_grant_detail(intent: &ToolIntent, web_tainted: bool, obs: &Observation) -> String {
    let action = approval_key(intent);
    let unproven = obs
        .payload
        .get("timed_out")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mut detail = if unproven {
        format!(
            "This command hit its deadline under a sandbox that denies network access:\n  {action}\n\
             If it was waiting on the network, its resolver error may have been hidden by a pipe. \
             Grant network access and retry?"
        )
    } else {
        format!(
            "The sandbox denied network access, so this command could not reach the network:\n  {action}\n\
             Grant network access and retry?"
        )
    };
    if web_tainted {
        detail.push_str(
            "\n\nThis command handled web-fetched content. Opening the network means it could \
             send that content out — approve only if you trust it to.",
        );
    }
    detail
}

/// Whether a call targets the memory store, and so needs kernel-computed trust
/// on the way in and its applied operation persisted on the way out. The dotted
/// names are the per-verb tools these were split across before they merged;
/// replaying a session logged then must take the same path it did originally.
fn is_memory_tool(tool: &str) -> bool {
    tool == "memory" || tool.starts_with("memory.")
}

/// Replace model-supplied trust metadata with kernel-computed values.
fn enrich_memory_intent(
    args: &mut serde_json::Value,
    taint: TrustLabel,
    window: &[ulid::Ulid],
    session_id: ulid::Ulid,
) {
    if !args.is_object() {
        *args = serde_json::json!({});
    }
    let obj = args.as_object_mut().expect("just ensured object");
    for key in [
        "trust",
        "confidence",
        "provenance",
        "sessions",
        "_trust",
        "_provenance",
        "_session",
        "_user_stated",
    ] {
        obj.remove(key);
    }
    obj.insert("_trust".into(), serde_json::json!(taint.as_str()));
    obj.insert(
        "_provenance".into(),
        serde_json::json!(window.iter().map(|u| u.to_string()).collect::<Vec<_>>()),
    );
    obj.insert("_session".into(), serde_json::json!(session_id.to_string()));
    obj.insert(
        "_user_stated".into(),
        serde_json::json!(taint == TrustLabel::User),
    );
}

/// Tighten web-tainted consequential actions unless network is confined.
fn escalate_for_trust_flow(
    decision: crate::types::Decision,
    radius: Option<BlastRadius>,
    web_tainted: bool,
    containment: crate::types::Containment,
) -> crate::types::Decision {
    use crate::types::Decision;
    let consequential = matches!(
        radius,
        Some(BlastRadius::IrreversibleLocal | BlastRadius::External)
    );
    if matches!(decision, Decision::Allow)
        && web_tainted
        && consequential
        && !containment.confines_network()
    {
        Decision::Human
    } else {
        decision
    }
}

/// The texts of the user messages the log currently ends with.
///
/// The counterpart to [`unlogged_tail`] on the durable side: what a retry after
/// a failed turn would otherwise append a second time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LoggedInput {
    id: ulid::Ulid,
    content: String,
    instruction_prefix: Option<String>,
    trust: TrustLabel,
    attachments: serde_json::Value,
}

fn logged_tail(events: &[Event]) -> Vec<LoggedInput> {
    let mut tail: Vec<LoggedInput> = Vec::new();
    for event in events.iter().rev() {
        match event.kind {
            EventKind::UserMessage => tail.push(LoggedInput {
                id: event.id,
                instruction_prefix: event.payload["instruction_prefix"]
                    .as_str()
                    .map(str::to_owned),
                content: event.payload["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                trust: event.trust,
                attachments: event
                    .payload
                    .get("attachments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!([])),
            }),
            // These events do not finish the pending user turn.
            EventKind::Interrupt | EventKind::ToolSnapshot => continue,
            _ => break,
        }
    }
    // Rebuilt in send order, so a caller matching front-to-back lines up with
    // what was actually appended.
    tail.reverse();
    tail
}

/// Index of the first user message this turn has not logged yet. A run, not one
/// message: a surface can append a typed prompt and an agent report together.
fn unlogged_tail(messages: &[Message]) -> usize {
    messages
        .iter()
        .rposition(|message| message.role != Role::User)
        .map_or(0, |last_other| last_other + 1)
}

#[cfg(test)]
mod unlogged_tail_tests {
    use super::{hydrate_ordered_log, unlogged_tail};
    use crate::events::{Event, project_ordered_messages};
    use crate::types::{Message, Role, Session, TrustLabel};

    #[test]
    fn a_prompt_and_a_report_arriving_together_are_both_new() {
        let messages = vec![
            Message::system("s"),
            Message::user("earlier"),
            Message::new(Role::Assistant, "answered"),
            Message::user("what the user typed"),
            Message::user("[background agent finished] …"),
        ];
        // Taking only the last dropped the typed prompt from the log, and the
        // rehydration that follows rebuilds context from the log.
        assert_eq!(unlogged_tail(&messages), 3);
    }

    /// A line the user genuinely sent twice must be logged twice. Matching by
    /// membership rather than by count silently swallowed the repeat.
    #[test]
    fn a_repeated_line_is_not_mistaken_for_a_replay() {
        use super::logged_tail;
        use crate::events::Event;
        use crate::types::Session;

        let session = Session {
            id: ulid::Ulid::new(),
            done: false,
            autonomy: crate::types::AutonomyLevel::Careful,
        };
        let events = vec![
            Event::user_message(&session, "again"),
            Event::user_message(&session, "again"),
            Event::tool_snapshot(
                &session,
                &serde_json::from_value(serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"), "specs": [], "sources": {}
                }))
                .unwrap(),
            ),
        ];
        let tail = logged_tail(&events);
        assert_eq!(
            tail.iter()
                .map(|input| input.content.as_str())
                .collect::<Vec<_>>(),
            vec!["again", "again"]
        );
        assert_eq!(
            logged_tail(&[Event::model_text(&session, "answered")]),
            Vec::new(),
            "an assistant turn ends the run"
        );
    }

    #[test]
    fn a_turn_that_added_nothing_logs_nothing() {
        let messages = vec![
            Message::system("s"),
            Message::user("prompt"),
            Message::new(Role::Assistant, "answer"),
        ];
        assert_eq!(unlogged_tail(&messages), messages.len());
    }

    #[test]
    fn ordered_hydration_does_not_erase_equal_admitted_inputs() {
        let session = Session::new();
        let events = vec![
            Event::user_message(&session, "again"),
            Event::user_message(&session, "again"),
            Event::user_input(&session, "again", TrustLabel::Web),
            Event::model_message(&session, &Message::new(Role::Assistant, "answer").ordered()),
        ];
        let hydrated = hydrate_ordered_log(
            &[Message::system("system")],
            project_ordered_messages(&events),
        );

        assert_eq!(hydrated.len(), 5);
        assert_eq!(hydrated[1].trust, None);
        assert_eq!(hydrated[2].trust, None);
        assert_eq!(hydrated[3].trust, Some(TrustLabel::Web));
    }
}

#[cfg(test)]
mod enrich_memory_tests {
    use super::enrich_memory_intent;
    use crate::types::TrustLabel;
    use serde_json::json;
    use ulid::Ulid;

    #[test]
    fn strips_smuggled_trust_keys_and_injects_kernel_values() {
        let mut args = json!({
            "name": "n", "claim": "c",
            "trust": "user", "confidence": "confirmed",
            "provenance": ["fake"], "_trust": "system", "_user_stated": true,
        });
        let w = [Ulid::new(), Ulid::new()];
        let sid = Ulid::new();
        enrich_memory_intent(&mut args, TrustLabel::Web, &w, sid);

        assert_eq!(args["_trust"], "web", "taint wins, smuggled 'trust' gone");
        assert_eq!(args["_user_stated"], false);
        assert_eq!(args["_session"], sid.to_string());
        assert_eq!(args["_provenance"].as_array().unwrap().len(), 2);
        assert!(args.get("trust").is_none());
        assert!(args.get("confidence").is_none());
        assert_eq!(args["name"], "n", "real args untouched");
    }

    #[test]
    fn clean_user_window_is_user_stated() {
        let mut args = json!({ "name": "n" });
        enrich_memory_intent(&mut args, TrustLabel::User, &[Ulid::new()], Ulid::new());
        assert_eq!(args["_trust"], "user");
        assert_eq!(args["_user_stated"], true);
    }

    #[test]
    fn taint_min_flows_to_the_floor() {
        assert_eq!(TrustLabel::User.min(TrustLabel::Web), TrustLabel::Web);
        assert_eq!(TrustLabel::User.min(TrustLabel::Tool), TrustLabel::Tool);
        assert_eq!(TrustLabel::Tool.min(TrustLabel::User), TrustLabel::Tool);
        assert_eq!(TrustLabel::User.min(TrustLabel::User), TrustLabel::User);
    }
}

#[cfg(test)]
mod progressive_context_tests {
    use super::{attach_discovered_context, successful_observed_path};
    use crate::{DiscoveredContext, ObsStatus, Observation, TrustLabel};
    use serde_json::json;

    #[test]
    fn progressive_context_is_visible_and_workspace_labeled() {
        let mut observation = Observation::ok("tool-1", json!("plain result"));
        attach_discovered_context(
            &mut observation,
            &DiscoveredContext {
                path: "sub/AGENTS.md".into(),
                content: "use the submodule rules".into(),
                blocked: false,
                trust: TrustLabel::Workspace,
            },
        );
        assert_eq!(observation.payload["result"], "plain result");
        assert_eq!(observation.payload["project_context"]["trust"], "workspace");
        assert_eq!(
            observation.payload["project_context"]["content"],
            "use the submodule rules"
        );

        let mut blocked = Observation::ok("tool-2", json!({ "ok": true }));
        attach_discovered_context(
            &mut blocked,
            &DiscoveredContext {
                path: "sub/CLAUDE.md".into(),
                content: "[blocked context file sub/CLAUDE.md]".into(),
                blocked: true,
                trust: TrustLabel::Workspace,
            },
        );
        assert_eq!(blocked.payload["project_context"]["blocked"], true);
        assert!(
            blocked.payload["project_context"]["content"]
                .as_str()
                .unwrap()
                .contains("blocked context file")
        );

        let mut external = Observation::ok("tool-3", json!({ "path": "/approved/file" }));
        attach_discovered_context(
            &mut external,
            &DiscoveredContext {
                path: "/approved/AGENTS.md".into(),
                content: "external guidance".into(),
                blocked: false,
                trust: TrustLabel::Tool,
            },
        );
        assert_eq!(external.payload["project_context"]["trust"], "tool");
    }

    #[test]
    fn only_successful_tools_that_report_a_touched_path_trigger_discovery() {
        let ok = Observation::ok("ok", json!({ "path": "src/lib.rs" }));
        assert_eq!(
            successful_observed_path(&ok),
            Some(std::path::Path::new("src/lib.rs"))
        );

        for status in [
            ObsStatus::Denied,
            ObsStatus::Rejected,
            ObsStatus::SchemaInvalid,
            ObsStatus::Error,
        ] {
            let observation = Observation {
                intent_id: "failed".into(),
                status,
                payload: json!({ "path": "/tmp/untrusted/file" }),
                media: Vec::new(),
                relayed_trust: None,
                net_denied: false,
                model_text: None,
            };
            assert!(
                successful_observed_path(&observation).is_none(),
                "a failed/denied path request must not discover context"
            );
        }
        assert!(
            successful_observed_path(&Observation::ok("no-path", json!({ "ok": true }))).is_none(),
            "success alone is insufficient when the tool did not report a touched path"
        );
    }
}

#[cfg(test)]
mod approval_key_tests {
    use super::approval_key;
    use crate::types::ToolIntent;
    use serde_json::json;

    fn intent(tool: &str, args: serde_json::Value) -> ToolIntent {
        ToolIntent {
            id: "1".into(),
            tool: tool.into(),
            args,
        }
    }

    #[test]
    fn approval_key_scopes_to_the_salient_arg_not_just_the_tool() {
        let a = approval_key(&intent("shell.exec", json!({ "command": "cargo build" })));
        let b = approval_key(&intent("shell.exec", json!({ "command": "rm -rf build" })));
        assert_eq!(a, "shell.exec: cargo build");
        assert_ne!(a, b, "distinct commands must not share an auto-approve key");
        assert_eq!(
            approval_key(&intent("edit", json!({ "path": "x.rs" }))),
            "edit: x.rs"
        );
        assert_eq!(
            approval_key(&intent("update_plan", json!({}))),
            "update_plan"
        );
    }
}

#[cfg(test)]
mod trust_flow_tests {
    use super::escalate_for_trust_flow;
    use crate::types::{BlastRadius, Containment, Decision};

    #[test]
    fn escalates_web_tainted_consequential_action_in_a_leaky_box() {
        // Irreversible action, web-tainted, FS jail but network reachable → gate.
        let d = escalate_for_trust_flow(
            Decision::Allow,
            Some(BlastRadius::IrreversibleLocal),
            true,
            Containment::OsFsJail,
        );
        assert!(matches!(d, Decision::Human));
        // External action on the bare host → gate.
        let d = escalate_for_trust_flow(
            Decision::Allow,
            Some(BlastRadius::External),
            true,
            Containment::None,
        );
        assert!(matches!(d, Decision::Human));
    }

    #[test]
    fn no_escalation_when_the_box_confines_network() {
        // A network-denied jail can't exfiltrate, so the same action runs freely.
        let d = escalate_for_trust_flow(
            Decision::Allow,
            Some(BlastRadius::IrreversibleLocal),
            true,
            Containment::OsFsJailNoNet,
        );
        assert!(matches!(d, Decision::Allow));
    }

    #[test]
    fn no_escalation_without_taint_or_for_low_risk_classes() {
        // No web taint → unchanged.
        assert!(matches!(
            escalate_for_trust_flow(
                Decision::Allow,
                Some(BlastRadius::IrreversibleLocal),
                false,
                Containment::None
            ),
            Decision::Allow
        ));
        // Read-class is never consequential.
        assert!(matches!(
            escalate_for_trust_flow(
                Decision::Allow,
                Some(BlastRadius::Read),
                true,
                Containment::None
            ),
            Decision::Allow
        ));
        // Reversible-local (snapshotted + jailed) is left alone to avoid nagging.
        assert!(matches!(
            escalate_for_trust_flow(
                Decision::Allow,
                Some(BlastRadius::ReversibleLocal),
                true,
                Containment::None
            ),
            Decision::Allow
        ));
    }

    #[test]
    fn only_tightens_never_relaxes() {
        // A denial stays denied even under taint (escalation is one-directional).
        let d = escalate_for_trust_flow(
            Decision::Deny {
                reason: "blocked".into(),
            },
            Some(BlastRadius::External),
            true,
            Containment::None,
        );
        assert!(matches!(d, Decision::Deny { .. }));
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn backoff_grows_and_stays_within_its_jitter_band() {
        for attempt in 1..=5u32 {
            let curve = RETRY_BASE * (1u32 << (attempt - 1));
            let spread = curve / 4;
            for _ in 0..64 {
                let actual = retry_backoff(attempt);
                assert!(
                    actual >= curve - spread && actual <= curve + spread,
                    "attempt {attempt}: {actual:?} outside {curve:?} ±{spread:?}"
                );
            }
        }
    }

    #[test]
    fn backoff_is_capped_however_many_attempts() {
        for attempt in 1..64u32 {
            assert!(
                retry_backoff(attempt) <= RETRY_MAX_BACKOFF,
                "attempt {attempt} exceeded the ceiling"
            );
        }
    }

    #[test]
    fn the_ceiling_leaves_room_for_a_rate_limit_to_clear() {
        // The failure this replaces: a 250ms curve over 3 attempts gave up
        // 1.75s after the first refusal, well inside any real limit window.
        let total: std::time::Duration = (1..=MAX_TURN_RETRIES).map(retry_backoff).sum();
        assert!(total >= std::time::Duration::from_secs(10), "{total:?}");
    }

    #[test]
    fn a_stated_retry_after_is_preferred_over_the_curve() {
        let asked = std::time::Duration::from_secs(7);
        let throttled = crate::provider::ProviderError::Throttled {
            status: 429,
            retry_after: asked,
            body: "slow down".into(),
        };
        assert!(throttled.is_retryable());
        assert_eq!(throttled.retry_after(), Some(asked));
        assert_eq!(
            throttled.retry_after().unwrap_or_else(|| retry_backoff(1)),
            asked
        );
    }

    /// A stored result is previewed by whole lines from its start and its end,
    /// so a test run's first error and its final summary both stay in view.
    #[test]
    fn a_stored_result_is_previewed_by_its_first_and_last_lines() {
        let content: String = (1..=5_000).map(|n| format!("line {n}\n")).collect();
        let preview = spilled_preview(&content, "abc");
        let (head, rest) = preview.split_once("\n[… ").unwrap();
        let (note, tail) = rest.split_once("]\n").unwrap();
        assert!(head.starts_with("line 1\nline 2\n") && head.ends_with(char::is_numeric));
        assert!(tail.ends_with("line 4999\nline 5000\n") && tail.starts_with("line "));
        assert!(
            note.contains(&format!("of {} bytes not shown", content.len())),
            "{note}"
        );
        assert!(note.contains("read hash=\"abc\""));
        let shown = head.len() + tail.len();
        assert!((1_500..=SPILL_PREVIEW + 20).contains(&shown), "{shown}");
        assert!(tail.len() > head.len(), "the end gets the larger share");
    }

    #[test]
    fn an_unthrottled_transient_falls_back_to_the_curve() {
        let stalled = crate::provider::ProviderError::Stream("stream stalled".into());
        assert!(stalled.is_retryable());
        assert_eq!(stalled.retry_after(), None);
    }
}
