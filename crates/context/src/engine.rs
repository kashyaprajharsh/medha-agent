//! `PipelineEngine` — the default `ContextEngine` (§4.3). Operates at the
//! kernel `Message` level (not the standalone `HistoryItem`) because it must
//! preserve OpenAI tool-call pairing: an assistant message that requested tool
//! calls and the tool results answering it must never be split across the
//! summarize boundary, or the provider rejects the request. It reuses the
//! budget / policy / token / summarizer primitives; the `compactor` module
//! remains the standalone-tested engine over `HistoryItem`.

use crate::budget::ContextBudget;
use crate::compactor::{ExtractiveSummarizer, HistoryItem, ItemKind, Summarizer};
use crate::policy::{CompactionAction, CompactionPolicy};
use crate::tokens::{BpeCounter, TokenCounter};
use async_trait::async_trait;
use kernel::{CompileResult, ContextEngine, Message, Role};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

pub struct PipelineEngine {
    policy: CompactionPolicy,
    /// Token counter for the pre-flight estimate and per-item boundaries. A real
    /// BPE tokenizer in production; injectable (a model-exact tokenizer, or the
    /// heuristic for tests) via [`PipelineEngine::with_counter`].
    counter: Arc<dyn TokenCounter>,
    /// Real prompt tokens from the provider's last response (0 = unknown). The
    /// authoritative basis for the compaction decision — not an estimate.
    last_prompt_tokens: AtomicU32,
    /// Consecutive compactions that barely helped; backs off to avoid the
    /// "compact every turn" thrash.
    ineffective: AtomicU32,
    /// Context size when the anti-thrash backoff latched. Growth past this
    /// releases the latch: new material means compaction can find new cuts —
    /// without this, a latched session sat at >100% of usable with compaction
    /// refusing to run until the 95%-of-true-window emergency line.
    latched_at: AtomicU32,
    /// Summarizer for Full compaction. Defaults to the deterministic extractive
    /// fallback; the CLI injects an `LlmSummarizer` for real summaries.
    summarizer: Arc<dyn Summarizer>,
    /// Last summary produced, fed back as `previous` so re-compaction UPDATES it
    /// (iterative re-summary) instead of summarizing a gist-of-a-gist.
    last_summary: std::sync::Mutex<Option<String>>,
    /// Artifact store for lossless prune: pruned tool output is spilled here and
    /// referenced by hash, so the model can `read_artifact` it back (P1-3). When
    /// absent (tests), prune falls back to an honest non-recoverable placeholder.
    artifacts: Option<Arc<dyn kernel::ArtifactStore>>,
    /// Fixed per-request tool-definition token overhead, sized once by
    /// `note_tools` and added to every estimate (P1-9).
    tool_overhead: AtomicU32,
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
            ineffective: AtomicU32::new(0),
            latched_at: AtomicU32::new(0),
            summarizer: Arc::new(ExtractiveSummarizer),
            last_summary: std::sync::Mutex::new(None),
            artifacts: None,
            tool_overhead: AtomicU32::new(0),
        }
    }

    /// Inject the summarizer used for Full compaction (e.g. an LLM summarizer).
    pub fn with_summarizer(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = summarizer;
        self
    }

    /// Inject the artifact store so pruned tool output is spilled + re-fetchable
    /// (lossless prune, P1-3).
    pub fn with_artifacts(mut self, artifacts: Arc<dyn kernel::ArtifactStore>) -> Self {
        self.artifacts = Some(artifacts);
        self
    }
}

impl Default for PipelineEngine {
    fn default() -> Self {
        Self::new(CompactionPolicy::default())
    }
}

/// One message's estimate for the budget walks: the full tool-call envelope
/// (name + args + id), not just text content — otherwise tool-heavy turns are
/// badly undercounted (P1-9).
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

/// Extra tokens per tool for chat-template scaffolding (headers/instructions the
/// serialized schema doesn't capture). Biased safe-high — undercount is the risk.
const PER_TOOL_SCAFFOLD_TOKENS: u32 = 18;

