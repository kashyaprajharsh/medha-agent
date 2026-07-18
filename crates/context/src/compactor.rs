//! The two-phase compactor (§4.3). Phase 1 = deterministic prune (no LLM);
//! Phase 2 = LLM summarization of the middle, preserving a protected head and
//! tail. The defining properties:
//!
//!   * **Lossless:** pruning never destroys data — the full output stays
//!     addressable by `artifact` hash, so the model can re-fetch it. Compaction
//!     shrinks the *live window*, not the truth (P3).
//!   * **Lineage:** every summary carries the `source_events` it covers (§4.3),
//!     so a summary line can be traced back to the exact events.
//!   * **Iterative re-summary:** a previous summary is passed in and *updated*,
//!     not restarted, so detail accretes coherently across repeated compactions.
//!   * **Offline fallback:** an extractive, no-LLM summarizer keeps compaction
//!     working when the routed compressor model is unavailable or unreliable —
//!     important when running entirely on local open-weight models.

use crate::budget::ContextBudget;
use crate::policy::{CompactionAction, CompactionPolicy};
use crate::tokens::TokenCounter;
use async_trait::async_trait;
use kernel::Role;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    Text,
    ToolOutput,
    Summary,
}

/// One unit of conversation history the compactor operates over. Mapped from
/// the event log in the full context compiler; standalone here for testability.
#[derive(Debug, Clone)]
pub struct HistoryItem {
    pub role: Role,
    pub content: String,
    pub kind: ItemKind,
    /// ULIDs of the events this item derives from — lineage pointers (§4.3).
    pub source_events: Vec<String>,
    /// Content-addressed hash of the full payload, if it spilled to the blob
    /// store. Lets pruning be lossless: the full output is re-fetchable.
    pub artifact: Option<String>,
    /// Pinned spans are never pruned or summarized (§16).
    pub pinned: bool,
    /// Set once this item's content has been replaced by a prune placeholder.
    pub pruned: bool,
}

