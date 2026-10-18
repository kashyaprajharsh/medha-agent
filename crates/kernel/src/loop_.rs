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

mod admitted;
mod dispatch;
mod session;
mod turn;

#[path = "budget_finish.rs"]
mod budget_finish;

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

fn relayed_source(base: TrustLabel, source: TrustLabel) -> TrustLabel {
    if base == TrustLabel::Tool && source == TrustLabel::Skill {
        source
    } else {
        base.min(source)
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
    /// Stored results that came from outside; reading one back is outside text too.
    outside_stored: std::sync::Mutex<std::collections::HashMap<String, TrustLabel>>,
    progressive_context: Option<Arc<dyn crate::context::ProgressiveContext>>,
    request_context: Option<Arc<dyn crate::context::RequestContext>>,
    max_parallel_tools: usize,
    /// Times a turn waits for a server that is away, after all else is used up.
    server_waits: u32,
    /// (per result, per round) limits set in config; each one unset is worked
    /// out from the model's context window.
    result_limits: (Option<usize>, Option<usize>),
    /// Removed from every tool result before the model reads it.
    secrets: Arc<crate::redact::Secrets>,
    pub egress: Arc<crate::egress::Guard>,
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
            outside_stored: Default::default(),
            progressive_context: None,
            request_context: None,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            server_waits: crate::recovery::WAITS,
            result_limits: (None, None),
            secrets: Arc::default(),
            egress: Arc::default(),
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
            outside_stored: Default::default(),
            progressive_context: self.progressive_context.clone(),
            request_context: self.request_context.clone(),
            max_parallel_tools: self.max_parallel_tools,
            server_waits: self.server_waits,
            result_limits: self.result_limits,
            secrets: Arc::clone(&self.secrets),
            egress: Arc::clone(&self.egress),
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
        let default = own.or_else(|| self.provider.list_price());
        if own_model {
            self.request_context
                .as_ref()
                .map_or(default, |context| context.pricing(default))
        } else {
            default
        }
    }

    /// Set the operator's own rate, so the governor meters real dollars. It is
    /// a rate for the model in use now, not for one the chat changes to later.
    pub fn with_pricing(mut self, pricing: Option<crate::types::Pricing>) -> Self {
        self.priced_model = pricing.map(|_| self.provider.context_identity());
        self.pricing = pricing;
        self
    }

    pub fn with_request_context(
        mut self,
        context: Arc<dyn crate::context::RequestContext>,
    ) -> Self {
        self.priced_model = Some(self.provider.context_identity());
        self.request_context = Some(context);
        self
    }

    pub fn refresh_memory(&self, memory: &str) -> Option<String> {
        self.request_context.as_ref()?.refresh_memory(memory)
    }
    pub fn owns_memory_slot(&self) -> bool {
        self.request_context
            .as_ref()
            .is_some_and(|context| context.owns_memory_slot())
    }

    pub fn with_child_context(mut self, workspace: Option<&std::path::Path>) -> Self {
        if let Some(context) = self.request_context.as_ref().and_then(|context| {
            context.fork(
                workspace,
                self.executor
                    .specs()
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect(),
            )
        }) {
            if let Some(policy) = context.policy() {
                self.policy = policy;
            }
            let summary = context.clone();
            if let Some(engine) = self.context.fork_with_summary_prompt(Arc::new(move || {
                summary.summary_prompt().unwrap_or_default()
            })) {
                self.context = engine;
            }
            self.request_context = Some(context);
        }
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
        if let std::borrow::Cow::Owned(clean) = self.egress.redact(&text) {
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
            if let std::borrow::Cow::Owned(clean) = self.egress.redact(&message.content) {
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
                if let ContentPart::ToolResult(result) = part
                    && let std::borrow::Cow::Owned(clean) = self.egress.redact(&result.content)
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
        let fallback = || self.context.estimate_prepared(prepared);
        match counted {
            Ok(Some(count)) if count.request_fingerprint != prepared.request_fingerprint => {
                if strict {
                    Err(KernelError::Provider(
                        "strict token accounting rejected a stale request fingerprint".into(),
                    ))
                } else {
                    Ok(fallback())
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
            Ok(None) => Ok(fallback()),
            Err(error) if strict => Err(KernelError::Provider(error.to_string())),
            Err(_) => Ok(fallback()),
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

    async fn begin_context_message(
        &self,
        session: &Session,
        events: &mut Vec<Event>,
        sink: &dyn StreamSink,
    ) -> Result<(), KernelError> {
        if let Some(context) = &self.request_context {
            for (key, review) in context.begin_message(events, sink) {
                let event = self
                    .log
                    .append(Event::project_trust_notice(session, key, review))
                    .await?;
                sink.project_trust(&event.payload["review"]);
                events.push(event);
            }
        }
        Ok(())
    }

    async fn run_session_inner(
        &self,
        session: &Session,
        messages: Vec<Message>,
        budget: crate::budgets::Budget,
        sink: &dyn StreamSink,
        mut interrupts: Option<crate::interrupts::InterruptQueue>,
    ) -> Result<(Vec<Message>, StopReason), KernelError> {
        session::run(self, session, messages, budget, sink, &mut interrupts).await
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

/// An action that cannot be undone or reaches outside, taken after web text
/// was read, where the box it runs in leaves the network open.
fn follows_web(
    radius: Option<BlastRadius>,
    web_tainted: bool,
    containment: crate::types::Containment,
) -> bool {
    let consequential = matches!(
        radius,
        Some(BlastRadius::IrreversibleLocal | BlastRadius::External)
    );
    web_tainted && consequential && !containment.confines_network()
}

/// Tighten web-tainted consequential actions unless network is confined.
fn escalate_for_trust_flow(
    decision: crate::types::Decision,
    radius: Option<BlastRadius>,
    web_tainted: bool,
    containment: crate::types::Containment,
) -> crate::types::Decision {
    use crate::types::Decision;
    if matches!(decision, Decision::Allow) && follows_web(radius, web_tainted, containment) {
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
#[path = "trust_flow_tests.rs"]
mod trust_flow_tests;

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
