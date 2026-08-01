//! The kernel loop. Deliberately boring; all intelligence lives in modules.
//! The model proposes blocks; the kernel disposes via validate→police→
//! verify→execute (Vol 1 §4.1, Vol 3 §4). Phase 0 wires the spine and the
//! multi-turn tool loop; Policy and Verifier are stubbed (the executor's tools
//! are sandbox-jailed) until their crates land.

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

/// Default cap on tool calls executed concurrently within one turn (§12).
/// Overridable via `[budget] max_parallel_tools` in `medha.lock` or
/// `MEDHA_MAX_PARALLEL_TOOLS`, or per session via [`Kernel::with_max_parallel_tools`].
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 16;

/// A short, human-readable preview of what an intent will do — shown at the
/// approval gate. (A rendered diff via tool dry-run is a later refinement.)
fn approval_detail(intent: &ToolIntent) -> String {
    let s = |k: &str| intent.args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match intent.tool.as_str() {
        "shell.exec" => format!("$ {}", s("command")),
        "fs.edit" => {
            let old: String = s("old_string").chars().take(120).collect();
            let new: String = s("new_string").chars().take(120).collect();
            format!("edit {}\n- {}\n+ {}", s("path"), old, new)
        }
        "fs.write" => format!("write {} ({} bytes)", s("path"), s("content").len()),
        _ => {
            let a: String = intent.args.to_string().chars().take(200).collect();
            format!("{} {}", intent.tool, a)
        }
    }
}

/// The auto-approve scope key for the human gate (K9): the tool plus its most
/// salient argument, so "always allow" is scoped to *this* action — approving
/// `rm -rf build/` doesn't then auto-approve every future `shell.exec`. Falls
/// back to the bare tool name for tools with no obvious identifying arg.
fn approval_key(intent: &ToolIntent) -> String {
    let arg = ["command", "path", "url"]
        .iter()
        .find_map(|k| intent.args.get(*k).and_then(|v| v.as_str()));
    match arg {
        Some(a) => format!("{}: {a}", intent.tool),
        None => intent.tool.clone(),
    }
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
            vec![legacy]
        }
    }
}

/// Reuse exact canonical messages retained by a compaction result.
///
/// Message values are deliberately not searched: two turns can have identical
/// legacy text while carrying different signed/opaque provider state. The
/// context engine supplies the exact input occurrence for retained messages;
/// generated or rewritten messages are bridged from their legacy form.
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
    /// A budget ceiling was reached (the continuation policy can resume).
    Budget(crate::budgets::BudgetStop),
    /// The surface cancelled the turn; in-flight work settled gracefully and
    /// the returned history is consistent (every intent has an observation).
    Interrupted,
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
    progressive_context: Option<Arc<dyn crate::context::ProgressiveContext>>,
    max_parallel_tools: usize,
    /// Resolved model pricing (P1-12); `None` = cost unknown, meter stays off.
    pricing: Option<crate::types::Pricing>,
    /// Serializes human-gate prompts: parallel tool dispatch must not pop
    /// several approval cards at once (P2 gate race).
    gate_serial: futures::lock::Mutex<()>,
    /// Orders state-changing turns across concurrent root/child sessions. The
    /// guard spans execution through durable observation logging, so another
    /// mutation cannot commit in the gap before replay learns about this one.
    mutation_serial: Arc<tokio::sync::Mutex<()>>,
    /// Post-cancel settle window for in-flight tools (tunable in tests).
    settle_grace: std::time::Duration,
}

/// Tool-result payloads larger than this spill to the artifact store and are
/// replaced in-context by a head + a `read_artifact` reference (§4.5).
const SPILL_THRESHOLD: usize = 16_000;

/// Absolute per-turn ingestion limits. These are deliberately independent of
/// provider/model limits: a broken or adversarial stream must not be able to
/// grow the kernel's in-memory transcript forever.
const MAX_TOOL_INTENTS_PER_TURN: usize = 64;
const MAX_PROVIDER_STREAM_BLOCKS: usize = 16_384;
const MAX_PROVIDER_STREAM_BYTES: usize = 8 * 1024 * 1024;