impl HistoryItem {
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            kind: ItemKind::Text,
            source_events: Vec::new(),
            artifact: None,
            pinned: false,
            pruned: false,
        }
    }

    pub fn tool_output(content: impl Into<String>, artifact: Option<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            kind: ItemKind::ToolOutput,
            source_events: Vec::new(),
            artifact,
            pinned: false,
            pruned: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub items: Vec<HistoryItem>,
    pub action: CompactionAction,
    pub pruned: usize,
    pub summarized: usize,
    pub before_tokens: u32,
    pub after_tokens: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum SummarizeError {
    #[error("summarizer unavailable: {0}")]
    Unavailable(String),
}

/// Pluggable summarizer (P8). The LLM impl routes to the `compressor` model
/// (§4.4); `ExtractiveSummarizer` is the deterministic offline fallback.
#[async_trait]
pub trait Summarizer: Send + Sync {
    /// Summarize `items`, optionally *updating* a previous summary rather than
    /// restarting it (iterative re-summarization).
    async fn summarize(
        &self,
        previous: Option<&str>,
        items: &[HistoryItem],
    ) -> Result<String, SummarizeError>;
}

// The LLM summarizer's instruction template is no longer a code literal: it is
// resolved from the prompt registry (`crate::prompts::compaction_summary()`),
// so it can be overridden and eval-gated as a versioned artifact (§4.11).

pub fn total_tokens(items: &[HistoryItem], counter: &dyn TokenCounter) -> u32 {
    items.iter().map(|i| counter.count(&i.content)).sum()
}

/// Select an action for the current pressure level (graduated escalation).
pub fn decide(
    items: &[HistoryItem],
    budget: &ContextBudget,
    policy: &CompactionPolicy,
    counter: &dyn TokenCounter,
) -> CompactionAction {
    let usable = budget.usable().max(1) as f32;
    let ratio = total_tokens(items, counter) as f32 / usable;
    if ratio >= policy.trigger_ratio {
        CompactionAction::Full
    } else if ratio >= policy.microcompact_ratio {
        CompactionAction::Prune
    } else {
        CompactionAction::None
    }
}

/// Run compaction according to policy. Protects a head (first N) and a tail
/// (most-recent, by token budget with a floor); only the middle is touched.
pub async fn compact(
    items: Vec<HistoryItem>,
    budget: &ContextBudget,
    policy: &CompactionPolicy,
    counter: &dyn TokenCounter,
    summarizer: &dyn Summarizer,
    previous_summary: Option<&str>,
) -> Result<CompactionResult, SummarizeError> {
    let before_tokens = total_tokens(&items, counter);
    let action = decide(&items, budget, policy, counter);
    if action == CompactionAction::None {
        return Ok(CompactionResult {
            items,
            action,
            pruned: 0,
            summarized: 0,
            before_tokens,
            after_tokens: before_tokens,
        });
    }

    let n = items.len();
    let head_end = policy.protect_first_n.min(n);
    let tail_start = tail_start_index(&items, head_end, budget, policy, counter);

    // Split into protected head/tail and the compactable middle.
    let mut head: Vec<HistoryItem> = items[..head_end].to_vec();
    let tail: Vec<HistoryItem> = items[tail_start..].to_vec();
    let middle: Vec<HistoryItem> = items[head_end..tail_start].to_vec();

    // Pinned middle items are never compacted; keep them verbatim.
    let (pinned_middle, compactable): (Vec<_>, Vec<_>) =
        middle.into_iter().partition(|i| i.pinned);

    // Phase 1 — prune tool outputs (deterministic, lossless).
    let mut compactable = compactable;
    let mut pruned = 0;
    let tool_tokens: u32 = compactable
        .iter()
        .filter(|i| i.kind == ItemKind::ToolOutput && !i.pruned)
        .map(|i| counter.count(&i.content))
        .sum();
    if tool_tokens >= policy.prune_floor(budget.usable()) {
        for item in compactable.iter_mut() {
            if item.kind == ItemKind::ToolOutput && !item.pruned {
                let toks = counter.count(&item.content);
                let artifact = item.artifact.clone().unwrap_or_else(|| "—".into());
                item.content = format!("[pruned tool output: {toks} tokens, artifact {artifact}]");
                item.pruned = true;
                pruned += 1;
            }
        }
    }

    // Phase 2 — summarize the compactable middle (only on Full).
    let (mut new_middle, summarized) = if action == CompactionAction::Full && !compactable.is_empty()
    {
        let summary_text = summarizer.summarize(previous_summary, &compactable).await?;
        let source_events: Vec<String> =
            compactable.iter().flat_map(|i| i.source_events.clone()).collect();
        // Assistant, not system: the summary sits mid-array (after the head),
        // and strict providers (vLLM) reject a `system` message that isn't
        // first. Same invariant as the live engine's summary.
        let summary = HistoryItem {
            role: Role::Assistant,
            content: summary_text,
            kind: ItemKind::Summary,
            source_events,
            artifact: None,
            pinned: false,
            pruned: false,
        };
        (vec![summary], compactable.len())
    } else {
        (compactable, 0)
    };

    // Reassemble: head + [summary | pruned middle] + pinned middle + tail.
    // (Pinned items follow the summary for Phase 1; chronological re-weave of
    // pinned spans is a Phase 2 refinement.)
    let mut out = Vec::with_capacity(head.len() + new_middle.len() + pinned_middle.len() + tail.len());
    out.append(&mut head);
    out.append(&mut new_middle);
    out.extend(pinned_middle);
    out.extend(tail);

    let after_tokens = total_tokens(&out, counter);
    Ok(CompactionResult { items: out, action, pruned, summarized, before_tokens, after_tokens })
}

/// Walk back from the end, keeping items until the tail token budget is met,
/// but never fewer than `protect_last_n` and never crossing into the head.
fn tail_start_index(
    items: &[HistoryItem],
    head_end: usize,
    budget: &ContextBudget,
    policy: &CompactionPolicy,
    counter: &dyn TokenCounter,
) -> usize {
    let tail_budget = (budget.usable() as f32 * policy.tail_ratio) as u32;
    let mut acc = 0u32;
    let mut start = items.len();
    while start > head_end {
        let candidate = start - 1;
        acc += counter.count(&items[candidate].content);
        let kept = items.len() - candidate;
        // Stop growing the tail once we've met both the count floor and the
        // token budget.
        if kept >= policy.protect_last_n && acc >= tail_budget {
            break;
        }
        start = candidate;
    }
    start.max(head_end)
}

/// LLM summarizer: routes the middle through a model using the versioned
/// `compaction_summary` template. On any failure it returns
/// `SummarizeError::Unavailable`, so the engine falls back to the extractive
/// summarizer — a keyword scrape is far better than an empty summary that
/// invites hallucination.
pub struct LlmSummarizer<P: kernel::Provider> {
    provider: std::sync::Arc<P>,
}

impl<P: kernel::Provider> LlmSummarizer<P> {
    pub fn new(provider: std::sync::Arc<P>) -> Self {
        Self { provider }
    }
}

fn role_label(role: &Role) -> &'static str {
    match role {
        Role::User => "USER",
        Role::Assistant => "ASSISTANT",
        Role::Tool => "TOOL",
        Role::System => "SYSTEM",
    }
}

