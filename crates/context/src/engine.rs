//! `PipelineEngine`, the default `ContextEngine`, preserves assistant/tool-result
//! groups across compaction boundaries.

use crate::budget::ContextBudget;
use crate::compactor::{ExtractiveSummarizer, HistoryItem, ItemKind, Summarizer};
use crate::policy::{CompactionAction, CompactionPolicy};
use crate::tokens::{BpeCounter, TokenCounter};
use async_trait::async_trait;
use kernel::{CompileResult, ContextEngine, InputTokenCount, Message, Role, TokenCountQuality};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

type SystemRefresh = dyn Fn(&str) -> String + Send + Sync;

pub struct PipelineEngine {
    policy: CompactionPolicy,
    /// Token counter for the pre-flight estimate and per-item boundaries. A real
    /// BPE tokenizer in production; injectable (a model-exact tokenizer, or the
    /// heuristic for tests) via [`PipelineEngine::with_counter`].
    counter: Arc<dyn TokenCounter>,
    /// Real prompt tokens from the provider's last response (0 = unknown). The
    /// authoritative basis for the compaction decision — not an estimate.
    last_prompt_tokens: AtomicU32,
    /// Count for the exact request candidate currently being compiled. Cleared
    /// before every prepare/count cycle so it can never leak across a changed
    /// request body.
    preflight_tokens: AtomicU32,
    preflight_quality: AtomicU8,
    preflight_fingerprint: std::sync::Mutex<Option<String>>,
    /// Provider overflow asks for one bounded compaction pass. This is a
    /// one-shot latch, not a guessed replacement context window.
    force_next: AtomicBool,
    /// A completed compaction is judged against the next real provider usage.
    pending_usage_verification: AtomicBool,
    verification_threshold: AtomicU32,
    /// Consecutive compactions that barely helped; backs off to avoid the
    /// "compact every turn" thrash.
    ineffective: AtomicU32,
    /// Context size when anti-thrash latched; sufficient growth releases it.
    latched_at: AtomicU32,
    summarizer: Arc<dyn Summarizer>,
    last_summary: std::sync::Mutex<Option<String>>,
    artifacts: Option<Arc<dyn kernel::ArtifactStore>>,
    tool_overhead: AtomicU32,
    /// Frozen startup sheaths refresh only at full compaction.
    full_compaction_refresh: Option<Arc<SystemRefresh>>,
}

impl PipelineEngine {
    /// Production default: a real BPE tokenizer (`o200k_base`) for estimation.
    pub fn new(policy: CompactionPolicy) -> Self {
        Self::with_counter(policy, Arc::new(BpeCounter::o200k()))
    }

    /// Construct with a specific token counter — an exact per-model tokenizer,
    /// or the heuristic for deterministic tests.
    pub fn with_counter(policy: CompactionPolicy, counter: Arc<dyn TokenCounter>) -> Self {
        Self {
            policy,
            counter,
            last_prompt_tokens: AtomicU32::new(0),
            preflight_tokens: AtomicU32::new(0),
            preflight_quality: AtomicU8::new(quality_code(TokenCountQuality::LocalEstimate)),
            preflight_fingerprint: std::sync::Mutex::new(None),
            force_next: AtomicBool::new(false),
            pending_usage_verification: AtomicBool::new(false),
            verification_threshold: AtomicU32::new(0),
            ineffective: AtomicU32::new(0),
            latched_at: AtomicU32::new(0),
            summarizer: Arc::new(ExtractiveSummarizer),
            last_summary: std::sync::Mutex::new(None),
            artifacts: None,
            tool_overhead: AtomicU32::new(0),
            full_compaction_refresh: None,
        }
    }

    /// Inject the summarizer used for Full compaction (e.g. an LLM summarizer).
    pub fn with_summarizer(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = summarizer;
        self
    }

    /// Inject the artifact store used for lossless pruning.
    pub fn with_artifacts(mut self, artifacts: Arc<dyn kernel::ArtifactStore>) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    /// Refresh frozen system-prompt sections only after a Full compaction.
    pub fn with_full_compaction_refresh(mut self, refresh: Arc<SystemRefresh>) -> Self {
        self.full_compaction_refresh = Some(refresh);
        self
    }

    /// The same configuration over a fresh conversation.
    fn forked(&self) -> Self {
        let mut engine = Self::with_counter(self.policy.clone(), Arc::clone(&self.counter));
        engine.summarizer = Arc::clone(&self.summarizer);
        engine.artifacts = self.artifacts.clone();
        engine.full_compaction_refresh = self.full_compaction_refresh.clone();
        engine
    }
}

impl Default for PipelineEngine {
    fn default() -> Self {
        Self::new(CompactionPolicy::default())
    }
}

/// Count the full tool-call envelope, including arguments and ids.
fn count_msg(m: &Message, counter: &dyn TokenCounter) -> u32 {
    let mut t = counter.count(&m.content);
    for tc in &m.tool_calls {
        t += counter.count(&tc.tool) + counter.count(&tc.args.to_string()) + 4;
    }
    if let Some(id) = &m.tool_call_id {
        t += counter.count(id);
    }
    t
}

/// Estimate tokens for the message-selection budget walks (head/tail).
fn count_all(messages: &[Message], counter: &dyn TokenCounter) -> u32 {
    messages.iter().map(|m| count_msg(m, counter)).sum()
}

fn passthrough(messages: &[Message], tokens: u32, overflow: bool) -> CompileResult {
    CompileResult {
        messages: messages.to_vec(),
        source_indices: (0..messages.len()).map(Some).collect(),
        compacted: false,
        summarized: false,
        before_tokens: tokens,
        after_tokens: tokens,
        overflow,
        summary: None,
    }
}

/// Hard safety ceiling check against the *true* model window (not the reduced
/// usable budget) — the second, independent layer above the normal trigger.
fn is_overflow(tokens: f32, true_max_ctx: u32, policy: &CompactionPolicy) -> bool {
    tokens >= true_max_ctx as f32 * policy.emergency_ratio
}

const fn quality_code(quality: TokenCountQuality) -> u8 {
    match quality {
        TokenCountQuality::Authoritative => 0,
        TokenCountQuality::ProviderEstimate => 1,
        TokenCountQuality::LocalEstimate => 2,
    }
}

fn quality_from_code(code: u8) -> TokenCountQuality {
    match code {
        0 => TokenCountQuality::Authoritative,
        1 => TokenCountQuality::ProviderEstimate,
        _ => TokenCountQuality::LocalEstimate,
    }
}

/// Extra tokens per tool for chat-template scaffolding (headers/instructions the
/// serialized schema doesn't capture). Biased safe-high — undercount is the risk.
const PER_TOOL_SCAFFOLD_TOKENS: u32 = 18;

#[async_trait]
impl ContextEngine for PipelineEngine {
    fn fork(&self) -> Option<Arc<dyn ContextEngine>> {
        Some(Arc::new(self.forked()))
    }