/// How many times a turn's model stream is retried on a transient provider
/// failure (429 / 5xx / network drop) before giving up (K3).
const MAX_TURN_RETRIES: u32 = 3;
/// Bound measure → compact → remeasure so a pathological compressor cannot
/// rewrite the same turn indefinitely.
const MAX_COMPACTION_PASSES: u32 = 3;

/// After a cancel, how long an in-flight tool gets to settle before its future
/// is dropped and an `[interrupted]` observation is synthesized. Dropping is
/// safe here: process trees die with the future (group reaper), and the
/// synthesized observation keeps the intent→observation invariant intact.
const TOOL_SETTLE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Capped exponential backoff between stream retries: 250ms, 500ms, 1s, …
fn retry_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(250u64.saturating_mul(1 << attempt.saturating_sub(1).min(4)))
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
            progressive_context: None,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            pricing: None,
            gate_serial: futures::lock::Mutex::new(()),
            mutation_serial: Arc::new(tokio::sync::Mutex::new(())),
            settle_grace: TOOL_SETTLE_GRACE,
        }
    }

    /// A kernel for a sub-session: same provider, log, artifacts, policy and
    /// verifier, but its own executor, context engine and gate.
    ///
    /// Everything that shapes how a run behaves — pricing, tool parallelism,
    /// progressive context, the settle window — is inherited. Rebuilding a
    /// child with `Kernel::new` silently dropped all of it: a child metered no
    /// cost at all, so a shared cost ceiling could never trip on its spend.
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
            progressive_context: self.progressive_context.clone(),
            max_parallel_tools: self.max_parallel_tools,
            pricing: self.pricing,
            gate_serial: futures::lock::Mutex::new(()),
            mutation_serial: Arc::clone(&self.mutation_serial),
            settle_grace: self.settle_grace,
        }
    }

    /// Set resolved model pricing so the governor meters real dollars (P1-12).
    pub fn with_pricing(mut self, pricing: Option<crate::types::Pricing>) -> Self {
        self.pricing = pricing;
        self
    }

    /// Replace the deterministic verifier for a derived execution context.
    ///
    /// Writer sub-agents use this to disable the parent's fixed-directory
    /// verifier: their checkout is verified authoritatively by the orchestrator
    /// after patch extraction, while running the parent's verifier here would
    /// check the wrong tree.
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
        // The surface gets the text back to put in its input box; the label
        // matters only to the loop that would have applied it.
        let leftover: Vec<String> = q.drain_steers().into_iter().map(|(text, _)| text).collect();
        if !leftover.is_empty() {
            sink.steers_returned(&leftover);
        }
    }

    /// Spill an oversized tool-result payload to the artifact store, returning a
    /// truncated head + a `read_artifact` pointer. The full payload is still in
    /// the event log (P3), so nothing is lost — only the *live context* shrinks.
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
                     lost. Continue reading it: call read_artifact with hash=\"{hash}\" \
                     (offset, length) to page through the remainder, or re-read a specific \
                     line range with fs.read offset+limit. Do NOT report to the user that the \
                     output was truncated or that you can't see it — page through it and \
                     finish the task.]",
                    content.len()
                )
            }
            Err(_) => content, // spill failed → keep full rather than lose data
        }
    }

    /// Apply the live-context spill policy to both request representations.
    ///
    /// Durable replay/checkpoints hydrate canonical `ModelMessage` values
    /// directly. Rewriting only the legacy compatibility view therefore left
    /// the provider-facing ordered request carrying the original unbounded
    /// payload. Keep tool-call identity and opaque provider state intact while
    /// replacing only the result body with its content-addressed reference.
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

    /// Override the per-turn concurrency cap (§12).
    pub fn with_max_parallel_tools(mut self, n: usize) -> Self {
        self.max_parallel_tools = n.clamp(1, MAX_TOOL_INTENTS_PER_TURN);
        self
    }

    /// Execute one already-admitted intent while preserving the kernel's
    /// cancellation invariant: a call either settles with its real result or
    /// gets one synthesized interrupted observation. Calls that have not
    /// started when cancellation arrives are never dispatched.
    async fn execute_admitted(
        &self,
        session: &Session,
        intent: ToolIntent,
        web_tainted: bool,
        cancel: tokio_util::sync::CancellationToken,
        wall_deadline: Option<tokio::time::Instant>,
        settle_deadline: Arc<std::sync::OnceLock<tokio::time::Instant>>,
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
            let fut = self.dispatch_one(session, &intent, web_tainted);
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

    /// Make one settled execution durable and feed its observation back into the
    /// active request. A mutation guard, when needed, is held by the caller
    /// across both [`Self::execute_admitted`] and this method. Keeping persistence
    /// here lets the scheduler release that guard before it runs unrelated
    /// read/wait tools, while still ensuring another mutation cannot commit in
    /// the side-effect → event-log gap.
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
        // Label web-tool output as untrusted content (P7): a fetched page must
        // not be treated like a local file read. A tool relaying content it did
        // not produce declares that content's label, and the weaker wins.
        let trust = match self.executor.category(&tool) {
            Some(ToolCategory::Web) => TrustLabel::Web,
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
        // A settled memory mutation becomes a MemoryWrite event — the durable
        // record the projection rebuilds from (I1). Commit it before the
        // conversation observation, so a later observation append failure
        // cannot leave an applied memory missing from replay. The earlier
        // ToolEffectPrepared record still marks the attempt if this append
        // itself fails after the projection-side effect.
        let applied =
            if matches!(obs.status, crate::types::ObsStatus::Ok) && tool.starts_with("memory.") {
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
        window_events.push(event.id);
        *window_taint = window_taint.min(trust);
        // Once untrusted web content lands, taint the following provider turn
        // so consequential actions derived from it get escalated (§4.6).
        if matches!(trust, TrustLabel::Web) {
            *web_tainted = true;
        }
        let ok = matches!(obs.status, crate::types::ObsStatus::Ok);
        sink.tool_result(&tool, ok, &obs.payload);
        let content = self
            .maybe_spill(serde_json::to_string(&obs.payload).unwrap_or_default())
            .await;
        let message = Message::tool_result(&id, content);
        ordered_messages.push(message.ordered());
        messages.push(message);
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

    /// Run a session to completion: stream the model, execute any tool calls,
    /// feed results back, and repeat until the model finishes with text only or
    /// `max_turns` is hit. Returns the full message transcript.
    ///
    /// Context is recompiled from `messages` each turn (fresh per turn, §4.3);
    /// step (c) replaces this with a compile-from-event-log + compaction pass.
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
        let specs = self.executor.specs();
        // Size the tool-def overhead once so token estimates match the real
        // request (tool defs are sent every turn) (P1-9).
        self.context.note_tools(&specs);
        let mut gov = crate::budgets::Governor::new(budget);
        // Record this turn's new user messages so the session is fully
        // reconstructable from the log (resume/replay). Every *trailing* user
        // message is new, not just the last: a surface can append a typed prompt
        // and then a background agent's report in one turn, and logging only the
        // tail dropped the prompt from the log — and with it from the rehydrated
        // context below, so the model never saw what was typed.
        // Memory taint window (D6): the evidence a memory write may cite — event
        // ids since the last real user message, and the lowest trust label seen
        // among them. Kernel-owned; the model can never assert these.
        let mut window_events: Vec<ulid::Ulid> = Vec::new();
        let mut window_taint = TrustLabel::User;
        // Skip what the log already ends with. A turn that failed after this
        // point leaves its trailing run logged; retrying with the same messages
        // would append the run a second time, and the projection only collapses
        // *adjacent* identical user turns, so `[prompt, report]` twice survives
        // as four messages.
        let prior_events = self.log.events(session.id).await;
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
            if already
                .get(logged_cursor)
                .is_some_and(|input| input.content == message.content && input.trust == trust)
            {
                logged_cursor += 1;
                continue;
            }
            let e = self
                .log
                .append(Event::user_input(session, &message.content, trust))
                .await?;
            window_events.push(e.id);
            // A report carries the weakest label its agent touched; injected
            // content must not enter as if the user had typed it.
            if let Some(trust) = message.trust {
                window_taint = window_taint.min(trust);
            }
        }
        let mut ordered_messages: Vec<ModelMessage> =
            messages.iter().map(Message::ordered).collect();
        let logged_events = self.log.events(session.id).await;
        let has_checkpoint = logged_events.iter().any(|event| {
            event.kind == EventKind::Compaction
                && crate::events::has_valid_compaction_snapshot(&event.payload)
        });
        if has_checkpoint {
            // A compaction event is a full request checkpoint. Replace both
            // views—including the freshly generated system sheath—with that
            // exact state plus the durable events appended after it. Keeping
            // the surface's system message as well would duplicate system
            // instructions and would not be byte-equivalent replay.
            messages = crate::events::project_request_messages(&logged_events);
            ordered_messages = crate::events::project_request_ordered_messages(&logged_events);
        } else if logged_events
            .iter()
            .any(|event| event.kind == EventKind::ModelMessage)
        {
            let projected = crate::events::project_ordered_messages(&logged_events);
            ordered_messages = hydrate_ordered_log(&messages, projected);
        }
        // Spill oversized tool results after hydration (K11). Checkpoint replay
        // can replace the surface transcript with full durable observations, so
        // spilling before that replacement would be silently undone. Apply the
        // rewrite independently to both views: ordered replay is authoritative
        // for provider requests and may carry opaque state which a legacy
        // round-trip would discard.
        self.spill_hydrated_tool_results(&mut messages, &mut ordered_messages)
            .await;
        // Trust-flow taint (§4.6): flips true once a web-labeled observation
        // enters this request, so a later consequential action derived from it
        // can be escalated. Scoped to the request (one run_session). Seeded from
        // injected content, so a sub-agent that read the web taints the parent
        // that acts on its report.
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
            let mut compaction_passes = 0u32;
            let (assistant, canonical, intents, usage, turn_interrupted) = 'model_call: loop {
                let (prepared, prepared_input_tokens, reserved_output_tokens) = loop {
                    let limits = self.provider.model_limits();
                    let input_limit = limits
                        .input_allowance(self.provider.requested_output_tokens())
                        .map(|tokens| tokens.min(u64::from(u32::MAX)) as u32);
                    let candidate = CompiledContext {
                        model: String::new(),
                        messages: messages.clone(),
                        ordered: Some(ordered_messages.clone()),
                        tools: specs.clone(),
                    };
                    let prepared = self
                        .provider
                        .prepare_request(&candidate)
                        .map_err(|error| KernelError::Provider(error.to_string()))?;

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
                    if compiled.overflow {
                        if let Some(q) = interrupts.as_mut() {
                            Self::return_unapplied_steers(q, sink);
                        }
                        return Ok((
                            messages,
                            StopReason::Budget(crate::budgets::BudgetStop::ContextOverflow),
                        ));
                    }
                    if !compiled.compacted {
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
                    // The durable log keeps both originals and this canonical
                    // checkpoint; the active view is now the only candidate
                    // that may be prepared and sent.
                    ordered_messages = compacted_ordered;
                    messages = compiled.messages;
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
                    Ok(t) => break t,
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
            // Feed real token usage back: the context engine uses it for accurate
            // compaction decisions, and the governor meters spend — real dollars
            // when pricing resolved, 0.0 (meter off) otherwise (P1-12).
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
                self.log
                    .append(Event::model_message(session, &canonical))
                    .await?;
                ordered_messages.push(canonical);
                messages.push(Message::assistant_calls(assistant.content, Vec::new()));
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

            if intents.is_empty() {
                // A steer that raced this final turn goes back to the surface.
                if let Some(q) = interrupts.as_mut() {
                    Self::return_unapplied_steers(q, sink);
                }
                return Ok((messages, StopReason::Finished)); // text-only finish
            }

            // Memory intents get their trust fields HERE, kernel-side (D6): any
            // model-supplied trust/confidence/provenance is stripped and replaced
            // with the taint-window values. The window covers events up to this
            // turn's dispatch — same-turn sibling observations aren't evidence
            // the model has seen yet.
            let mut intents = intents;
            for it in &mut intents {
                if it.tool.starts_with("memory.") {
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
                sink.tool_call(&it.tool, &it.args);
            }
            // Any locally-mutating tool triggers post-edit verification — derived
            // from the declared blast radius, not a hardcoded name list, so edits
            // via multi_edit / shell.exec (e.g. `sed -i`) are covered too (§4.7).
            let modified_files = intents.iter().any(|i| {
                matches!(
                    self.executor.blast_radius(&i.tool),
                    Some(BlastRadius::ReversibleLocal | BlastRadius::IrreversibleLocal)
                )
            });

            // Execute side-effect-free runs concurrently, but put a hard
            // barrier around every state mutation. The event log is the replay
            // authority: if two writes commit B→A while observations are logged
            // A→B, live state, resume, and rewind disagree. A mutation key is
            // still collected by the executor (not inferred from tool names),
            // including low-risk memory writes whose policy radius is `Read`.
            //
            // This first correctness-first scheduler serializes all mutations;
            // later it may parallelize distinct keys only after proving they
            // commute. The lock is scoped to ONE mutation's execution and durable
            // observation. Holding it across a later `agent.wait` would deadlock:
            // the parent would await a child whose own write needed this lock.
            // Read batches retain bounded parallelism between mutation barriers.
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
                        let mut batch = stream::iter(std::mem::take(&mut read_batch))
                            .map(|intent| {
                                self.execute_admitted(
                                    session,
                                    intent,
                                    dispatch_web_tainted,
                                    dispatch_cancel.clone(),
                                    dispatch_wall_deadline,
                                    Arc::clone(&dispatch_settle_deadline),
                                )
                            })
                            .buffered(self.max_parallel_tools);
                        while let Some((id, tool, obs, discovered)) = batch.next().await {
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
                    // A second process has a different in-memory mutex. The log
                    // backend supplies the durable writer lane, kept alive over
                    // both the external side effect and the events that make it
                    // replayable. Lease failure is a settled tool error rather
                    // than an early return: every admitted intent still gets its
                    // observation.
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
                let mut batch = stream::iter(read_batch)
                    .map(|intent| {
                        self.execute_admitted(
                            session,
                            intent,
                            dispatch_web_tainted,
                            dispatch_cancel.clone(),
                            dispatch_wall_deadline,
                            Arc::clone(&dispatch_settle_deadline),
                        )
                    })
                    .buffered(self.max_parallel_tools);
                while let Some((id, tool, obs, discovered)) = batch.next().await {
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

            // Deterministic verification after edits (§4.7): run the configured
            // check and feed the result back so a broken build self-corrects.
            if modified_files {
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
                if let Some(rep) = verification {
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
        }
    }

    /// One turn: stream the model (retrying transient failures) and collect a
    /// legacy control view plus the exact canonical assistant message.
    ///
    /// Retry policy (K3): a transient provider failure — network drop, 429, 5xx,
    /// mid-stream cutoff — is retried with capped exponential backoff, but ONLY
    /// while nothing has been streamed to the surface yet (re-running after
    /// partial output would duplicate it). A context-length rejection is surfaced
    /// as [`KernelError::ContextOverflow`] so `run_session` can compact and retry
    /// (P0-6); other errors are fatal for the turn.
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
                            return Err(KernelError::Provider(
                                "the provider rejected the requested output-token cap; lower the profile's max_output_tokens (context compaction cannot fix an output-cap error)"
                                    .into(),
                            ));
                        }
                        crate::provider::ProviderFailure::PayloadTooLarge => {
                            return Err(KernelError::Provider(
                                "the provider rejected the HTTP payload size; reduce retained media or byte-heavy tool results"
                                    .into(),
                            ));
                        }
                        crate::provider::ProviderFailure::Transient
                        | crate::provider::ProviderFailure::Fatal => {}
                    }
                    if e.is_retryable() && !emitted && attempt < MAX_TURN_RETRIES {
                        attempt += 1;
                        // The backoff nap races the cancel token too — Esc
                        // during a retry wait must stop the turn, not queue
                        // another attempt.
                        tokio::select! {
                            _ = tokio::time::sleep(retry_backoff(attempt)) => continue,
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
        // Keep the existing transparent reasoning audit event. The canonical
        // message is appended by `run_session` only after it decides whether
        // streamed tool calls reached dispatch admission.
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
        // Establishing the stream must ALSO race the cancel token: on large
        // models the server can spend minutes in prompt processing before the
        // first byte arrives, and an Esc during that window previously did
        // nothing (the select below only covered an already-open stream).
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
                    sink.usage(u.prompt_tokens, u.total_tokens);
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

    /// validate (P1) → police (§4.6) → gate (P5) → execute (§4.8).
    /// The policy authorizes deny-first; `Human` routes through the approval
    /// gate with a real preview. A pre-execution verifier chain (§4.7) will
    /// slot in here when it exists.
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

    async fn dispatch_one(
        &self,
        session: &Session,
        intent: &ToolIntent,
        web_tainted: bool,
    ) -> Observation {
        let radius = self.executor.blast_radius(&intent.tool);
        let raw = self.policy.authorize(session.autonomy, intent, radius);
        // Did trust-flow turn a permissive verdict into a gate? Such an escalated
        // gate must never be auto-approved (K9) — capture it before `raw` moves.
        let raw_permissive = matches!(raw, crate::types::Decision::Allow);
        let decision =
            escalate_for_trust_flow(raw, radius, web_tainted, self.executor.containment());
        let escalated = raw_permissive && matches!(decision, crate::types::Decision::Human);
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
        match decision {
            crate::types::Decision::Deny { reason } => Observation::denial(&intent.id, reason),
            crate::types::Decision::Human => {
                // One approval card at a time (P2 gate race): the lock spans
                // preview → answer so parallel dispatch can't pop several
                // prompts at once, but is dropped BEFORE execution so an
                // approved slow tool doesn't block the next card.
                let approved = {
                    let _one_gate = self.gate_serial.lock().await;
                    // Draft → approve → commit (P5): show a real preview (rendered
                    // diff via dry-run when available) and ask before executing.
                    let detail = self
                        .executor
                        .preview(intent)
                        .await
                        .unwrap_or_else(|| approval_detail(intent));
                    // Scope the approval to this specific action (tool + salient arg),
                    // so "always allow" doesn't blanket every call of the tool (K9).
                    let action = approval_key(intent);
                    self.gate
                        .confirm(&action, Some(&detail), escalated)
                        .await
                        .approved()
                };
                if approved {
                    self.execute_with_effect_outbox(session, intent).await
                } else {
                    Observation::denial(&intent.id, "rejected by human".to_string())
                }
            }
            crate::types::Decision::Allow => self.execute_with_effect_outbox(session, intent).await,
        }
    }
}

/// Kernel-computed trust for memory writes (D6): strip every trust-adjacent key
/// the model may have passed, then inject the taint-window values under `_`-
/// prefixed keys the memory tools read. Pure so it's unit-testable.
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
    // User-stated only when nothing below user trust entered the window.
    obj.insert(
        "_user_stated".into(),
        serde_json::json!(taint == TrustLabel::User),
    );
}

/// Trust-flow escalation (§4.6): gate a consequential, web-tainted action unless
/// the sandbox's containment blocks network exfiltration. Only ever *tightens*
/// an `Allow` to `Human` — it never relaxes a denial or an existing gate. Pure
/// and total so the policy is unit-testable in isolation.
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

/// Index of the first user message this turn has not logged yet.
///
/// Callers append what is new and then call `run_session`, so the trailing run
/// of user messages is this turn's; everything before it was logged by an
/// earlier call. It is a *run*, not one message: a surface can append a typed
/// prompt and a background agent's report together.
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
                relayed_trust: None,
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
        // Two different shell commands must produce DIFFERENT keys, so approving
        // one with "always" doesn't blanket-approve the other (K9).
        let a = approval_key(&intent("shell.exec", json!({ "command": "cargo build" })));
        let b = approval_key(&intent("shell.exec", json!({ "command": "rm -rf build" })));
        assert_eq!(a, "shell.exec: cargo build");
        assert_ne!(a, b, "distinct commands must not share an auto-approve key");
        // Path-based tools key on the path; arg-less tools fall back to the tool.
        assert_eq!(
            approval_key(&intent("fs.write", json!({ "path": "x.rs" }))),
            "fs.write: x.rs"
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
