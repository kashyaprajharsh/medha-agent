//! The kernel loop. Deliberately boring; all intelligence lives in modules.
//! The model proposes blocks; the kernel disposes via validate→police→
//! verify→execute (Vol 1 §4.1, Vol 3 §4). Phase 0 wires the spine and the
//! multi-turn tool loop; Policy and Verifier are stubbed (the executor's tools
//! are sandbox-jailed) until their crates land.

use crate::context::ContextEngine;
use crate::errors::KernelError;
use crate::events::{Event, EventLog};
use crate::executor::Executor;
use crate::provider::Provider;
use crate::sink::StreamSink;
use crate::types::{
    BlastRadius, Block, CompiledContext, Message, Observation, Session, ToolCategory, ToolIntent,
    TrustLabel,
};
use futures::stream::{self, StreamExt};
use std::sync::Arc;

/// Default cap on tool calls executed concurrently within one turn (§12,
/// `max_parallel_tools`). A sensible default, not a fixed limit — override per
/// session via [`Kernel::with_max_parallel_tools`] (will come from `medha.lock`).
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 8;

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

/// Why a session loop stopped — so the surface can tell the user (e.g. which
/// budget ceiling was hit, and that it can be resumed) instead of returning
/// silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished with a text-only turn (task complete).
    Finished,
    /// A budget ceiling was reached (the continuation policy can resume).
    Budget(crate::budgets::BudgetStop),
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
    max_parallel_tools: usize,
}

/// Tool-result payloads larger than this spill to the artifact store and are
/// replaced in-context by a head + a `read_artifact` reference (§4.5).
const SPILL_THRESHOLD: usize = 16_000;