#[async_trait]
impl ContextEngine for PipelineEngine {
    fn update_usage(&self, prompt_tokens: u32, _total_tokens: u32) {
        // Real usage already counts tool defs — store verbatim.
        self.last_prompt_tokens.store(prompt_tokens, Ordering::Relaxed);
    }

    fn update_estimate(&self, count: u32) {
        // Server count omits tool defs — add the overhead so the basis matches
        // what's actually sent (P1-9).
        self.last_prompt_tokens
            .store(count + self.tool_overhead.load(Ordering::Relaxed), Ordering::Relaxed);
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

    async fn compile(&self, messages: &[Message], max_ctx: Option<u32>) -> CompileResult {
        let counter: &dyn TokenCounter = self.counter.as_ref();
        // Include the fixed tool-def overhead so the estimate matches the real
        // request size (tool defs are sent every turn but not in `count_all`).
        let overhead = self.tool_overhead.load(Ordering::Relaxed);
        let before = count_all(messages, counter) + overhead;

        // Unknown window → never guess; send as-is (the budget cannot be sized;
        // overflow is unknowable too, so we can't claim it — false).
        let Some(mc) = max_ctx else {
            return passthrough(messages, before, false);
        };
        let budget = ContextBudget::from_max_ctx(mc);
        let usable = budget.usable().max(1) as f32;

        // Decision basis: the max of the last reported/counted figure and the
        // local count of the CURRENT messages. On hosts with no count route the
        // stored figure is last turn's usage, which excludes this turn's tool
        // results — trusting it alone triggered compaction one turn late (P2).
        // max() biases toward compacting earlier, the safe direction.
        let actual = self.last_prompt_tokens.load(Ordering::Relaxed);
        let basis = (actual as f32).max(before as f32);
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
                return passthrough(messages, before, false);
            }
        }

        let action = if basis >= usable * self.policy.trigger_ratio || near_hard_ceiling {
            CompactionAction::Full
        } else if basis >= usable * self.policy.microcompact_ratio {
            CompactionAction::Prune
        } else {
            CompactionAction::None
        };
        if action == CompactionAction::None {
            return passthrough(messages, before, false);
        }

        let n = messages.len();
        let mut head_end = self.policy.protect_first_n.min(n);
        // Head-boundary pairing guard (mirror of the tail guard below): the head
        // must not *end* on an assistant message that carries tool_calls — its
        // tool results live in the middle, and Full compaction would summarize
        // them away, leaving a dangling tool_calls message the provider rejects
        // with a 400. Extend the head forward to swallow the whole tool-result
        // group so the call and its results stay together.
        if head_end > 0
            && head_end < n
            && messages[head_end - 1].role == Role::Assistant
            && !messages[head_end - 1].tool_calls.is_empty()
        {
            while head_end < n && messages[head_end].role == Role::Tool {
                head_end += 1;
            }
        }
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
            return passthrough(messages, before, near_hard_ceiling);
        }

        let summarized = matches!(action, CompactionAction::Full);
        let mut summary_text: Option<String> = None;
        let mut out: Vec<Message> = Vec::with_capacity(n);
        out.extend_from_slice(&messages[..head_end]);
        let middle = &messages[head_end..tail_start];

        match action {
            CompactionAction::Prune => {
                // Cheap, lossless: shrink tool-result bodies, keep structure
                // (and tool_call_id) so pairing is untouched. Oldest-first,
                // honoring the prune floor, and STOP once pressure is back
                // under the prune trigger — wiping the whole middle at 60%
                // threw away recent context the model was still using (P2).
                let floor = self.policy.prune_floor(budget.usable()).max(1);
                let target = usable * self.policy.microcompact_ratio;
                let mut est = basis;
                for m in middle {
                    let toks = if m.role == Role::Tool { counter.count(&m.content) } else { 0 };
                    if m.role == Role::Tool && toks >= floor && est >= target {
                        let mut pm = m.clone();
                        // Lossless prune: spill the full output and reference it so
                        // the model can re-read it (P1-3). No store → honest
                        // non-recoverable placeholder.
                        pm.content = match self.artifacts.as_ref().and_then(|s| s.put(m.content.as_bytes()).ok()) {
                            Some(hash) => format!(
                                "[tool output pruned to save context — {toks} tokens. Re-read with read_artifact hash=\"{hash}\"]"
                            ),
                            None => format!("[earlier tool output pruned to save context — {toks} tokens]"),
                        };
                        est -= (toks.saturating_sub(counter.count(&pm.content))) as f32;
                        out.push(pm);
                    } else {
                        out.push(m.clone());
                    }
                }
            }
            CompactionAction::Full => {
                // Replace the whole middle with one summary system message. The
                // full history is retained by the kernel/log (P3).
                let items: Vec<HistoryItem> = middle.iter().map(msg_to_item).collect();
                // Injected summarizer (LLM), then extractive fallback — never the
                // useless "[summary unavailable]" placeholder that produced
                // hallucination-inducing empty context.
                // Feed the last summary back so the model UPDATES it (iterative).
                let previous = self.last_summary.lock().ok().and_then(|g| g.clone());
                let text = match self.summarizer.summarize(previous.as_deref(), &items).await {
                    Ok(s) => s,
                    Err(_) => ExtractiveSummarizer
                        .summarize(previous.as_deref(), &items)
                        .await
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
                out.push(Message::system(text));
            }
            CompactionAction::None => unreachable!(),
        }

        out.extend_from_slice(&messages[tail_start..]);
        let after = count_all(&out, counter) + overhead;

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
        let overflow = is_overflow(after as f32, mc, &self.policy);

        CompileResult {
            messages: out,
            compacted: true,
            summarized,
            before_tokens: before,
            after_tokens: after,
            overflow,
            summary: summary_text,
        }
    }
}