    fn update_usage(&self, prompt_tokens: u32, _total_tokens: u32) {
        // Real usage already counts tool defs — store verbatim.
        self.last_prompt_tokens
            .store(prompt_tokens, Ordering::Relaxed);
        if self
            .pending_usage_verification
            .swap(false, Ordering::AcqRel)
        {
            let threshold = self.verification_threshold.load(Ordering::Acquire);
            if threshold > 0 && prompt_tokens >= threshold {
                self.ineffective.fetch_add(1, Ordering::Relaxed);
                self.latched_at.store(prompt_tokens, Ordering::Relaxed);
            } else {
                self.ineffective.store(0, Ordering::Relaxed);
            }
        }
    }

    fn clear_preflight(&self) {
        self.preflight_tokens.store(0, Ordering::Release);
        self.preflight_quality.store(
            quality_code(TokenCountQuality::LocalEstimate),
            Ordering::Release,
        );
        if let Ok(mut fingerprint) = self.preflight_fingerprint.lock() {
            *fingerprint = None;
        }
    }

    fn update_preflight(&self, count: &InputTokenCount) {
        self.preflight_tokens.store(
            count.tokens.min(u64::from(u32::MAX)) as u32,
            Ordering::Release,
        );
        self.preflight_quality
            .store(quality_code(count.quality), Ordering::Release);
        if let Ok(mut fingerprint) = self.preflight_fingerprint.lock() {
            *fingerprint = Some(count.request_fingerprint.clone());
        }
    }

    fn force_next_compaction(&self) {
        self.force_next.store(true, Ordering::Release);
    }

    fn note_tools(&self, tools: &[kernel::ToolSpec]) {
        let n: u32 = tools
            .iter()
            .map(|t| {
                self.counter.count(&t.name)
                    + self.counter.count(&t.description)
                    + self.counter.count(&t.schema.to_string())
                    + PER_TOOL_SCAFFOLD_TOKENS
            })
            .sum();
        self.tool_overhead.store(n, Ordering::Relaxed);
    }

    async fn compile(&self, messages: &[Message], max_input_tokens: Option<u32>) -> CompileResult {
        self.compile_inner(
            messages,
            max_input_tokens,
            &kernel::CompileControl::unlimited(),
        )
        .await
        .expect("an unlimited compile control cannot be interrupted")
    }

    async fn compile_controlled(
        &self,
        messages: &[Message],
        max_input_tokens: Option<u32>,
        control: &kernel::CompileControl,
    ) -> Result<CompileResult, kernel::ContextCompileError> {
        self.compile_inner(messages, max_input_tokens, control)
            .await
    }
}

impl PipelineEngine {
    async fn compile_inner(
        &self,
        messages: &[Message],
        max_input_tokens: Option<u32>,
        control: &kernel::CompileControl,
    ) -> Result<CompileResult, kernel::ContextCompileError> {
        control.check()?;
        let counter: &dyn TokenCounter = self.counter.as_ref();
        // Include the fixed tool-def overhead so the estimate matches the real
        // request size (tool defs are sent every turn but not in `count_all`).
        let overhead = self.tool_overhead.load(Ordering::Relaxed);
        let before = count_all(messages, counter) + overhead;

        let forced = self.force_next.swap(false, Ordering::AcqRel);
        // An unknown model limit remains unknown. After a real provider overflow
        // only, a synthetic one-pass target may shrink the rejected request; it
        // is never cached or presented as the model's context length.
        let synthetic_limit = max_input_tokens.is_none() && forced;
        let mc = match max_input_tokens {
            Some(limit) => limit,
            None if forced => before.saturating_mul(3).checked_div(4).unwrap_or(1).max(1),
            None => return Ok(passthrough(messages, before, false)),
        };
        let preflight = self.preflight_tokens.load(Ordering::Acquire);
        let quality = if preflight > 0 {
            quality_from_code(self.preflight_quality.load(Ordering::Acquire))
        } else {
            TokenCountQuality::LocalEstimate
        };
        let budget = ContextBudget::from_input_limit(mc, quality);
        let usable = budget.usable().max(1) as f32;

        // Last-turn usage excludes new messages; the larger local count avoids
        // compacting one turn late when no provider preflight count exists.
        let actual = self.last_prompt_tokens.load(Ordering::Relaxed);
        let basis = if preflight > 0 {
            preflight as f32
        } else {
            (actual as f32).max(before as f32)
        };
        let near_hard_ceiling = is_overflow(basis, mc, &self.policy);

        // Anti-thrash: if the last couple of compactions barely helped, stop —
        // UNLESS we're at the hard safety ceiling (must keep trying rather than
        // silently send an overflowing turn), or the context has since GROWN
        // ≥10% past where the backoff latched (new material = new cuts to make;
        // holding the latch there left sessions stuck over 100% of usable).
        if self.ineffective.load(Ordering::Relaxed) >= 2 && !near_hard_ceiling {
            let latched = self.latched_at.load(Ordering::Relaxed);
            if basis as u32 > latched.saturating_add(latched / 10) {
                self.ineffective.store(0, Ordering::Relaxed);
            } else {
                return Ok(passthrough(messages, before, false));
            }
        }

        let action = if forced || basis >= usable * self.policy.trigger_ratio || near_hard_ceiling {
            CompactionAction::Full
        } else if basis >= usable * self.policy.microcompact_ratio {
            CompactionAction::Prune
        } else {
            CompactionAction::None
        };
        if action == CompactionAction::None {
            return Ok(passthrough(messages, before, false));
        }

        let n = messages.len();
        let mut head_end = self.policy.protect_first_n.min(n);
        head_end = complete_head_tool_group(messages, head_end);
        let mut tail_start = tail_start_index(messages, head_end, &budget, &self.policy, counter);

        // Tool-call pairing guard: the tail must not *begin* on a tool result,
        // or its owning assistant (with the tool_call) would be summarized away,
        // leaving a dangling tool message the provider rejects. Walk the
        // boundary back to include the whole tool-call group in the tail.
        while tail_start > head_end && messages[tail_start].role == Role::Tool {
            tail_start -= 1;
        }
        if tail_start <= head_end {
            // Nothing safely compactable (e.g. one huge single turn). If we're
            // at the hard ceiling, this is the exact scenario the emergency
            // layer exists for: report overflow so the kernel refuses to send
            // rather than risk a provider context-length error.
            return Ok(passthrough(messages, before, near_hard_ceiling));
        }

        let summarized = matches!(action, CompactionAction::Full);
        let mut summary_text: Option<String> = None;
        let mut out: Vec<TrackedMessage> = Vec::with_capacity(n);
        out.extend(messages[..head_end].iter().cloned().enumerate().map(
            |(source_index, message)| TrackedMessage {
                message,
                source_index: Some(source_index),
            },
        ));
        let raw_middle = messages[head_end..tail_start]
            .iter()
            .cloned()
            .enumerate()
            .map(|(offset, message)| TrackedMessage {
                message,
                source_index: Some(head_end + offset),
            })
            .collect::<Vec<_>>();
        let deduped = dedupe_tracked_tool_outputs(
            &raw_middle,
            self.policy.prune_floor(budget.usable()),
            counter,
        );
        let microcompacted = microcompact_tracked(&group_tracked_into_turns(&deduped));
        let raw_middle_tokens: u32 = raw_middle
            .iter()
            .map(|tracked| count_msg(&tracked.message, counter))
            .sum();
        let microcompacted_tokens: u32 = microcompacted
            .iter()
            .map(|tracked| count_msg(&tracked.message, counter))
            .sum();
        let pre_pass_saved = raw_middle_tokens.saturating_sub(microcompacted_tokens);
        let middle: &[TrackedMessage] = &microcompacted;

        match action {
            CompactionAction::Prune => {
                // Preserve tool-call structure and ids while pruning bodies.
                let floor = self.policy.prune_floor(budget.usable()).max(1);
                let target = usable * self.policy.microcompact_ratio;
                let mut est = basis - pre_pass_saved as f32;
                for tracked in middle {
                    let m = &tracked.message;
                    let toks = if m.role == Role::Tool {
                        counter.count(&m.content)
                    } else {
                        0
                    };
                    if m.role == Role::Tool && toks >= floor && est >= target {
                        let mut pm = m.clone();
                        pm.content = match self
                            .artifacts
                            .as_ref()
                            .and_then(|s| s.put(m.content.as_bytes()).ok())
                        {
                            Some(hash) => format!(
                                "[tool output pruned to save context — {toks} tokens. Re-read with read_artifact hash=\"{hash}\"]"
                            ),
                            None => format!(
                                "[earlier tool output pruned to save context — {toks} tokens]"
                            ),
                        };
                        est -= (toks.saturating_sub(counter.count(&pm.content))) as f32;
                        out.push(TrackedMessage {
                            message: pm,
                            // The provider state belongs to the original full
                            // result, not the rewritten artifact pointer.
                            source_index: None,
                        });
                    } else {
                        out.push(tracked.clone());
                    }
                }
            }
            CompactionAction::Full => {
                // A mid-array system message is invalid for strict providers;
                // the chronological summary is an assistant message.
                let items: Vec<HistoryItem> = middle
                    .iter()
                    .map(|tracked| msg_to_item(&tracked.message))
                    .collect();
                let previous = self.last_summary.lock().ok().and_then(|g| g.clone());
                let primary = control
                    .run(self.summarizer.summarize(previous.as_deref(), &items))
                    .await?;
                let text = match primary {
                    Ok(s) => s,
                    Err(_) => control
                        .run(ExtractiveSummarizer.summarize(previous.as_deref(), &items))
                        .await?
                        .unwrap_or_else(|_| extractive_stub(&items)),
                };
                // Cap the summary so a runaway one can't itself blow the budget
                // (~15% of usable). Truncate on a char boundary with a marker.
                let cap = (usable * 0.15) as u32;
                let text = cap_summary(text, cap, counter);
                if let Ok(mut g) = self.last_summary.lock() {
                    *g = Some(text.clone());
                }
                summary_text = Some(text.clone());
                out.push(TrackedMessage {
                    message: Message::new(Role::Assistant, text),
                    source_index: None,
                });
            }
            CompactionAction::None => unreachable!(),
        }

        out.extend(
            messages[tail_start..]
                .iter()
                .cloned()
                .enumerate()
                .map(|(offset, message)| TrackedMessage {
                    message,
                    source_index: Some(tail_start + offset),
                }),
        );
        if summarized {
            if let (Some(refresh), Some(system)) = (
                &self.full_compaction_refresh,
                out.iter_mut()
                    .find(|tracked| tracked.message.role == Role::System),
            ) {
                system.message.content = refresh(&system.message.content);
                // A refreshed system sheath is generated content, even if a
                // particular refresh happens to return the same visible text.
                system.source_index = None;
            }
        }
        let after = out
            .iter()
            .map(|tracked| count_msg(&tracked.message, counter))
            .sum::<u32>()
            + overhead;

        // Track effectiveness: a compaction that frees <10% counts as
        // ineffective; two in a row trips the anti-thrash backoff above.
        // Remember the size we latched at so growth can release the backoff.
        if before.saturating_sub(after) < before / 10 {
            self.ineffective.fetch_add(1, Ordering::Relaxed);
            self.latched_at.store(after, Ordering::Relaxed);
        } else {
            self.ineffective.store(0, Ordering::Relaxed);
        }

        // Even after compaction, check the result against the true hard
        // ceiling — the final guard before this goes to the kernel.
        let overflow = if synthetic_limit {
            // A provider rejected the original and we do not know its limit.
            // Only a material reduction authorizes the one retry.
            after >= before.saturating_sub(before / 10)
        } else {
            is_overflow(after as f32, mc, &self.policy)
        };
        self.pending_usage_verification
            .store(true, Ordering::Release);
        self.verification_threshold.store(
            (usable * self.policy.trigger_ratio) as u32,
            Ordering::Release,
        );

        Ok(CompileResult {
            source_indices: out.iter().map(|tracked| tracked.source_index).collect(),
            messages: out.into_iter().map(|tracked| tracked.message).collect(),
            compacted: true,
            summarized,
            before_tokens: before,
            after_tokens: after,
            overflow,
            summary: summary_text,
        })
    }
}