/// How many times a turn's model stream is retried on a transient provider
/// failure (429 / 5xx / network drop) before giving up (K3).
const MAX_TURN_RETRIES: u32 = 3;

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
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
        }
    }

    /// Spill an oversized tool-result payload to the artifact store, returning a
    /// truncated head + a `read_artifact` pointer. The full payload is still in
    /// the event log (P3), so nothing is lost — only the *live context* shrinks.
    fn maybe_spill(&self, content: String) -> String {
        if content.len() <= SPILL_THRESHOLD {
            return content;
        }
        match self.artifacts.put(content.as_bytes()) {
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

    /// Override the per-turn concurrency cap (§12).
    pub fn with_max_parallel_tools(mut self, n: usize) -> Self {
        self.max_parallel_tools = n.max(1);
        self
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
    ) -> Result<(Vec<Message>, StopReason), KernelError> {
        let specs = self.executor.specs();
        let max_ctx = self.provider.capabilities().max_ctx;
        let mut gov = crate::budgets::Governor::new(budget);
        // Spill oversized tool results already in the working set (K11). The live
        // path spills at execute time (below), but messages rebuilt from the log
        // on resume carry the FULL payloads — a resumed context could be
        // megabytes larger than the live one ever was. Re-applying the spill here
        // is idempotent (already-spilled results are under the threshold) and
        // covers every resume entry point (headless / REPL / TUI).
        for m in messages.iter_mut() {
            if m.role == crate::types::Role::Tool && m.content.len() > SPILL_THRESHOLD {
                m.content = self.maybe_spill(std::mem::take(&mut m.content));
            }
        }
        // Record this turn's new user message so the session is fully
        // reconstructable from the log (resume/replay). Callers append the user
        // message then call run_session, so the tail is the new prompt; earlier
        // user turns were logged on their own prior calls (no duplication).
        if let Some(last) = messages.last() {
            if last.role == crate::types::Role::User {
                self.log.append(Event::user_message(session, &last.content)).await?;
            }
        }
        // Trust-flow taint (§4.6): flips true once a web-labeled observation
        // enters this request, so a later consequential action derived from it
        // can be escalated. Scoped to the request (one run_session).
        let mut web_tainted = false;
        loop {
            // Budget gate: stop gracefully before a turn if any ceiling is hit (I4).
            if let Some(stop) = gov.check() {
                return Ok((messages, StopReason::Budget(stop)));
            }
            gov.record_turn();

            // Exact pre-flight token count when the host offers a tokenization
            // route (else the engine uses its local estimate / last usage).
            // Only worth it when we know the window; best-effort and never
            // blocks the turn. Feeds the engine's decision basis via the same
            // authoritative-count channel the post-turn `usage` uses.
            if max_ctx.is_some() {
                if let Some(exact) = self.provider.count_tokens(&messages).await {
                    self.context.update_usage(exact, exact);
                }
            }

            // Compile a budget-fitted view of the working history (§4.3).
            let compiled = self.context.compile(&messages, max_ctx).await;
            // Hard safety ceiling (independent of the soft trigger): if even the
            // engine's best effort still overflows, refuse to send this turn
            // rather than risk a provider context-length error (I4).
            if compiled.overflow {
                return Ok((messages, StopReason::Budget(crate::budgets::BudgetStop::ContextOverflow)));
            }
            let view = compiled.messages;
            if compiled.compacted {
                sink.compaction(compiled.before_tokens, compiled.after_tokens, compiled.summarized);
                self.log
                    .append(Event::compaction(
                        session,
                        compiled.before_tokens,
                        compiled.after_tokens,
                        compiled.summary.as_deref(),
                    ))
                    .await?;
                // Carry-forward: the compacted view becomes the working set, so
                // history stays bounded and we don't recompact it every turn.
                // The full originals remain in the durable hash-chained log (P3),
                // so this is lossless — better than discarding them.
                messages = view.clone();
            }
            let mut ctx = CompiledContext { model: String::new(), messages: view, tools: specs.clone() };

            // Run the turn; if the provider rejects it as too long despite our
            // pre-flight budgeting (P0-6 — the local estimate undercounted), do
            // one emergency compaction with a halved window (forces the engine's
            // hard path) and retry once, rather than dying with a fatal error.
            let mut overflow_retried = false;
            let (assistant, intents, usage) = loop {
                match self.run_turn(session, &ctx, sink).await {
                    Ok(t) => break t,
                    Err(KernelError::ContextOverflow) if !overflow_retried => {
                        overflow_retried = true;
                        let emergency = max_ctx.map(|m| (m / 2).max(1));
                        let recompiled = self.context.compile(&messages, emergency).await;
                        sink.compaction(recompiled.before_tokens, recompiled.after_tokens, recompiled.summarized);
                        self.log
                            .append(Event::compaction(
                                session,
                                recompiled.before_tokens,
                                recompiled.after_tokens,
                                recompiled.summary.as_deref(),
                            ))
                            .await?;
                        messages = recompiled.messages.clone();
                        ctx = CompiledContext {
                            model: String::new(),
                            messages: recompiled.messages,
                            tools: specs.clone(),
                        };
                    }
                    Err(KernelError::ContextOverflow) => {
                        // Already retried once and still over — stop gracefully.
                        return Ok((messages, StopReason::Budget(crate::budgets::BudgetStop::ContextOverflow)));
                    }
                    Err(e) => return Err(e),
                }
            };
            // Feed real token usage back: the context engine uses it for accurate
            // compaction decisions, and the governor meters spend (cost = 0.0
            // until a per-model price table is wired).
            if let Some(u) = usage {
                self.context.update_usage(u.prompt_tokens, u.total_tokens);
                gov.record_tokens(u.total_tokens as u64, 0.0);
            }
            messages.push(assistant);

            if intents.is_empty() {
                return Ok((messages, StopReason::Finished)); // text-only finish
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

            // Execute the turn's calls concurrently, order-preserved and bounded
            // (§12). Models routinely emit several independent calls at once
            // (e.g. parallel reads); running them sequentially wastes wall-clock.
            // Full dependency-aware DAG with write-path serialization is the next
            // refinement (parallel.rs); for now tools are sandbox-jailed and
            // conflicting same-turn writes are rare.
            let results: Vec<(String, String, Observation)> = stream::iter(intents)
                .map(|intent| async move {
                    let obs = self.dispatch_one(session, &intent, web_tainted).await;
                    (intent.id, intent.tool, obs)
                })
                .buffered(self.max_parallel_tools)
                .collect()
                .await;

            // Append in deterministic (request) order; surface each result to the
            // sink (diffs, errors) and feed it back to the model.
            for (id, tool, obs) in results {
                // Label web-tool output as untrusted content (P7): a fetched
                // page must not be treated like a local file read.
                let trust = match self.executor.category(&tool) {
                    Some(ToolCategory::Web) => TrustLabel::Web,
                    _ => TrustLabel::Tool,
                };
                self.log.append(Event::tool_obs(session, &obs, trust)).await?;
                // Once untrusted web content lands, taint the rest of the
                // request so later consequential actions get escalated (§4.6).
                if matches!(trust, TrustLabel::Web) {
                    web_tainted = true;
                }
                let ok = matches!(obs.status, crate::types::ObsStatus::Ok);
                sink.tool_result(&tool, ok, &obs.payload);
                let content = serde_json::to_string(&obs.payload).unwrap_or_default();
                messages.push(Message::tool_result(&id, self.maybe_spill(content)));
            }

            // Deterministic verification after edits (§4.7): run the configured
            // check and feed the result back so a broken build self-corrects.
            if modified_files {
                if let Some(rep) = self.verifier.check().await {
                    sink.verify(rep.ok, &rep.summary);
                    let mut tail: Vec<&str> = rep.output.lines().rev().take(40).collect();
                    tail.reverse();
                    let feedback = format!(
                        "[verifier] {} — {}\n{}",
                        if rep.ok { "PASS" } else { "FAIL" },
                        rep.summary,
                        tail.join("\n")
                    );
                    // Log it as a user message so a resumed session sees the same
                    // verifier feedback the model reasoned about (K12) — otherwise
                    // replay diverges (the model self-corrected against text that
                    // no longer exists in the reconstructed history).
                    self.log.append(Event::user_message(session, &feedback)).await.ok();
                    messages.push(Message::user(feedback));
                }
            }
        }
    }

    /// One turn: stream the model (retrying transient failures), log every
    /// block, and collect text + intents into a single assistant message.
    ///
    /// Retry policy (K3): a transient provider failure — network drop, 429, 5xx,
    /// mid-stream cutoff — is retried with capped exponential backoff, but ONLY
    /// while nothing has been streamed to the surface yet (re-running after
    /// partial output would duplicate it). A context-length rejection is surfaced
    /// as [`KernelError::ContextOverflow`] so `run_session` can compact and retry
    /// (P0-6); other errors are fatal for the turn.
    async fn run_turn(
        &self,
        session: &Session,
        ctx: &CompiledContext,
        sink: &dyn StreamSink,
    ) -> Result<(Message, Vec<ToolIntent>, Option<crate::types::Usage>), KernelError> {
        let mut attempt = 0u32;
        let (text, reasoning, intents, usage) = loop {
            match self.stream_turn(ctx, sink).await {
                Ok(data) => break data,
                Err((e, emitted)) => {
                    if e.is_context_overflow() {
                        return Err(KernelError::ContextOverflow);
                    }
                    if e.is_retryable() && !emitted && attempt < MAX_TURN_RETRIES {
                        attempt += 1;
                        futures_timer::Delay::new(retry_backoff(attempt)).await;
                        continue;
                    }
                    return Err(KernelError::Provider(e.to_string()));
                }
            }
        };
        // Log (P3/P7) after a successful stream. Reasoning is logged for
        // transparency but excluded from the Message that re-enters history.
        if !reasoning.is_empty() {
            self.log.append(Event::model_reasoning(session, &reasoning)).await?;
        }
        if !text.is_empty() {
            self.log.append(Event::model_text(session, &text)).await?;
        }
        for it in &intents {
            self.log.append(Event::model_intent(session, it)).await?;
        }
        Ok((Message::assistant_calls(text, intents.clone()), intents, usage))
    }

    /// Establish and consume one model stream, emitting deltas to the sink as
    /// they arrive. Returns the accumulated (text, reasoning, intents, usage) on
    /// success, or `(error, already_emitted)` — the flag tells the caller whether
    /// retrying is safe (retrying after content was streamed would duplicate it).
    #[allow(clippy::type_complexity)]
    async fn stream_turn(
        &self,
        ctx: &CompiledContext,
        sink: &dyn StreamSink,
    ) -> Result<
        (String, String, Vec<ToolIntent>, Option<crate::types::Usage>),
        (crate::provider::ProviderError, bool),
    > {
        let mut stream = self.provider.stream(ctx).await.map_err(|e| (e, false))?;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut intents: Vec<ToolIntent> = Vec::new();
        let mut usage: Option<crate::types::Usage> = None;
        let mut emitted = false;
        while let Some(block) = stream.next().await {
            match block {
                Ok(Block::Text(t)) => {
                    emitted = true;
                    sink.text(&t);
                    text.push_str(&t);
                }
                Ok(Block::Reasoning(r)) => {
                    emitted = true;
                    sink.reasoning(&r);
                    reasoning.push_str(&r);
                }
                Ok(Block::ToolStarted { name, target }) => {
                    emitted = true;
                    sink.tool_started(&name, target.as_deref());
                }
                Ok(Block::ToolIntent(it)) => {
                    emitted = true;
                    intents.push(it);
                }
                Ok(Block::Usage(u)) => {
                    sink.usage(u.prompt_tokens, u.total_tokens);
                    usage = Some(u);
                }
                Err(e) => return Err((e, emitted)),
            }
        }
        Ok((text, reasoning, intents, usage))
    }

    /// validate (P1) → police (§4.6) → verify (§4.7) → execute (§4.8).
    /// The policy authorizes deny-first; the verifier layer (§4.7) will gate
    /// `Verify`/`Human` once it exists — until then they fall through to
    /// execution (Verify) or a denial (Human, no gate yet).
    async fn dispatch_one(
        &self,
        session: &Session,
        intent: &ToolIntent,
        web_tainted: bool,
    ) -> Observation {
        let radius = self.executor.blast_radius(&intent.tool);
        let raw = self.policy.authorize(intent, radius);
        // Did trust-flow turn a permissive verdict into a gate? Such an escalated
        // gate must never be auto-approved (K9) — capture it before `raw` moves.
        let raw_permissive = matches!(raw, crate::types::Decision::Allow | crate::types::Decision::Verify);
        let decision =
            escalate_for_trust_flow(raw, radius, web_tainted, self.executor.containment());
        let escalated = raw_permissive && matches!(decision, crate::types::Decision::Human);
        self.log.append(Event::policy(session, intent, &decision)).await.ok();
        match decision {
            crate::types::Decision::Deny { reason } => Observation::denial(&intent.id, reason),
            crate::types::Decision::Human => {
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
                if self.gate.confirm(&action, Some(&detail), escalated).await.approved() {
                    self.executor.execute(intent).await
                } else {
                    Observation::denial(&intent.id, "rejected by human".to_string())
                }
            }
            crate::types::Decision::Allow | crate::types::Decision::Verify => {
                self.executor.execute(intent).await
            }
        }
    }
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
    let consequential =
        matches!(radius, Some(BlastRadius::IrreversibleLocal | BlastRadius::External));
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

#[cfg(test)]
mod approval_key_tests {
    use super::approval_key;
    use crate::types::ToolIntent;
    use serde_json::json;

    fn intent(tool: &str, args: serde_json::Value) -> ToolIntent {
        ToolIntent { id: "1".into(), tool: tool.into(), args }
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
        assert_eq!(approval_key(&intent("fs.write", json!({ "path": "x.rs" }))), "fs.write: x.rs");
        assert_eq!(approval_key(&intent("update_plan", json!({}))), "update_plan");
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
            escalate_for_trust_flow(Decision::Allow, Some(BlastRadius::Read), true, Containment::None),
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
            Decision::Deny { reason: "blocked".into() },
            Some(BlastRadius::External),
            true,
            Containment::None,
        );
        assert!(matches!(d, Decision::Deny { .. }));
    }
}