/// Last-resort summary if even the extractive fallback errors (it doesn't today,
/// but keep the promise: never emit an empty/"unavailable" summary).
fn extractive_stub(items: &[HistoryItem]) -> String {
    format!("[compacted {} earlier messages; full history in the event log]", items.len())
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

fn msg_to_item(m: &Message) -> HistoryItem {
    let kind = if m.role == Role::Tool { ItemKind::ToolOutput } else { ItemKind::Text };
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

/// Walk back from the end, keeping messages until the tail token budget is met,
/// never fewer than `protect_last_n`, never crossing into the head.
fn tail_start_index(
    messages: &[Message],
    head_end: usize,
    budget: &ContextBudget,
    policy: &CompactionPolicy,
    counter: &dyn TokenCounter,
) -> usize {
    let tail_budget = (budget.usable() as f32 * policy.tail_ratio) as u32;
    let mut acc = 0u32;
    let mut start = messages.len();
    while start > head_end {
        let candidate = start - 1;
        // Full envelope incl. tool-call args (P1-9): an assistant message whose
        // args carry a whole file must count as such, or the "protected tail"
        // walks far deeper than the budget it claims to respect.
        acc += count_msg(&messages[candidate], counter);
        // The candidate joins the tail before the threshold check — `kept` and
        // `acc` include it, so excluding it protected one message fewer than
        // `protect_last_n` promises.
        start = candidate;
        let kept = messages.len() - candidate;
        if kept >= policy.protect_last_n && acc >= tail_budget {
            break;
        }
    }
    start.max(head_end)
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

    struct OkSummarizer(&'static str);
    #[async_trait]
    impl Summarizer for OkSummarizer {
        async fn summarize(&self, _p: Option<&str>, _i: &[HistoryItem]) -> Result<String, crate::compactor::SummarizeError> {
            Ok(self.0.to_string())
        }
    }
    struct ErrSummarizer;
    #[async_trait]
    impl Summarizer for ErrSummarizer {
        async fn summarize(&self, _p: Option<&str>, _i: &[HistoryItem]) -> Result<String, crate::compactor::SummarizeError> {
            Err(crate::compactor::SummarizeError::Unavailable("no model".into()))
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
        CompactionPolicy { protect_first_n: 1, protect_last_n: 2, tail_ratio: 0.1, ..Default::default() }
    }

    #[test]
    fn tail_walk_counts_tool_call_args_not_just_text() {
        // P1-9 (tail half): an assistant message whose tool-call args carry a
        // big payload must weigh its full size in the tail-budget walk. With
        // args counted, the tail budget is filled by the last message alone;
        // uncounted (content is empty), the walk would run past it.
        let counter = HeuristicCounter;
        let budget = ContextBudget::from_max_ctx(10_000);
        let policy = CompactionPolicy { protect_last_n: 1, tail_ratio: 0.1, ..Default::default() };
        let mut msgs: Vec<Message> = (0..6).map(|i| user(&format!("m{i}"))).collect();
        let big_args = serde_json::json!({ "content": "z".repeat(40_000) });
        msgs.push(Message::assistant_calls(
            String::new(),
            vec![ToolIntent { id: "1".into(), tool: "fs.write".into(), args: big_args }],
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
        let r = eng.compile(&full_compaction_history(), Some(2_000)).await;
        assert!(r.compacted && r.summarized);
        assert_eq!(r.summary.as_deref(), Some("HANDOFF"), "summary text carried out for K12 persistence");
        assert!(r.messages.iter().any(|m| m.content == "HANDOFF"), "summary is in the compacted view");
    }

    #[tokio::test]
    async fn full_compaction_falls_back_to_extractive_never_unavailable() {
        let eng = engine(full_policy()).with_summarizer(Arc::new(ErrSummarizer));
        let r = eng.compile(&full_compaction_history(), Some(2_000)).await;
        assert!(r.summarized);
        let s = r.summary.expect("a summary is always produced");
        assert!(!s.contains("[summary unavailable]"), "must not emit the empty placeholder");
        assert!(!s.trim().is_empty());
    }

    struct RecordingSummarizer(std::sync::Mutex<Vec<Option<String>>>);
    #[async_trait]
    impl Summarizer for RecordingSummarizer {
        async fn summarize(&self, previous: Option<&str>, _i: &[HistoryItem]) -> Result<String, crate::compactor::SummarizeError> {
            self.0.lock().unwrap().push(previous.map(str::to_string));
            Ok("SUMMARY-vN".to_string())
        }
    }

    #[tokio::test]
    async fn tool_def_overhead_lifts_the_preflight_basis_over_the_trigger() {
        use kernel::{BlastRadius, ContextEngine, ToolCategory, ToolSpec};
        // A big tool set → sizable fixed overhead a tool-blind count ignores.
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

        // Tool-blind pre-flight basis alone stays under the trigger → no compaction.
        let plain = engine(full_policy());
        plain.update_estimate(msg_tokens);
        assert!(!plain.compile(&history, Some(8_000)).await.compacted, "messages alone stay under trigger");

        // Same basis + the noted tool-def overhead crosses the trigger → compaction.
        let withtools = engine(full_policy());
        withtools.note_tools(&tools);
        withtools.update_estimate(msg_tokens);
        assert!(withtools.compile(&history, Some(8_000)).await.compacted, "tool-def overhead pushes it over");
    }

    #[tokio::test]
    async fn re_compaction_threads_the_previous_summary() {
        let rec = Arc::new(RecordingSummarizer(std::sync::Mutex::new(Vec::new())));
        let eng = engine(full_policy()).with_summarizer(rec.clone());
        eng.compile(&full_compaction_history(), Some(2_000)).await; // first Full
        eng.compile(&full_compaction_history(), Some(2_000)).await; // second Full
        let seen = rec.0.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].is_none(), "first compaction has no previous summary");
        assert_eq!(seen[1].as_deref(), Some("SUMMARY-vN"), "second UPDATES the previous summary");
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
            ..Default::default()
        });
        // Prune band (~3120..4420 of usable 5200 @8k), but the middle holds no
        // tool results — pruning frees nothing → "ineffective" twice → latch.
        let mut msgs = vec![Message::system("S")];
        for i in 0..8 {
            msgs.push(user(&format!("ask {i} {}", "y".repeat(1_600))));
        }
        let r1 = eng.compile(&msgs, Some(8_000)).await;
        assert!(r1.compacted && r1.before_tokens == r1.after_tokens, "nothing prunable");
        let r2 = eng.compile(&msgs, Some(8_000)).await;
        assert!(r2.compacted);
        let r3 = eng.compile(&msgs, Some(8_000)).await;
        assert!(!r3.compacted, "latched after two ineffective passes");

        // Same size again → still latched.
        let r4 = eng.compile(&msgs, Some(8_000)).await;
        assert!(!r4.compacted, "latch holds while nothing changed");

        // Growth past 10% of the latch point → released, compaction runs again
        // (now in the Full band, which CAN shrink this content).
        for i in 0..3 {
            msgs.push(user(&format!("more {i} {}", "z".repeat(1_600))));
        }
        let r5 = eng.compile(&msgs, Some(8_000)).await;
        assert!(r5.compacted, "grown context must release the anti-thrash latch");
        assert!(r5.after_tokens < r5.before_tokens, "and this pass actually shrinks");
    }

    #[tokio::test]
    async fn prune_is_oldest_first_respects_floor_and_stops_at_target() {
        // P2: the prune tier must (a) skip outputs under the floor, (b) prune
        // oldest-first, (c) STOP once pressure is back under the prune trigger
        // — not wipe every middle tool result at 60%.
        let eng = engine(CompactionPolicy {
            protect_first_n: 1,
            protect_last_n: 2,
            tail_ratio: 0.05,
            prune_min_tool_tokens: Some(100),
            ..Default::default()
        });
        // usable(8k) ≈ 5200; band [3120, 4420). Three 1100-token outputs + one
        // 50-token one ≈ 3360 → Prune. Pruning the OLDEST (-~1075) lands under
        // target 3120, so the rest must survive.
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
        let r = eng.compile(&msgs, Some(8_000)).await;
        assert!(r.compacted && !r.summarized, "expected a Prune pass (compacted={} summarized={} before={} after={})", r.compacted, r.summarized, r.before_tokens, r.after_tokens);
        let pruned: Vec<&Message> =
            r.messages.iter().filter(|m| m.content.contains("pruned to save context")).collect();
        assert_eq!(pruned.len(), 1, "stop at target: only the oldest is pruned: {:?}",
            r.messages.iter().map(|m| m.content.chars().take(30).collect::<String>()).collect::<Vec<_>>());
        assert_eq!(pruned[0].tool_call_id.as_deref(), Some("c0"), "oldest-first");
        assert!(
            r.messages.iter().any(|m| m.tool_call_id.as_deref() == Some("c1") && m.content.starts_with("bbb")),
            "newer big output survives once under target"
        );
        assert!(
            r.messages.iter().any(|m| m.tool_call_id.as_deref() == Some("c2") && m.content.starts_with("ccc")),
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
        // Sized into the Prune band (>=60%, <85% of usable ~5200 for an 8k window).
        let mut msgs = vec![Message::system("S")];
        for i in 0..8 {
            msgs.push(user(&format!("ask {i}")));
            msgs.push(Message::tool_result(format!("c{i}"), "z".repeat(1600)));
        }
        msgs.push(user("LAST"));
        let r = eng.compile(&msgs, Some(8_000)).await;
        assert!(r.compacted && !r.summarized, "expected a Prune pass (summarized={})", r.summarized);
        assert!(
            r.messages.iter().any(|m| m.content.contains("read_artifact hash=")),
            "pruned tool output must be spilled + re-fetchable"
        );
    }

    #[tokio::test]
    async fn single_uncompactable_turn_reports_overflow_not_silent_passthrough() {
        // Regression test for the exact bug found earlier: one giant turn
        // (a single tool-call group right after the head) that cannot be
        // safely compacted (pairing guard blocks it) and pushes past the true
        // hard ceiling. Must report overflow=true, not silently pass through.
        let eng = engine(CompactionPolicy::default());
        let call = ToolIntent { id: "c1".into(), tool: "fs.read".into(), args: serde_json::json!({}) };
        let msgs = vec![
            Message::system("sys"),
            user("read three huge files"),
            Message::assistant_calls("", vec![call]),
            Message::tool_result("c1", "z".repeat(40_000)), // ~10k tokens alone
        ];
        // True window small enough that this single turn exceeds the 95% ceiling.
        let r = eng.compile(&msgs, Some(8_000)).await;
        assert!(r.overflow, "must flag overflow rather than silently send over budget");
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

        // Build a long history with assistant->tool pairs in the middle.
        let mut msgs = vec![Message::system("SYSTEM")];
        for i in 0..12 {
            msgs.push(user(&format!("ask {i} {}", "y".repeat(400))));
            let call = ToolIntent { id: format!("c{i}"), tool: "fs.read".into(), args: serde_json::json!({}) };
            msgs.push(Message::assistant_calls("", vec![call]));
            msgs.push(Message::tool_result(format!("c{i}"), "z".repeat(400)));
        }
        msgs.push(user("FINAL QUESTION"));

        let r = eng.compile(&msgs, Some(2_000)).await; // tiny window forces Full
        assert!(r.compacted);
        assert!(r.after_tokens < r.before_tokens);

        // Head + tail survive; a summary exists.
        assert_eq!(r.messages.first().unwrap().content, "SYSTEM");
        assert_eq!(r.messages.last().unwrap().content, "FINAL QUESTION");

        // Pairing invariant: every tool message is immediately preceded by an
        // assistant message that requested a matching tool_call id.
        for (i, m) in r.messages.iter().enumerate() {
            if m.role == Role::Tool {
                let id = m.tool_call_id.as_deref().unwrap();
                let prev = &r.messages[i - 1];
                assert_eq!(prev.role, Role::Assistant);
                assert!(prev.tool_calls.iter().any(|c| c.id == id), "dangling tool result");
            }
        }
    }

    #[tokio::test]
    async fn full_compaction_does_not_leave_a_dangling_tool_call_at_the_head() {
        // Regression for P0-2: with protect_first_n=3, the last kept head
        // message (index 2) is an assistant WITH tool_calls whose result sits in
        // the middle. Full compaction must not summarize that result away and
        // leave a dangling tool_calls message (provider 400) — the head guard
        // should extend the head to keep the call and its result together.
        let eng = engine(CompactionPolicy {
            protect_first_n: 3,
            protect_last_n: 2,
            tail_ratio: 0.1,
            ..Default::default()
        });
        let mut msgs = vec![Message::system("SYSTEM"), user("first ask")];
        // Index 2: assistant with a tool call whose result is the next message.
        let head_call = ToolIntent { id: "head".into(), tool: "fs.read".into(), args: serde_json::json!({}) };
        msgs.push(Message::assistant_calls("", vec![head_call]));
        msgs.push(Message::tool_result("head", "z".repeat(400)));
        // Enough middle turns to force a Full compaction under a tiny window.
        for i in 0..12 {
            msgs.push(user(&format!("ask {i} {}", "y".repeat(400))));
            let call = ToolIntent { id: format!("c{i}"), tool: "fs.read".into(), args: serde_json::json!({}) };
            msgs.push(Message::assistant_calls("", vec![call]));
            msgs.push(Message::tool_result(format!("c{i}"), "z".repeat(400)));
        }
        msgs.push(user("FINAL"));

        let r = eng.compile(&msgs, Some(2_000)).await;
        assert!(r.compacted && r.summarized, "expected a Full compaction");

        // Forward pairing invariant (the one P0-2 breaks): every tool_call id in
        // any assistant message must have a matching tool_result later in the
        // request — no dangling call.
        for (i, m) in r.messages.iter().enumerate() {
            for call in &m.tool_calls {
                let paired = r.messages[i + 1..]
                    .iter()
                    .any(|later| later.role == Role::Tool && later.tool_call_id.as_deref() == Some(&call.id));
                assert!(paired, "dangling tool_call `{}` at head boundary (index {i})", call.id);
            }
        }
    }
}