fn complete_head_tool_group(messages: &[Message], head_end: usize) -> usize {
    if head_end == 0 || head_end >= messages.len() || messages[head_end].role != Role::Tool {
        return head_end;
    }

    let mut first_result = head_end;
    while first_result > 0 && messages[first_result - 1].role == Role::Tool {
        first_result -= 1;
    }
    let Some(owner_index) = first_result.checked_sub(1) else {
        return head_end;
    };
    let owner = &messages[owner_index];
    if owner.role != Role::Assistant || owner.tool_calls.is_empty() {
        return head_end;
    }
    let belongs_to_owner = |message: &Message| {
        message
            .tool_call_id
            .as_deref()
            .is_some_and(|id| owner.tool_calls.iter().any(|tool_call| tool_call.id == id))
    };
    if !messages[first_result..=head_end]
        .iter()
        .all(belongs_to_owner)
    {
        return head_end;
    }

    let mut end = head_end;
    while end < messages.len()
        && messages[end].role == Role::Tool
        && belongs_to_owner(&messages[end])
    {
        end += 1;
    }
    end
}

/// Last-resort summary if even the extractive fallback errors (it doesn't today,
/// but keep the promise: never emit an empty/"unavailable" summary).
fn extractive_stub(items: &[HistoryItem]) -> String {
    format!(
        "[compacted {} earlier messages; full history in the event log]",
        items.len()
    )
}

/// Cap a summary to `max_tokens`; truncate on a char boundary + a marker if over.
fn cap_summary(text: String, max_tokens: u32, counter: &dyn TokenCounter) -> String {
    if max_tokens == 0 || counter.count(&text) <= max_tokens {
        return text;
    }
    // ~4 chars/token heuristic for the cut point; leave room for the marker.
    let keep = (max_tokens as usize).saturating_mul(4);
    let mut cut = keep.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…[summary truncated to fit context]", &text[..cut])
}

/// A compiled legacy message plus the exact input occurrence it retained.
///
/// Opaque provider state is held in the kernel's ordered sidecar, so the
/// context engine cannot copy it directly. Carrying the input index through
/// every transformation lets the kernel recover the right occurrence even
/// when several messages have byte-identical legacy views.
#[derive(Clone)]
struct TrackedMessage {
    message: Message,
    source_index: Option<usize>,
}