#[async_trait]
impl<P: kernel::Provider + 'static> Summarizer for LlmSummarizer<P> {
    async fn summarize(
        &self,
        previous: Option<&str>,
        items: &[HistoryItem],
    ) -> Result<String, SummarizeError> {
        use futures::StreamExt;
        use kernel::{Block, CompiledContext, Message};

        let mut body = String::new();
        if let Some(prev) = previous {
            body.push_str("=== previous summary (update it) ===\n");
            body.push_str(prev);
            body.push_str("\n=== conversation to fold in ===\n");
        }
        for it in items {
            body.push_str(role_label(&it.role));
            body.push_str(": ");
            body.push_str(&it.content);
            body.push('\n');
        }

        let ctx = CompiledContext {
            model: String::new(),
            messages: vec![Message::system(crate::prompts::compaction_summary()), Message::user(body)],
            tools: Vec::new(),
        };
        let mut stream = self
            .provider
            .stream(&ctx)
            .await
            .map_err(|e| SummarizeError::Unavailable(e.to_string()))?;
        let mut text = String::new();
        while let Some(block) = stream.next().await {
            match block {
                Ok(Block::Text(t)) => text.push_str(&t),
                Ok(_) => {} // ignore reasoning/tool/usage blocks
                Err(e) => return Err(SummarizeError::Unavailable(e.to_string())),
            }
        }
        if text.trim().is_empty() {
            return Err(SummarizeError::Unavailable("model returned empty summary".into()));
        }
        Ok(text)
    }
}

/// Deterministic, no-LLM fallback. Lossy but honest and offline-capable; the
/// full detail remains recoverable via each item's `source_events`/`artifact`.
pub struct ExtractiveSummarizer;

#[async_trait]
impl Summarizer for ExtractiveSummarizer {
    async fn summarize(
        &self,
        previous: Option<&str>,
        items: &[HistoryItem],
    ) -> Result<String, SummarizeError> {
        let mut out = String::from("[MEDHA extractive summary — deterministic fallback, no LLM]\n");
        if let Some(prev) = previous {
            out.push_str("Previous summary:\n");
            out.push_str(prev);
            out.push_str("\n---\n");
        }
        out.push_str(&format!("Summarized {} items. Full detail recoverable via event lineage.\n", items.len()));

        let user_gists: Vec<String> = items
            .iter()
            .filter(|i| i.role == Role::User)
            .map(|i| {
                let g: String = i.content.chars().take(160).collect();
                format!("- {}", g.trim())
            })
            .collect();
        if !user_gists.is_empty() {
            out.push_str("User asks:\n");
            out.push_str(&user_gists.join("\n"));
            out.push('\n');
        }

        let mut files: Vec<&str> = items
            .iter()
            .flat_map(|i| i.content.split_whitespace())
            .filter(|t| t.contains('/') && t.contains('.') && !t.contains("://"))
            .collect();
        files.sort_unstable();
        files.dedup();
        if !files.is_empty() {
            out.push_str("Files mentioned: ");
            out.push_str(&files.join(", "));
            out.push('\n');
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::HeuristicCounter;

    fn big(role: Role, n: usize) -> HistoryItem {
        HistoryItem::text(role, "x".repeat(n))
    }

    #[tokio::test]
    async fn no_action_when_under_pressure() {
        let items = vec![big(Role::User, 40), big(Role::Assistant, 40)];
        let budget = ContextBudget::from_max_ctx(32_768);
        let policy = CompactionPolicy::default();
        let counter = HeuristicCounter;
        assert_eq!(decide(&items, &budget, &policy, &counter), CompactionAction::None);
    }

    #[tokio::test]
    async fn full_compaction_shrinks_window_and_protects_ends() {
        // Tiny window so we cross the trigger easily.
        let budget = ContextBudget::from_max_ctx(2_000); // usable = 2000-500-200=1300
        let policy = CompactionPolicy {
            protect_first_n: 1,
            protect_last_n: 1,
            tail_ratio: 0.1,
            prune_min_tool_tokens: Some(10),
            ..Default::default()
        };
        let counter = HeuristicCounter;

        let mut items = vec![HistoryItem::text(Role::System, "SYSTEM PROMPT")];
        for i in 0..10 {
            items.push(HistoryItem::text(Role::User, format!("question {i} {}", "y".repeat(400))));
            items.push(HistoryItem::tool_output("z".repeat(800), Some("sha256:abc".into())));
        }
        items.push(HistoryItem::text(Role::User, "LAST MESSAGE"));

        let before = total_tokens(&items, &counter);
        let res = compact(items, &budget, &policy, &counter, &ExtractiveSummarizer, None)
            .await
            .unwrap();

        assert_eq!(res.action, CompactionAction::Full);
        assert!(res.after_tokens < before, "should shrink");
        assert!(res.pruned > 0, "tool outputs should be pruned");
        assert!(res.summarized > 0, "middle should be summarized");
        // Head and tail survive verbatim.
        assert_eq!(res.items.first().unwrap().content, "SYSTEM PROMPT");
        assert_eq!(res.items.last().unwrap().content, "LAST MESSAGE");
        // A summary item exists and carries the fallback marker.
        assert!(res.items.iter().any(|i| i.kind == ItemKind::Summary));
    }
}
