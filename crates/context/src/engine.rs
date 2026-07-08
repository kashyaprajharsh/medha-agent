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
        }
    }
}

impl Default for PipelineEngine {
    fn default() -> Self {
        Self::new(CompactionPolicy::default())
    }
}

/// Estimate tokens for the message-selection budget walks (head/tail). Counts
/// the full tool-call envelope (name + args + id), not just text content —
/// otherwise tool-heavy turns are badly undercounted.
fn count_all(messages: &[Message], counter: &dyn TokenCounter) -> u32 {
    messages
        .iter()
        .map(|m| {
            let mut t = counter.count(&m.content);
            for tc in &m.tool_calls {
                t += counter.count(&tc.tool) + counter.count(&tc.args.to_string()) + 4;
            }
            if let Some(id) = &m.tool_call_id {
                t += counter.count(id);
            }
            t
        })
        .sum()
}

fn passthrough(messages: &[Message], tokens: u32, overflow: bool) -> CompileResult {
    CompileResult {
        messages: messages.to_vec(),
        compacted: false,
        summarized: false,
        before_tokens: tokens,
        after_tokens: tokens,
        overflow,
    }
}

/// Hard safety ceiling check against the *true* model window (not the reduced
/// usable budget) — the second, independent layer above the normal trigger.
fn is_overflow(tokens: f32, true_max_ctx: u32, policy: &CompactionPolicy) -> bool {
    tokens >= true_max_ctx as f32 * policy.emergency_ratio
}

#[async_trait]
impl ContextEngine for PipelineEngine {
    fn update_usage(&self, prompt_tokens: u32, _total_tokens: u32) {
        self.last_prompt_tokens.store(prompt_tokens, Ordering::Relaxed);
    }

    async fn compile(&self, messages: &[Message], max_ctx: Option<u32>) -> CompileResult {
        let counter: &dyn TokenCounter = self.counter.as_ref();
        let before = count_all(messages, counter);

        // Unknown window → never guess; send as-is (the budget cannot be sized;
        // overflow is unknowable too, so we can't claim it — false).
        let Some(mc) = max_ctx else {
            return passthrough(messages, before, false);
        };
        let budget = ContextBudget::from_max_ctx(mc);
        let usable = budget.usable().max(1) as f32;

        // Decide on the *real* last-reported prompt tokens when we have them;
        // fall back to the estimate only before the first response.
        let actual = self.last_prompt_tokens.load(Ordering::Relaxed);
        let basis = if actual > 0 { actual as f32 } else { before as f32 };
        let near_hard_ceiling = is_overflow(basis, mc, &self.policy);

        // Anti-thrash: if the last couple of compactions barely helped, stop —
        // UNLESS we're at the hard safety ceiling, in which case we must keep
        // trying rather than silently send an overflowing turn (the second,
        // independent safety layer above the soft trigger).
        if self.ineffective.load(Ordering::Relaxed) >= 2 && !near_hard_ceiling {
            return passthrough(messages, before, false);
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
        let head_end = self.policy.protect_first_n.min(n);
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
        let mut out: Vec<Message> = Vec::with_capacity(n);
        out.extend_from_slice(&messages[..head_end]);
        let middle = &messages[head_end..tail_start];

        match action {
            CompactionAction::Prune => {
                // Cheap, lossless: shrink tool-result bodies, keep structure
                // (and tool_call_id) so pairing is untouched.
                for m in middle {
                    if m.role == Role::Tool {
                        let toks = counter.count(&m.content);
                        let mut pm = m.clone();
                        pm.content = format!("[pruned tool output: {toks} tokens — recoverable from log]");
                        out.push(pm);
                    } else {
                        out.push(m.clone());
                    }
                }
            }
            CompactionAction::Full => {
                // Replace the whole middle with one summary system message. The
                // full history is retained by the kernel/log (P3); this only
                // shrinks the model's view.
                let items: Vec<HistoryItem> = middle.iter().map(msg_to_item).collect();
                let summary = ExtractiveSummarizer
                    .summarize(None, &items)
                    .await
                    .unwrap_or_else(|_| "[summary unavailable]".to_string());
                out.push(Message::system(summary));
            }
            CompactionAction::None => unreachable!(),
        }

        out.extend_from_slice(&messages[tail_start..]);
        let after = count_all(&out, counter);

        // Track effectiveness: a compaction that frees <10% counts as
        // ineffective; two in a row trips the anti-thrash backoff above.
        if before.saturating_sub(after) < before / 10 {
            self.ineffective.fetch_add(1, Ordering::Relaxed);
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
        }
    }
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
        acc += counter.count(&messages[candidate].content);
        let kept = messages.len() - candidate;
        if kept >= policy.protect_last_n && acc >= tail_budget {
            break;
        }
        start = candidate;
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
}