fn group_tracked_into_turns(messages: &[TrackedMessage]) -> Vec<Vec<TrackedMessage>> {
    let mut turns = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let m = &messages[i].message;
        let mut end = i + 1;
        if m.role == Role::Assistant && !m.tool_calls.is_empty() {
            while end < messages.len()
                && end - i <= m.tool_calls.len()
                && messages[end].message.role == Role::Tool
            {
                end += 1;
            }
        }
        turns.push(messages[i..end].to_vec());
        i = end;
    }
    turns
}

/// Split messages into turns — an assistant message plus every tool result
/// answering it, as one atomic unit; a plain message is its own turn. Every
/// operation below collapses whole turns, never a partial one, so pairing
/// can't break.
#[cfg(test)]
fn group_into_turns(messages: &[Message]) -> Vec<Vec<Message>> {
    let tracked = messages
        .iter()
        .cloned()
        .map(|message| TrackedMessage {
            message,
            source_index: None,
        })
        .collect::<Vec<_>>();
    group_tracked_into_turns(&tracked)
        .into_iter()
        .map(|turn| turn.into_iter().map(|tracked| tracked.message).collect())
        .collect()
}

/// The `update_plan` snapshot in this turn, if it called that tool: `(title,
/// status)` per step, read from the tool's own echoed result.
fn plan_snapshot(turn: &[Message]) -> Option<Vec<(String, String)>> {
    let call = turn
        .first()?
        .tool_calls
        .iter()
        .find(|c| c.tool == "update_plan")?;
    let result = turn
        .iter()
        .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(call.id.as_str()))?;
    let v: serde_json::Value = serde_json::from_str(&result.content).ok()?;
    let steps = v.get("steps")?.as_array()?;
    Some(
        steps
            .iter()
            .filter_map(|s| {
                Some((
                    s.get("title")?.as_str()?.to_string(),
                    s.get("status")?.as_str()?.to_string(),
                ))
            })
            .collect(),
    )
}

