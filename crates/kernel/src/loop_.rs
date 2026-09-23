//! The multi-turn kernel loop: models propose blocks and the kernel validates,
//! authorizes, verifies, and executes them.

use crate::context::ContextEngine;
use crate::errors::KernelError;
use crate::events::{Event, EventKind, EventLog};
use crate::executor::Executor;
use crate::provider::{InputTokenCount, Provider, TokenAccountingMode, TokenCountQuality};
use crate::sink::StreamSink;
use crate::types::{
    BlastRadius, Block, CompiledContext, ContentPart, Message, ModelMessage, Observation,
    ReasoningPart, Role, Session, TextPart, ToolCallPart, ToolCategory, ToolIntent, TrustLabel,
};
use futures::stream::{self, StreamExt};
use std::sync::Arc;

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
    for part in &message.parts {
        match part {
            ContentPart::Text(part) => text.push_str(&part.text),
            ContentPart::Reasoning(part) => {
                if let Some(summary) = &part.text {
                    reasoning.push_str(summary);
                }
            }
            ContentPart::ToolCall(part) => intents.push(ToolIntent {
                id: part.id.clone(),
                tool: part.tool.clone(),
                args: part.args.clone(),
            }),
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
    /// Reads images for a route whose own model cannot. `None` means an image
    /// reaching such a route fails the turn instead of being dropped.
    vision: Option<Arc<dyn crate::vision::VisionDescriber>>,
    progressive_context: Option<Arc<dyn crate::context::ProgressiveContext>>,
    max_parallel_tools: usize,
    pricing: Option<crate::types::Pricing>,
    /// Serializes human-gate prompts: parallel tool dispatch must not pop
    /// several approval cards at once.
    ///
    /// Shared with every kernel derived from this one, because the operator is
    /// shared too. A per-session lock only orders one session's own cards, so a
    /// tree of agents would each hold their own lane and race the single surface
    /// they all prompt through.
    gate_serial: Arc<futures::lock::Mutex<()>>,
    /// Orders state-changing turns across concurrent root/child sessions. The
    /// guard spans execution through durable observation logging, so another
    /// mutation cannot commit in the gap before replay learns about this one.
    mutation_serial: Arc<tokio::sync::Mutex<()>>,
    settle_grace: std::time::Duration,
    /// Sessions whose start hooks already ran in this process, shared with
    /// derived kernels so a resumed tree fires them once.
    started_sessions: Arc<std::sync::Mutex<std::collections::HashSet<ulid::Ulid>>>,
    /// Derived for a sub-agent: agent start/stop hooks describe it instead of
    /// session and prompt hooks.
    sub_agent: bool,
}

/// Tool-result payloads larger than this spill to the artifact store and are
/// replaced in-context by a head and a `read_artifact` reference. Public
/// because the paging tool must size its pages to survive this threshold: a
/// page that spills costs a round trip instead of saving one.
pub const SPILL_THRESHOLD: usize = 16_000;

/// Absolute per-turn ingestion limits. These are deliberately independent of
/// provider/model limits: a broken or adversarial stream must not be able to
/// grow the kernel's in-memory transcript forever.
const MAX_TOOL_INTENTS_PER_TURN: usize = 64;
const MAX_PROVIDER_STREAM_BLOCKS: usize = 16_384;
const MAX_PROVIDER_STREAM_BYTES: usize = 8 * 1024 * 1024;

/// How many times a turn's model stream is retried on a transient provider
/// failure (429 / 5xx / network drop / stalled stream) before giving up.
const MAX_TURN_RETRIES: u32 = 5;

/// First backoff nap between stream retries.
const RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(500);

/// Ceiling on one backoff nap. A rate limit needs tens of seconds to clear, so
/// a sub-second curve spends the entire retry budget inside the window that was
/// refusing us and reports failure before the limit has lifted.
const RETRY_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
/// Bound measure → compact → remeasure so a pathological compressor cannot
/// rewrite the same turn indefinitely.
const MAX_COMPACTION_PASSES: u32 = 3;

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
            progressive_context: None,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            pricing: None,
            gate_serial: Arc::new(futures::lock::Mutex::new(())),
            mutation_serial: Arc::new(tokio::sync::Mutex::new(())),
            settle_grace: TOOL_SETTLE_GRACE,
            started_sessions: Arc::default(),
            sub_agent: false,
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
            progressive_context: self.progressive_context.clone(),
            max_parallel_tools: self.max_parallel_tools,
            pricing: self.pricing,
            // Shared, not rebuilt: see the field's own note. A fresh lane here
            // is what let three children pop three cards at one surface.
            gate_serial: Arc::clone(&self.gate_serial),
            mutation_serial: Arc::clone(&self.mutation_serial),
            settle_grace: self.settle_grace,
            started_sessions: Arc::clone(&self.started_sessions),
            sub_agent: true,
        }
    }

    /// Whether this route can carry images itself: the wire contract must
    /// support them and the model must not declare that it cannot see.
    fn route_reads_images(&self, force_text: bool) -> Result<bool, KernelError> {
        if force_text {
            return Ok(false);
        }
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

    /// Set resolved model pricing so the governor meters real dollars.
    pub fn with_pricing(mut self, pricing: Option<crate::types::Pricing>) -> Self {
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

    /// Override the post-cancel tool settle window (tests use a short one).
    pub fn with_settle_grace(mut self, grace: std::time::Duration) -> Self {
        self.settle_grace = grace;
        self
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

    /// Spill an oversized tool-result payload to the artifact store, returning a
    /// truncated head + a `read_artifact` pointer. The full payload is still in
    /// the event log, so nothing is lost; only the *live context* shrinks.
    async fn maybe_spill(&self, content: String) -> String {
        if content.len() <= SPILL_THRESHOLD {
            return content;
        }
        match Arc::clone(&self.artifacts)
            .put_async(content.as_bytes().to_vec())
            .await
        {
            Ok(hash) => {
                let head: String = content.chars().take(2_000).collect();
                format!(
                    "{head}\n\n[SHOWING FIRST 2000 CHARS of {} total bytes — the rest is NOT \
                     lost. Continue reading it: call read with hash=\"{hash}\" \
                     (offset, length) to page through the remainder, or re-read a specific \
                     line range with read offset+limit. Do NOT report to the user that the \
                     output was truncated or that you can't see it — page through it and \
                     finish the task.]",
                    content.len()
                )
            }
            Err(_) => content, // spill failed → keep full rather than lose data
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
        for message in messages {
            if message.role == Role::Tool && message.content.len() > SPILL_THRESHOLD {
                message.content = self.maybe_spill(std::mem::take(&mut message.content)).await;
            }
        }
        for message in ordered_messages {
            if message.role != Role::Tool {
                continue;
            }
            for part in &mut message.parts {
                match part {
                    ContentPart::ToolResult(result) if result.content.len() > SPILL_THRESHOLD => {
                        result.content =
                            self.maybe_spill(std::mem::take(&mut result.content)).await;
                    }
                    ContentPart::Text(text) if text.text.len() > SPILL_THRESHOLD => {
                        text.text = self.maybe_spill(std::mem::take(&mut text.text)).await;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Override the per-turn concurrency cap.
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
            let fut = self.dispatch_one(session, &intent, web_tainted, &cancel, sink);
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
                self.execute_admitted(
                    session,
                    intent,
                    web_tainted,
                    cancel.clone(),
                    wall_deadline,
                    Arc::clone(settle_deadline),
                    sink,
                )
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
        let event = self
            .log
            .append(Event::tool_obs(session, &obs, trust))
            .await?;
        crate::events::strip_private_observation_fields(&mut obs.payload);
        window_events.push(event.id);
        *window_taint = window_taint.min(trust);
        // Once untrusted web content lands, taint the following provider turn
        // so consequential actions derived from it get escalated.
        if matches!(trust, TrustLabel::Web) {
            *web_tainted = true;
        }
        let ok = matches!(obs.status, crate::types::ObsStatus::Ok);
        sink.tool_result_with_id(&id, &tool, ok, &obs.payload);
        let content = self
            .maybe_spill(serde_json::to_string(&obs.payload).unwrap_or_default())
            .await;
        let message = Message::tool_result(&id, content);
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
        mut messages: Vec<Message>,
        budget: crate::budgets::Budget,
        sink: &dyn StreamSink,
        mut interrupts: Option<crate::interrupts::InterruptQueue>,
    ) -> Result<(Vec<Message>, StopReason), KernelError> {
        // `None` (headless) → a token that never trips; every cancel path is dead.
        let cancel = interrupts.as_ref().map(|q| q.token()).unwrap_or_default();
        let planning = session.autonomy == crate::types::AutonomyLevel::Plan;
        let specs: Vec<_> = self
            .executor
            .specs()
            .into_iter()
            .filter(|spec| {
                !planning || self.executor.blast_radius(&spec.name) == Some(BlastRadius::Read)
            })
            .collect();
        // Tool definitions are sent every turn and count toward the request.
        self.context.note_tools(&specs);
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
        let already = logged_tail(&prior_events);
        // Retried input is already durable but still belongs to this evidence
        // window. Skipping its append must not also erase its provenance or
        // upgrade a Web/Tool report back to User.
        window_events.extend(already.iter().map(|input| input.id));
        for input in &already {
            window_taint = window_taint.min(input.trust);
        }
        let mut logged_cursor = 0;
        let fresh = unlogged_tail(&messages);
        for message in &messages[fresh..] {
            // Match the durable suffix in order and with its trust label.
            // Membership matching reordered duplicate lines, while comparing
            // text alone could treat a Tool/Web report as a trusted retry.
            let trust = message.trust.unwrap_or(TrustLabel::User);
            if already.get(logged_cursor).is_some_and(|input| {
                input.content == message.content
                    && input.trust == trust
                    && input.attachments == serde_json::json!(message.attachments)
            }) {
                logged_cursor += 1;
                continue;
            }
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
            .map(|message| message.content.as_str())
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
        let mut ordered_messages: Vec<ModelMessage> =
            messages.iter().map(Message::ordered).collect();
        let logged_events = self.log.checked_events(session.id).await?;
        tracing::info!(
            elapsed_ms = history_started.elapsed().as_millis() as u64,
            "session history prepared"
        );
        let has_checkpoint = logged_events.iter().any(|event| {
            event.kind == EventKind::Compaction
                && crate::events::has_valid_compaction_snapshot(&event.payload)
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
        // Spill after hydration — checkpoint replay would silently undo an earlier
        // spill. Both views are rewritten independently.
        self.spill_hydrated_tool_results(&mut messages, &mut ordered_messages)
            .await;
        // Flips true once a web-labeled observation enters this request, so a
        // later consequential action derived from it escalates. Request-scoped.
        let mut web_tainted = messages
            .iter()
            .any(|message| message.trust == Some(TrustLabel::Web));
        loop {
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
            let mut image_fallback_retried = false;
            let mut force_text_images = false;
            let mut compaction_passes = 0u32;
            let (assistant, canonical, intents, usage, turn_interrupted) = 'model_call: loop {
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
                    // Transient session instructions stay out of durable history:
                    // resuming or leaving Plan must not retain a stale restriction.
                    if planning {
                        let directive = Message::system(
                            "Plan mode is active. Investigate using read-only tools and present an actionable plan with affected files, risks, and validation steps. Do not implement changes, execute commands, or delegate work. The user must switch to careful/normal/yolo mode before implementation.",
                        );
                        candidate
                            .ordered
                            .as_mut()
                            .expect("ordered candidate")
                            .insert(0, directive.ordered());
                        candidate.messages.insert(0, directive);
                    }
                    // Images either go to the model as pixels or reach it as a
                    // described, clearly-labelled substitute. They are never
                    // dropped on the way to a model that cannot see.
                    if self.route_reads_images(force_text_images)? {
                        crate::artifacts::resolve_media(
                            &mut candidate,
                            Arc::clone(&self.artifacts),
                        )
                        .await
                        .map_err(KernelError::Provider)?;
                    } else {
                        let describer = self
                            .vision
                            .clone()
                            .unwrap_or_else(|| Arc::new(crate::vision::NoVision));
                        let described = crate::vision::describe_media(
                            &mut candidate,
                            Arc::clone(&self.artifacts),
                            describer.as_ref(),
                        )
                        .await
                        .map_err(KernelError::Provider)?;
                        if described > 0 {
                            sink.notice(&format!(
                                "{described} image(s) described by {} — this model has no image input",
                                describer.model()
                            ));
                        }
                    }
                    let prepared = self
                        .provider
                        .prepare_request(&candidate)
                        .map_err(|error| KernelError::Provider(error.to_string()))?;

                    self.context.begin_request(&format!(
                        "{}:{}:{}:{limits:?}",
                        session.id,
                        self.provider.context_identity(),
                        prepared.model,
                    ));

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

                    sink.compacting(true);
                    let compiled = self
                        .context
                        .compile_controlled(&messages, input_limit, &compile_control)
                        .await;
                    sink.compacting(false);
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
                    self.log
                        .append(Event::compaction_snapshot(
                            session,
                            compiled.before_tokens,
                            compiled.after_tokens,
                            compiled.summary.as_deref(),
                            &compiled.messages,
                            &compacted_ordered,
                        ))
                        .await?;
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
                    )
                    .await
                {
                    Ok(t) => {
                        if self.provider.image_input_mode() == crate::ImageInputMode::Auto
                            && context_has_media(&prepared.context)
                        {
                            self.provider
                                .observe_image_support(crate::ImageSupport::Supported);
                        }
                        break t;
                    }
                    Err(KernelError::UnsupportedImage)
                        if !image_fallback_retried
                            && self.provider.image_input_mode() == crate::ImageInputMode::Auto
                            && self.provider.image_support() == crate::ImageSupport::Unknown =>
                    {
                        image_fallback_retried = true;
                        force_text_images = true;
                        self.provider
                            .observe_image_support(crate::ImageSupport::Unsupported);
                        sink.notice(
                            "native image input was rejected; retrying once through auxiliary vision",
                        );
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
                    Err(e) => return Err(e),
                }
            };
            if let Some(u) = usage {
                self.context.update_usage(u.prompt_tokens, u.total_tokens);
            }
            if let Some(p) = self.pricing {
                sink.cost(gov.cost_usd(), p.indicative);
            }
            // Interrupted mid-stream, or cancelled between the stream ending
            // and dispatch: keep visible content but drop un-admitted calls.
            // Persist the exact ordered remainder only after that decision, so
            // replay can never contain a call which dispatch never admitted.
            if turn_interrupted || cancel.is_cancelled() {
                let canonical = strip_tool_calls(&canonical);
                if !assistant.content.is_empty() {
                    self.log
                        .append(Event::model_text(session, &assistant.content))
                        .await?;
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
            if !assistant.content.is_empty() {
                self.log
                    .append(Event::model_text(session, &assistant.content))
                    .await?;
            }
            self.log
                .append(Event::model_message(session, &canonical))
                .await?;
            ordered_messages.push(canonical);
            messages.push(assistant);

            let finishing = intents.is_empty();

            // Model-supplied trust/confidence/provenance is stripped and replaced
            // with taint-window values, which stop at this turn's dispatch.
            let mut intents = intents;
            for it in &mut intents {
                if is_memory_tool(&it.tool) {
                    enrich_memory_intent(&mut it.args, window_taint, &window_events, session.id);
                }
            }

            // Dispatch admission: intents are logged HERE — after the cancel
            // check, immediately before execution — so a logged intent always
            // gets an observation (real or synthesized). Replay order per id is
            // intent → policy.decision → observation.
            for it in &intents {
                self.log.append(Event::model_intent(session, it)).await?;
            }
            // Notify the surface of the calls before they run (live feedback).
            for it in &intents {
                sink.tool_call_with_id(&it.id, &it.tool, &it.args);
            }
            // Blast radius, not tool name, determines whether verification runs.
            let modified_files = intents.iter().any(|i| {
                matches!(
                    self.executor.blast_radius(&i.tool),
                    Some(BlastRadius::ReversibleLocal | BlastRadius::IrreversibleLocal)
                )
            });

            // Reads run concurrently; every mutation gets a hard barrier. If two
            // writes commit B→A while the log records A→B, replay disagrees with
            // live state. Mutation keys come from the executor, not tool names.
            //
            // Scoped to one mutation's execution and durable observation. Holding
            // it across a later `agent.wait` would deadlock on a child's own write.
            let dispatch_cancel = cancel.clone();
            let dispatch_wall_deadline = gov.deadline();
            let dispatch_settle_deadline = Arc::new(std::sync::OnceLock::new());
            // Every intent in this batch was proposed from the same prior model
            // input. A web result persisted early below taints the *next* turn,
            // not sibling calls the model had already emitted without seeing it.
            let dispatch_web_tainted = web_tainted;
            let mut read_batch = Vec::new();
            for intent in intents {
                if let Some(mutation_key) = self.executor.mutation_key(&intent) {
                    if !read_batch.is_empty() {
                        let settled = self
                            .settle_reads(
                                session,
                                std::mem::take(&mut read_batch),
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
                    // Shared by every kernel derived from this one. Do not let a
                    // competing mutation commit until this one's observation
                    // (and MemoryWrite projection event, where applicable) is
                    // durable. Release before any following read/wait call.
                    let mutation_guard = tokio::select! {
                        biased;
                        _ = wait_for_deadline(dispatch_wall_deadline) => None,
                        _ = dispatch_cancel.cancelled() => None,
                        guard = self.mutation_serial.lock() => Some(guard),
                    };
                    let Some(mutation_guard) = mutation_guard else {
                        let id = intent.id;
                        let tool = intent.tool;
                        let reason = if dispatch_wall_deadline
                            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
                        {
                            "[interrupted] task wall-clock deadline elapsed while waiting for the mutation lane"
                        } else {
                            "[interrupted] cancelled before the mutation lane became available"
                        };
                        self.persist_admitted(
                            session,
                            id.clone(),
                            tool,
                            Observation::error(&id, reason),
                            None,
                            &mut window_events,
                            &mut window_taint,
                            &mut web_tainted,
                            &mut ordered_messages,
                            &mut messages,
                            sink,
                        )
                        .await?;
                        continue;
                    };
                    // A second process has its own in-memory mutex, so the durable
                    // writer lane comes from the log backend. Lease failure is a
                    // settled tool error — every admitted intent gets an observation.
                    let lease = tokio::select! {
                        biased;
                        _ = wait_for_deadline(dispatch_wall_deadline) => None,
                        _ = dispatch_cancel.cancelled() => None,
                        result = self.log.acquire_mutation_lease(&mutation_key) => Some(result),
                    };
                    let (durable_lease, executed) = match lease {
                        None => {
                            let id = intent.id;
                            let tool = intent.tool;
                            let reason = if dispatch_wall_deadline
                                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
                            {
                                "[interrupted] task wall-clock deadline elapsed before the mutation lease became available"
                            } else {
                                "[interrupted] cancelled before the mutation lease became available"
                            };
                            (
                                None,
                                (id.clone(), tool, Observation::error(&id, reason), None),
                            )
                        }
                        Some(Ok(lease)) => {
                            let result = self
                                .execute_admitted(
                                    session,
                                    intent,
                                    dispatch_web_tainted,
                                    dispatch_cancel.clone(),
                                    dispatch_wall_deadline,
                                    Arc::clone(&dispatch_settle_deadline),
                                    sink,
                                )
                                .await;
                            (Some(lease), result)
                        }
                        Some(Err(error)) => {
                            let id = intent.id;
                            let tool = intent.tool;
                            let obs = Observation::error(
                                &id,
                                format!(
                                    "state change was not started because its mutation lease \
                                         could not be acquired: {error}"
                                ),
                            );
                            (None, (id, tool, obs, None))
                        }
                    };
                    let (id, tool, obs, discovered) = executed;
                    self.persist_admitted(
                        session,
                        id,
                        tool,
                        obs,
                        discovered,
                        &mut window_events,
                        &mut window_taint,
                        &mut web_tainted,
                        &mut ordered_messages,
                        &mut messages,
                        sink,
                    )
                    .await?;
                    drop(durable_lease);
                    drop(mutation_guard);
                } else {
                    read_batch.push(intent);
                }
            }
            if !read_batch.is_empty() {
                let settled = self
                    .settle_reads(
                        session,
                        read_batch,
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
                    self.log
                        .append(Event::user_input(session, &feedback, TrustLabel::Tool))
                        .await?;
                    let message = Message::user(feedback).carrying(TrustLabel::Tool);
                    ordered_messages.push(message.ordered());
                    messages.push(message);
                }
            }
            if finishing
                && self
                    .task_completion_hooks(
                        session,
                        completion_verified,
                        hook_continuations,
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
        let mut output_limit_retried = false;
        let mut request = prepared.clone();
        let (text, reasoning, intents, canonical, usage, interrupted) = loop {
            let reservation = governor
                .reserve_model(prepared_input_tokens, reserved_output_tokens, self.pricing)
                .map_err(KernelError::Budget)?;
            sink.phase(crate::progress::Phase::Generating);
            match self
                .stream_turn(&request, sink, cancel, wall_deadline)
                .await
            {
                Ok(data) => {
                    reservation
                        .reconcile(data.4, self.pricing)
                        .map_err(KernelError::Budget)?;
                    break data;
                }
                Err((e, emitted)) => {
                    // No authoritative usage survived this attempt. The request
                    // may nevertheless have reached the provider, so consume
                    // its full worst-case reservation.
                    reservation
                        .reconcile(None, self.pricing)
                        .map_err(KernelError::Budget)?;
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
                                    == crate::ImageSupport::Unknown
                                && context_has_media(&request.context) =>
                        {
                            return Err(KernelError::UnsupportedImage);
                        }
                        crate::provider::ProviderFailure::UnsupportedImage => {}
                        crate::provider::ProviderFailure::Transient
                        | crate::provider::ProviderFailure::Fatal => {}
                    }
                    if e.is_retryable() && attempt < MAX_TURN_RETRIES {
                        attempt += 1;
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
                    return Err(KernelError::Provider(e.to_string()));
                }
            }
        };
        // `run_session` appends the canonical message only after dispatch admission.
        if !reasoning.is_empty() {
            self.log
                .append(Event::model_reasoning(session, &reasoning))
                .await?;
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
        let mut stream_blocks = 0usize;
        let mut stream_bytes = 0usize;
        loop {
            let block = tokio::select! {
                block = stream.next() => block,
                _ = cancel.cancelled() => {
                    // Cancelled mid-stream: keep what streamed (the user saw
                    // it), drop un-dispatched intents — they were never
                    // admitted, so nothing in the log dangles.
                    let canonical = completed
                        .as_ref()
                        .map(strip_tool_calls)
                        .unwrap_or_else(|| strip_tool_calls(&message_from_stream_parts(parts)));
                    return Ok((text, reasoning, Vec::new(), canonical, usage, true));
                }
                _ = wait_for_deadline(wall_deadline) => {
                    let canonical = completed
                        .as_ref()
                        .map(strip_tool_calls)
                        .unwrap_or_else(|| strip_tool_calls(&message_from_stream_parts(parts)));
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
                    emitted = true;
                    sink.text(&t);
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
                    sink.tool_started(&name, target.as_deref());
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
                Err(e) => return Err((e, emitted)),
            }
        }
        let canonical = completed.unwrap_or_else(|| message_from_stream_parts(parts));
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
            sink.text(&canonical_text);
            text = canonical_text;
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

    async fn execute_with_effect_outbox(
        &self,
        session: &Session,
        intent: &ToolIntent,
    ) -> Observation {
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
        let request = crate::hooks::HookRequest::new(session.id.to_string(), point, trust, payload);
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
        observation
    }

    async fn dispatch_one(
        &self,
        session: &Session,
        intent: &ToolIntent,
        web_tainted: bool,
        cancel: &tokio_util::sync::CancellationToken,
        sink: &dyn StreamSink,
    ) -> Observation {
        let radius = self.executor.blast_radius(&intent.tool);
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
                            self.execute_with_effect_outbox(session, intent),
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
                let approved = {
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
                    self.gate
                        .confirm(&action, Some(&detail), escalated)
                        .await
                        .approved()
                };
                if approved {
                    self.running_tool(intent, sink);
                    let observation = self
                        .execute_with_net_retry(session, intent, web_tainted, radius)
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
                    .execute_with_net_retry(session, intent, web_tainted, radius)
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
    ) -> Observation {
        let obs = self.execute_with_effect_outbox(session, intent).await;
        if !obs.net_denied || !self.executor.allows_network_retry(intent) {
            return obs;
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
        match decision {
            crate::NetworkDecision::Deny => obs,
            crate::NetworkDecision::Once => {
                crate::network_once_scope(self.execute_with_effect_outbox(session, intent)).await
            }
            crate::NetworkDecision::Session | crate::NetworkDecision::Persistent => {
                // The grant already flipped the shared network flag; the retry
                // and every later command now reach the network.
                self.execute_with_effect_outbox(session, intent).await
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
    trust: TrustLabel,
    attachments: serde_json::Value,
}

fn logged_tail(events: &[Event]) -> Vec<LoggedInput> {
    let mut tail: Vec<LoggedInput> = Vec::new();
    for event in events.iter().rev() {
        match event.kind {
            EventKind::UserMessage => tail.push(LoggedInput {
                id: event.id,
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
            // Anything else ends the run: only the trailing block is this
            // turn's, and an identical prompt from an earlier turn is a real
            // repeat the user typed.
            EventKind::Interrupt => continue,
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

    #[test]
    fn an_unthrottled_transient_falls_back_to_the_curve() {
        let stalled = crate::provider::ProviderError::Stream("stream stalled".into());
        assert!(stalled.is_retryable());
        assert_eq!(stalled.retry_after(), None);
    }
}