#[cfg(test)]
fn microcompact(turns: &[Vec<Message>]) -> Vec<Message> {
    let tracked = turns
        .iter()
        .map(|turn| {
            turn.iter()
                .cloned()
                .map(|message| TrackedMessage {
                    message,
                    source_index: None,
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    microcompact_tracked(&tracked)
        .into_iter()
        .map(|tracked| tracked.message)
        .collect()
}

fn microcompact_tracked(turns: &[Vec<TrackedMessage>]) -> Vec<TrackedMessage> {
    let plans: Vec<(usize, Vec<(String, String)>)> = turns
        .iter()
        .enumerate()
        .filter_map(|(i, turn)| {
            let messages = turn
                .iter()
                .map(|tracked| tracked.message.clone())
                .collect::<Vec<_>>();
            plan_snapshot(&messages).map(|snapshot| (i, snapshot))
        })
        .collect();

    let mut spans: Vec<(usize, usize, Vec<String>)> = Vec::new();
    for pair in plans.windows(2) {
        let (i0, before) = &pair[0];
        let (i1, after) = &pair[1];
        if i1.saturating_sub(*i0) <= 1 {
            continue;
        }
        let titles: Vec<String> = after
            .iter()
            .filter(|(title, status)| {
                status == "completed" && before.iter().any(|(t, s)| t == title && s != "completed")
            })
            .map(|(title, _)| title.clone())
            .collect();
        if !titles.is_empty() {
            spans.push((*i0 + 1, *i1, titles));
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    let mut next_span = 0;
    while i < turns.len() {
        if next_span < spans.len() && spans[next_span].0 == i {
            let (start, end, titles) = &spans[next_span];
            for turn in &turns[*start..*end] {
                for tracked in turn {
                    // User words are never synthesized away: a mid-task steer
                    // may still bind the model ("use tabs not spaces").
                    if tracked.message.role == Role::User {
                        out.push(tracked.clone());
                    }
                }
            }
            let marker = titles
                .iter()
                .map(|title| format!("✓ {title}"))
                .collect::<Vec<_>>()
                .join("\n");
            out.push(TrackedMessage {
                // An assistant checkpoint, not system: it sits chronologically
                // mid-conversation and is generated rather than retained.
                message: Message::new(Role::Assistant, marker),
                source_index: None,
            });
            i = *end;
            next_span += 1;
        } else {
            out.extend(turns[i].iter().cloned());
            i += 1;
        }
    }
    out
}

/// A tool result byte-identical to an earlier one in `middle` is elided to a
/// pointer — content and pairing (`tool_call_id`) are otherwise untouched.
#[cfg(test)]
fn dedupe_tool_outputs(middle: &[Message], floor: u32, counter: &dyn TokenCounter) -> Vec<Message> {
    let tracked = middle
        .iter()
        .cloned()
        .map(|message| TrackedMessage {
            message,
            source_index: None,
        })
        .collect::<Vec<_>>();
    dedupe_tracked_tool_outputs(&tracked, floor, counter)
        .into_iter()
        .map(|tracked| tracked.message)
        .collect()
}

fn dedupe_tracked_tool_outputs(
    middle: &[TrackedMessage],
    floor: u32,
    counter: &dyn TokenCounter,
) -> Vec<TrackedMessage> {
    let mut seen: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    let mut out = Vec::with_capacity(middle.len());
    for tracked in middle {
        let m = &tracked.message;
        if m.role != Role::Tool || counter.count(&m.content) < floor {
            out.push(tracked.clone());
            continue;
        }
        if let Some(&first_id) = seen.get(m.content.as_str()) {
            let mut duplicate = m.clone();
            duplicate.content =
                format!("[duplicate of tool result {first_id} — identical output elided]");
            out.push(TrackedMessage {
                message: duplicate,
                source_index: None,
            });
        } else {
            seen.insert(&m.content, m.tool_call_id.as_deref().unwrap_or(""));
            out.push(tracked.clone());
        }
    }
    out
}

fn msg_to_item(m: &Message) -> HistoryItem {
    let kind = if m.role == Role::Tool {
        ItemKind::ToolOutput
    } else {
        ItemKind::Text
    };
    HistoryItem {
        role: m.role.clone(),
        content: m.content.clone(),
        kind,
        source_events: Vec::new(),
        artifact: None,
        pinned: false,
        pruned: false,
    }
}

/// Find the protected-tail boundary using the full message envelope, including
/// tool-call arguments.
fn tail_start_index(
    messages: &[Message],
    head_end: usize,
    budget: &ContextBudget,
    policy: &CompactionPolicy,
    counter: &dyn TokenCounter,
) -> usize {
    crate::compactor::tail_start_index_by(messages.len(), head_end, budget, policy, |index| {
        count_msg(&messages[index], counter)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::HeuristicCounter;
    use kernel::ToolIntent;

    fn user(s: &str) -> Message {
        Message::user(s.to_string())
    }

    /// These tests calibrate token math against chars/4 (using repeated-char
    /// strings), so they pin the heuristic counter rather than the BPE default.
    fn engine(policy: CompactionPolicy) -> PipelineEngine {
        PipelineEngine::with_counter(policy, Arc::new(HeuristicCounter))
    }

    /// `PipelineEngine::compile` receives an input-only allowance. These tests
    /// use local estimates, which reserve a 10% quality margin, so derive the
    /// allowance that produces the requested usable budget.
    fn local_input_limit(usable_tokens: u32) -> u32 {
        usable_tokens.saturating_mul(10) / 9
    }

    struct OkSummarizer(&'static str);
    #[async_trait]
    impl Summarizer for OkSummarizer {
        async fn summarize(
            &self,
            _p: Option<&str>,
            _i: &[HistoryItem],
        ) -> Result<String, crate::compactor::SummarizeError> {
            Ok(self.0.to_string())
        }
    }
    struct ErrSummarizer;
    #[async_trait]
    impl Summarizer for ErrSummarizer {
        async fn summarize(
            &self,
            _p: Option<&str>,
            _i: &[HistoryItem],
        ) -> Result<String, crate::compactor::SummarizeError> {
            Err(crate::compactor::SummarizeError::Unavailable(
                "no model".into(),
            ))
        }
    }

    fn full_compaction_history() -> Vec<Message> {
        let mut msgs = vec![Message::system("SYSTEM")];
        for i in 0..12 {
            msgs.push(user(&format!("ask {i} {}", "y".repeat(400))));
        }
        msgs.push(user("FINAL"));
        msgs
    }
    fn full_policy() -> CompactionPolicy {
        CompactionPolicy {
            protect_first_n: 1,
            protect_last_n: 2,
            tail_ratio: 0.1,
            trigger_ratio: 0.85,
            ..Default::default()
        }
    }

    #[test]
    fn tail_walk_counts_tool_call_args_not_just_text() {
        let counter = HeuristicCounter;
        let budget = ContextBudget::from_max_ctx(10_000);
        let policy = CompactionPolicy {
            protect_last_n: 1,
            tail_ratio: 0.1,
            ..Default::default()
        };
        let mut msgs: Vec<Message> = (0..6).map(|i| user(&format!("m{i}"))).collect();
        let big_args = serde_json::json!({ "content": "z".repeat(40_000) });
        msgs.push(Message::assistant_calls(
            String::new(),
            vec![ToolIntent {
                id: "1".into(),
                tool: "fs.write".into(),
                args: big_args,
            }],
        ));
        let start = tail_start_index(&msgs, 0, &budget, &policy, &counter);
        assert_eq!(
            start,
            msgs.len() - 1,
            "the args-heavy final message alone exceeds the tail budget"
        );
    }

    #[tokio::test]
    async fn full_compaction_uses_injected_summarizer_and_persists_it() {
        let eng = engine(full_policy()).with_summarizer(Arc::new(OkSummarizer("HANDOFF")));
        let r = eng
            .compile(&full_compaction_history(), Some(local_input_limit(1_300)))
            .await;
        assert!(r.compacted && r.summarized);
        assert_eq!(
            r.summary.as_deref(),
            Some("HANDOFF"),
            "summary text carried out for K12 persistence"
        );
        assert!(
            r.messages.iter().any(|m| m.content == "HANDOFF"),
            "summary is in the compacted view"
        );
    }

    #[tokio::test]
    async fn full_compaction_reports_exact_occurrences_for_duplicate_tail_values() {
        let policy = CompactionPolicy {
            protect_first_n: 3,
            protect_last_n: 2,
            tail_ratio: 0.0,
            trigger_ratio: 0.5,
            ..Default::default()
        };
        let eng = engine(policy).with_summarizer(Arc::new(OkSummarizer("HANDOFF")));
        let messages = vec![
            Message::system("SYSTEM"),
            user("head"),
            Message::new(Role::Assistant, "head response"),
            user("repeat"),
            Message::new(Role::Assistant, "same answer"),
            user(&"middle ".repeat(1_000)),
            Message::new(Role::Assistant, "middle response"),
            user("repeat"),
            Message::new(Role::Assistant, "same answer"),
        ];

        let compiled = eng.compile(&messages, Some(local_input_limit(1_000))).await;
        assert!(compiled.compacted && compiled.summarized);
        assert_eq!(
            compiled.source_indices,
            vec![Some(0), Some(1), Some(2), None, Some(7), Some(8)],
            "the protected duplicate pair must identify the tail occurrence, not its equal middle"
        );
    }

    #[tokio::test]
    async fn frozen_system_refresh_runs_only_at_full_compaction() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let eng = engine(full_policy())
            .with_summarizer(Arc::new(OkSummarizer("HANDOFF")))
            .with_full_compaction_refresh(Arc::new(move |system| {
                seen.fetch_add(1, Ordering::Relaxed);
                format!("{system}\n\n## Memory\n\nrefreshed")
            }));

        let unchanged = eng
            .compile(
                &[Message::system("SYSTEM"), user("short")],
                Some(local_input_limit(1_300)),
            )
            .await;
        assert!(!unchanged.compacted);
        assert_eq!(calls.load(Ordering::Relaxed), 0);

        let compacted = eng
            .compile(&full_compaction_history(), Some(local_input_limit(1_300)))
            .await;
        assert!(compacted.compacted && compacted.summarized);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(
            compacted.messages[0]
                .content
                .ends_with("## Memory\n\nrefreshed")
        );
    }

    #[tokio::test]
    async fn full_compaction_falls_back_to_extractive_never_unavailable() {
        let eng = engine(full_policy()).with_summarizer(Arc::new(ErrSummarizer));
        let r = eng
            .compile(&full_compaction_history(), Some(local_input_limit(1_300)))
            .await;
        assert!(r.summarized);
        let s = r.summary.expect("a summary is always produced");
        assert!(
            !s.contains("[summary unavailable]"),
            "must not emit the empty placeholder"
        );
        assert!(!s.trim().is_empty());
    }

    struct CancellableSummarizer {
        calls: std::sync::atomic::AtomicUsize,
        started: tokio::sync::Notify,
    }

    #[async_trait]
    impl Summarizer for CancellableSummarizer {
        async fn summarize(
            &self,
            _previous: Option<&str>,
            _items: &[HistoryItem],
        ) -> Result<String, crate::compactor::SummarizeError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.started.notify_one();
                std::future::pending::<()>().await;
            }
            Ok("RECOVERED SUMMARY".into())
        }
    }

    #[tokio::test]
    async fn cancellation_drops_a_running_summarizer_and_leaves_compaction_reusable() {
        let summarizer = Arc::new(CancellableSummarizer {
            calls: std::sync::atomic::AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
        });
        let eng = Arc::new(
            engine(full_policy()).with_summarizer(summarizer.clone() as Arc<dyn Summarizer>),
        );
        let control = kernel::CompileControl::unlimited();
        let cancel = control.cancellation_token().clone();
        let running = {
            let eng = Arc::clone(&eng);
            tokio::spawn(async move {
                eng.compile_controlled(
                    &full_compaction_history(),
                    Some(local_input_limit(1_300)),
                    &control,
                )
                .await
            })
        };
        summarizer.started.notified().await;
        cancel.cancel();
        assert!(matches!(
            running.await.expect("compiler task"),
            Err(kernel::ContextCompileError::Cancelled)
        ));

        let recovered = eng
            .compile_controlled(
                &full_compaction_history(),
                Some(local_input_limit(1_300)),
                &kernel::CompileControl::unlimited(),
            )
            .await
            .expect("a cancelled pass must not poison later compaction");
        assert_eq!(recovered.summary.as_deref(), Some("RECOVERED SUMMARY"));
    }

    struct RecordingSummarizer(std::sync::Mutex<Vec<Option<String>>>);
    #[async_trait]
    impl Summarizer for RecordingSummarizer {
        async fn summarize(
            &self,
            previous: Option<&str>,
            _i: &[HistoryItem],
        ) -> Result<String, crate::compactor::SummarizeError> {
            self.0.lock().unwrap().push(previous.map(str::to_string));
            Ok("SUMMARY-vN".to_string())
        }
    }

    #[tokio::test]
    async fn complete_prepared_count_including_tools_drives_the_trigger() {
        use kernel::{
            BlastRadius, ContextEngine, InputTokenCount, TokenCountQuality, ToolCategory, ToolSpec,
        };
        let tools: Vec<ToolSpec> = (0..20)
            .map(|i| ToolSpec {
                name: format!("tool_{i}"),
                description: "x".repeat(400),
                schema: serde_json::json!({ "p": "q".repeat(400) }),
                blast_radius: BlastRadius::Read,
                category: ToolCategory::Read,
                icon: "•".into(),
            })
            .collect();
        let history = full_compaction_history();
        let msg_tokens = count_all(&history, &HeuristicCounter);

        // A count for messages alone stays under the trigger.
        let plain = engine(full_policy());
        plain.update_preflight(&InputTokenCount {
            tokens: u64::from(msg_tokens),
            quality: TokenCountQuality::Authoritative,
            request_fingerprint: "messages-only".into(),
        });
        assert!(
            !plain.compile(&history, Some(8_000)).await.compacted,
            "messages alone stay under trigger"
        );

        // The provider's count of the complete prepared request includes the
        // large tool schemas and crosses the trigger without guessed overhead.
        let withtools = engine(full_policy());
        withtools.note_tools(&tools);
        withtools.update_preflight(&InputTokenCount {
            tokens: 7_000,
            quality: TokenCountQuality::Authoritative,
            request_fingerprint: "complete-request".into(),
        });
        assert!(
            withtools.compile(&history, Some(8_000)).await.compacted,
            "tool-def overhead pushes it over"
        );
    }

    #[tokio::test]
    async fn re_compaction_threads_the_previous_summary() {
        let rec = Arc::new(RecordingSummarizer(std::sync::Mutex::new(Vec::new())));
        let eng = engine(full_policy()).with_summarizer(rec.clone());
        eng.compile(&full_compaction_history(), Some(local_input_limit(1_300)))
            .await; // first Full
        eng.compile(&full_compaction_history(), Some(local_input_limit(1_300)))
            .await; // second Full
        let seen = rec.0.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(
            seen[0].is_none(),
            "first compaction has no previous summary"
        );
        assert_eq!(
            seen[1].as_deref(),
            Some("SUMMARY-vN"),
            "second UPDATES the previous summary"
        );
    }

    #[derive(Default)]
    struct MemStore(std::sync::Mutex<usize>);
    impl kernel::ArtifactStore for MemStore {
        fn put(&self, _b: &[u8]) -> Result<String, String> {
            let mut n = self.0.lock().unwrap();
            *n += 1;
            Ok(format!("h{n}"))
        }
        fn get(&self, _h: &str, _o: usize, _l: Option<usize>) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
        fn size(&self, _h: &str) -> Result<usize, String> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn anti_thrash_latch_releases_when_context_grows() {
        // Two compactions that free nothing latch the backoff. The latch must
        // RELEASE once the context grows ≥10% past the latch point — holding
        // it left sessions stuck >100% of usable with compaction refusing to
        // run until the 95%-of-true-window emergency line.
        let eng = engine(CompactionPolicy {
            protect_first_n: 1,
            protect_last_n: 2,
            tail_ratio: 0.05,
            trigger_ratio: 0.85,
            ..Default::default()
        });
        // Prune band (~3120..4420 of a 5200-token usable input allowance), but the middle holds no
        // tool results — pruning frees nothing → "ineffective" twice → latch.
        let mut msgs = vec![Message::system("S")];
        for i in 0..8 {
            msgs.push(user(&format!("ask {i} {}", "y".repeat(1_600))));
        }
        let input_limit = Some(local_input_limit(5_200));
        let r1 = eng.compile(&msgs, input_limit).await;
        assert!(
            r1.compacted && r1.before_tokens == r1.after_tokens,
            "nothing prunable"
        );
        let r2 = eng.compile(&msgs, input_limit).await;
        assert!(r2.compacted);
        let r3 = eng.compile(&msgs, input_limit).await;
        assert!(!r3.compacted, "latched after two ineffective passes");

        // Same size again → still latched.
        let r4 = eng.compile(&msgs, input_limit).await;
        assert!(!r4.compacted, "latch holds while nothing changed");

        // Growth past 10% of the latch point → released, compaction runs again
        // (now in the Full band, which CAN shrink this content).
        for i in 0..3 {
            msgs.push(user(&format!("more {i} {}", "z".repeat(1_600))));
        }
        let r5 = eng.compile(&msgs, input_limit).await;
        assert!(
            r5.compacted,
            "grown context must release the anti-thrash latch"
        );
        assert!(
            r5.after_tokens < r5.before_tokens,
            "and this pass actually shrinks"
        );
    }

    #[tokio::test]
    async fn prune_is_oldest_first_respects_floor_and_stops_at_target() {
        let eng = engine(CompactionPolicy {
            protect_first_n: 1,
            protect_last_n: 2,
            tail_ratio: 0.05,
            prune_min_tool_tokens: Some(100),
            ..Default::default()
        });
        let msgs = vec![
            Message::system("S"),
            user("ask 0"),
            Message::tool_result("c0", "a".repeat(4_400)),
            user("ask 1"),
            Message::tool_result("c1", "b".repeat(4_400)),
            user("ask 2"),
            Message::tool_result("c2", "c".repeat(200)),
            user("ask 3"),
            Message::tool_result("c3", "d".repeat(4_400)),
            user("LAST"),
        ];
        let r = eng.compile(&msgs, Some(local_input_limit(5_200))).await;
        assert!(
            r.compacted && !r.summarized,
            "expected a Prune pass (compacted={} summarized={} before={} after={})",
            r.compacted,
            r.summarized,
            r.before_tokens,
            r.after_tokens
        );
        let pruned: Vec<&Message> = r
            .messages
            .iter()
            .filter(|m| m.content.contains("pruned to save context"))
            .collect();
        assert_eq!(
            pruned.len(),
            1,
            "stop at target: only the oldest is pruned: {:?}",
            r.messages
                .iter()
                .map(|m| m.content.chars().take(30).collect::<String>())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            pruned[0].tool_call_id.as_deref(),
            Some("c0"),
            "oldest-first"
        );
        assert!(
            r.messages
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("c1") && m.content.starts_with("bbb")),
            "newer big output survives once under target"
        );
        assert!(
            r.messages
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("c2") && m.content.starts_with("ccc")),
            "small output under the floor is never pruned"
        );
    }

    #[tokio::test]
    async fn prune_spills_tool_output_to_artifacts_when_available() {
        let eng = engine(CompactionPolicy {
            protect_first_n: 1,
            protect_last_n: 2,
            tail_ratio: 0.05,
            prune_min_tool_tokens: Some(1),
            ..Default::default()
        })
        .with_artifacts(Arc::new(MemStore::default()));
        // Sized into the Prune band (>=60%, <85% of a 5200-token usable input).
        let mut msgs = vec![Message::system("S")];
        for i in 0..8 {
            msgs.push(user(&format!("ask {i}")));
            msgs.push(Message::tool_result(
                format!("c{i}"),
                format!("{i}{}", "z".repeat(1600)),
            ));
        }
        msgs.push(user("LAST"));
        let r = eng.compile(&msgs, Some(local_input_limit(5_200))).await;
        assert!(
            r.compacted && !r.summarized,
            "expected a Prune pass (summarized={})",
            r.summarized
        );
        assert!(
            r.messages
                .iter()
                .any(|m| m.content.contains("read_artifact hash=")),
            "pruned tool output must be spilled + re-fetchable"
        );
    }

    #[tokio::test]
    async fn single_uncompactable_turn_reports_overflow_not_silent_passthrough() {
        let eng = engine(CompactionPolicy::default());
        let call = ToolIntent {
            id: "c1".into(),
            tool: "fs.read".into(),
            args: serde_json::json!({}),
        };
        let msgs = vec![
            Message::system("sys"),
            user("read three huge files"),
            Message::assistant_calls("", vec![call]),
            Message::tool_result("c1", "z".repeat(40_000)), // ~10k tokens alone
        ];
        // True window small enough that this single turn exceeds the 95% ceiling.
        let r = eng.compile(&msgs, Some(local_input_limit(5_200))).await;
        assert!(
            r.overflow,
            "must flag overflow rather than silently send over budget"
        );
    }

    #[tokio::test]
    async fn no_compaction_under_budget() {
        let eng = engine(CompactionPolicy::default());
        let msgs = vec![Message::system("sys"), user("hi")];
        let r = eng.compile(&msgs, Some(200_000)).await;
        assert!(!r.compacted);
        assert_eq!(r.messages.len(), 2);
    }

    #[tokio::test]
    async fn unknown_window_passes_through() {
        let eng = engine(CompactionPolicy::default());
        let msgs = vec![Message::system("sys"), user(&"x".repeat(100_000))];
        let r = eng.compile(&msgs, None).await;
        assert!(!r.compacted, "no window => no guessing => no compaction");
    }

    #[tokio::test]
    async fn full_compaction_preserves_tool_pairing() {
        let eng = engine(CompactionPolicy {
            protect_first_n: 1,
            protect_last_n: 2,
            tail_ratio: 0.1,
            ..Default::default()
        });

        let mut msgs = vec![Message::system("SYSTEM")];
        for i in 0..12 {
            msgs.push(user(&format!("ask {i} {}", "y".repeat(400))));
            let call = ToolIntent {
                id: format!("c{i}"),
                tool: "fs.read".into(),
                args: serde_json::json!({}),
            };
            msgs.push(Message::assistant_calls("", vec![call]));
            msgs.push(Message::tool_result(format!("c{i}"), "z".repeat(400)));
        }
        msgs.push(user("FINAL QUESTION"));

        let r = eng.compile(&msgs, Some(2_000)).await; // tiny window forces Full
        assert!(r.compacted);
        assert!(r.after_tokens < r.before_tokens);

        assert_eq!(r.messages.first().unwrap().content, "SYSTEM");
        assert_eq!(r.messages.last().unwrap().content, "FINAL QUESTION");

        // Pairing invariant: every tool message is immediately preceded by an
        // assistant message that requested a matching tool_call id.
        for (i, m) in r.messages.iter().enumerate() {
            if m.role == Role::Tool {
                let id = m.tool_call_id.as_deref().unwrap();
                let prev = &r.messages[i - 1];
                assert_eq!(prev.role, Role::Assistant);
                assert!(
                    prev.tool_calls.iter().any(|c| c.id == id),
                    "dangling tool result"
                );
            }
        }
    }

    #[tokio::test]
    async fn full_compaction_does_not_leave_a_dangling_tool_call_at_the_head() {
        let eng = engine(CompactionPolicy {
            protect_first_n: 3,
            protect_last_n: 2,
            tail_ratio: 0.1,
            ..Default::default()
        });
        let mut msgs = vec![Message::system("SYSTEM"), user("first ask")];
        let head_call = ToolIntent {
            id: "head".into(),
            tool: "fs.read".into(),
            args: serde_json::json!({}),
        };
        msgs.push(Message::assistant_calls("", vec![head_call]));
        msgs.push(Message::tool_result("head", "z".repeat(400)));
        for i in 0..12 {
            msgs.push(user(&format!("ask {i} {}", "y".repeat(400))));
            let call = ToolIntent {
                id: format!("c{i}"),
                tool: "fs.read".into(),
                args: serde_json::json!({}),
            };
            msgs.push(Message::assistant_calls("", vec![call]));
            msgs.push(Message::tool_result(format!("c{i}"), "z".repeat(400)));
        }
        msgs.push(user("FINAL"));

        let r = eng.compile(&msgs, Some(2_000)).await;
        assert!(r.compacted && r.summarized, "expected a Full compaction");

        for (i, m) in r.messages.iter().enumerate() {
            for call in &m.tool_calls {
                let paired = r.messages[i + 1..].iter().any(|later| {
                    later.role == Role::Tool && later.tool_call_id.as_deref() == Some(&call.id)
                });
                assert!(
                    paired,
                    "dangling tool_call `{}` at head boundary (index {i})",
                    call.id
                );
            }
        }
    }

    #[tokio::test]
    async fn full_compaction_keeps_a_parallel_tool_group_atomic_at_the_head() {
        let eng = engine(CompactionPolicy {
            protect_first_n: 4,
            protect_last_n: 2,
            tail_ratio: 0.1,
            ..Default::default()
        });
        let calls = vec![
            ToolIntent {
                id: "parallel-a".into(),
                tool: "fs.read".into(),
                args: serde_json::json!({}),
            },
            ToolIntent {
                id: "parallel-b".into(),
                tool: "grep".into(),
                args: serde_json::json!({}),
            },
        ];
        let mut msgs = vec![
            Message::system("SYSTEM"),
            user("parallel ask"),
            Message::assistant_calls("", calls),
            Message::tool_result("parallel-a", "first result"),
            Message::tool_result("parallel-b", "second result"),
        ];
        for i in 0..12 {
            msgs.push(user(&format!("ask {i} {}", "y".repeat(400))));
            let call = ToolIntent {
                id: format!("middle-{i}"),
                tool: "fs.read".into(),
                args: serde_json::json!({}),
            };
            msgs.push(Message::assistant_calls("", vec![call]));
            msgs.push(Message::tool_result(format!("middle-{i}"), "z".repeat(400)));
        }
        msgs.push(user("FINAL"));

        let compiled = eng.compile(&msgs, Some(2_000)).await;
        assert!(compiled.compacted && compiled.summarized);
        for id in ["parallel-a", "parallel-b"] {
            assert!(
                compiled
                    .messages
                    .iter()
                    .any(|message| message.tool_call_id.as_deref() == Some(id)),
                "parallel result {id} was split from its retained assistant"
            );
        }
    }

    fn update_plan_turn(id: &str, steps: &[(&str, &str)]) -> Vec<Message> {
        let call = ToolIntent {
            id: id.into(),
            tool: "update_plan".into(),
            args: serde_json::json!({}),
        };
        let steps_json: Vec<serde_json::Value> = steps
            .iter()
            .map(|(t, s)| serde_json::json!({"title": t, "status": s}))
            .collect();
        let result = serde_json::json!({"steps": steps_json}).to_string();
        vec![
            Message::assistant_calls(String::new(), vec![call]),
            Message::tool_result(id, result),
        ]
    }

    fn work_turn(id: &str, tool: &str, output: &str) -> Vec<Message> {
        let call = ToolIntent {
            id: id.into(),
            tool: tool.into(),
            args: serde_json::json!({}),
        };
        vec![
            Message::assistant_calls(String::new(), vec![call]),
            Message::tool_result(id, output),
        ]
    }

    #[test]
    fn dedupe_elides_a_repeated_tool_output_but_keeps_the_first() {
        let repeated = "a".repeat(400);
        let middle = vec![
            Message::tool_result("c0", repeated.clone()),
            user("ask"),
            Message::tool_result("c1", repeated.clone()),
        ];
        let out = dedupe_tool_outputs(&middle, 10, &HeuristicCounter);
        assert_eq!(out[0].content, repeated, "first occurrence kept verbatim");
        assert!(
            out[2].content.contains("duplicate of tool result c0"),
            "{}",
            out[2].content
        );
    }

    #[test]
    fn microcompact_collapses_turns_between_a_step_becoming_completed() {
        let turns = vec![
            update_plan_turn(
                "p1",
                &[("write foo", "in_progress"), ("write bar", "pending")],
            ),
            vec![user("working on foo")],
            work_turn("w1", "fs.write", "wrote foo"),
            update_plan_turn(
                "p2",
                &[("write foo", "completed"), ("write bar", "in_progress")],
            ),
            vec![user("now bar")],
        ];
        let out = microcompact(&turns);
        assert!(out.iter().any(|m| m.content == "✓ write foo"));
        assert!(!out.iter().any(|m| m.content.contains("wrote foo")));
        // The plan turns themselves are untouched, only the work between them collapses.
        assert!(out.iter().any(|m| m.content.contains("write bar")));
        assert!(out.iter().any(|m| m.content == "now bar"));
        // User words inside the collapsed window survive verbatim — a mid-task
        // steer may still bind the model.
        assert!(
            out.iter()
                .any(|m| m.role == Role::User && m.content == "working on foo"),
            "user message in the collapsed span must be preserved"
        );
    }

    #[test]
    fn microcompact_emits_one_marker_per_step_completed_in_the_same_window() {
        let turns = vec![
            update_plan_turn("p1", &[("a", "in_progress"), ("b", "pending")]),
            work_turn("w1", "fs.write", "did both"),
            update_plan_turn("p2", &[("a", "completed"), ("b", "completed")]),
        ];
        let out = microcompact(&turns);
        let marker = out
            .iter()
            .find(|m| m.content.contains('✓'))
            .expect("checkpoint marker");
        assert!(
            marker.content.contains("✓ a") && marker.content.contains("✓ b"),
            "{}",
            marker.content
        );
        assert!(!out.iter().any(|m| m.content.contains("did both")));
    }

    #[test]
    fn group_into_turns_does_not_swallow_a_user_steer_after_an_interrupted_turn() {
        // Assistant fired two calls but only one result landed (interrupt);
        // the user's steer must start its own turn, not be glued into the
        // tool-call group where a collapse could delete it.
        let calls = vec![
            ToolIntent {
                id: "a".into(),
                tool: "fs.read".into(),
                args: serde_json::json!({}),
            },
            ToolIntent {
                id: "b".into(),
                tool: "fs.read".into(),
                args: serde_json::json!({}),
            },
        ];
        let msgs = vec![
            Message::assistant_calls(String::new(), calls),
            Message::tool_result("a", "partial"),
            user("stop — do it differently"),
        ];
        let turns = group_into_turns(&msgs);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].len(), 2, "assistant + its one landed result");
        assert_eq!(turns[1][0].role, Role::User);
    }

    #[test]
    fn microcompact_is_a_noop_without_a_completed_transition() {
        let turns = vec![
            update_plan_turn("p1", &[("a", "pending")]),
            vec![user("x")],
            update_plan_turn("p2", &[("a", "in_progress")]),
        ];
        let out = microcompact(&turns);
        let flat: Vec<Message> = turns.into_iter().flatten().collect();
        assert_eq!(out.len(), flat.len());
    }

    #[test]
    fn microcompact_collapses_a_multi_tool_call_turn_atomically() {
        let calls = vec![
            ToolIntent {
                id: "a".into(),
                tool: "fs.read".into(),
                args: serde_json::json!({}),
            },
            ToolIntent {
                id: "b".into(),
                tool: "fs.read".into(),
                args: serde_json::json!({}),
            },
        ];
        let work = vec![
            Message::assistant_calls(String::new(), calls),
            Message::tool_result("a", "content a"),
            Message::tool_result("b", "content b"),
        ];
        let turns = vec![
            update_plan_turn("p1", &[("read files", "in_progress")]),
            work,
            update_plan_turn("p2", &[("read files", "completed")]),
        ];
        let out = microcompact(&turns);
        assert!(out.iter().any(|m| m.content == "✓ read files"));
        assert!(
            !out.iter()
                .any(|m| m.content.contains("content a") || m.content.contains("content b"))
        );
        for m in &out {
            for c in &m.tool_calls {
                assert!(
                    out.iter().any(|r| r.role == Role::Tool
                        && r.tool_call_id.as_deref() == Some(c.id.as_str())),
                    "dangling call {}",
                    c.id
                );
            }
        }
    }

    #[tokio::test]
    async fn compile_microcompacts_a_completed_plan_step_before_pruning() {
        let eng = engine(CompactionPolicy {
            protect_first_n: 1,
            protect_last_n: 1,
            tail_ratio: 0.0,
            prune_min_tool_tokens: Some(100),
            ..Default::default()
        });
        let mut msgs = vec![Message::system("S")];
        msgs.extend(update_plan_turn("p1", &[("big task", "in_progress")]));
        msgs.extend(work_turn("w0", "fs.read", &"a".repeat(15_000)));
        msgs.extend(update_plan_turn("p2", &[("big task", "completed")]));
        msgs.push(user("LAST"));

        let r = eng.compile(&msgs, Some(local_input_limit(5_200))).await;
        assert!(
            r.compacted,
            "expected compaction to fire (before={})",
            r.before_tokens
        );
        assert!(r.messages.iter().any(|m| m.content == "✓ big task"));
        assert!(
            !r.messages.iter().any(|m| m.content.len() > 1_000),
            "the big output must be gone, not just pruned"
        );
        assert_no_mid_array_system(&r.messages);
    }

    fn assert_no_mid_array_system(messages: &[Message]) {
        for (i, m) in messages.iter().enumerate() {
            assert!(
                i == 0 || m.role != Role::System,
                "mid-array system message at index {i}: {:?}",
                m.content
            );
        }
    }
}
