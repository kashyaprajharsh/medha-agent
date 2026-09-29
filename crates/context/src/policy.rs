//! Thresholds for prune-only and full compaction passes.

#[derive(Debug, Clone)]
pub struct CompactionPolicy {
    /// At/above this fraction of usable tokens, run full compaction (prune + summarize).
    pub trigger_ratio: f32,
    /// At/above this (but below trigger), run a cheap prune-only pass — the
    /// graduated step that defers expensive summarization.
    pub microcompact_ratio: f32,
    /// Maximum fraction of usable tokens retained at the tail; also bounded
    /// by the scaled recent-history budget.
    pub tail_ratio: f32,
    /// Head messages always preserved (system prompt + first exchange).
    pub protect_first_n: usize,
    /// Target number of most-recent messages, subject to the token ceiling.
    /// The newest item and complete tool/result pairs always survive.
    pub protect_last_n: usize,
    /// Only bother pruning tool outputs if they exceed this many tokens.
    /// `None` (default) scales with the window — see [`Self::prune_floor`]:
    /// an absolute constant can't fit both an 8k local model and a 200k
    /// hosted one (every other threshold here is a ratio for the same reason).
    pub prune_min_tool_tokens: Option<u32>,
    /// Hard ceiling as a fraction of the true model window, above the soft
    /// trigger. Crossing it forces compaction through the anti-thrash backoff;
    /// still over afterwards, the kernel refuses to send.
    pub emergency_ratio: f32,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            trigger_ratio: 0.90,
            microcompact_ratio: 0.60,
            tail_ratio: 0.20,
            protect_first_n: 3,
            protect_last_n: 20,
            prune_min_tool_tokens: None,
            emergency_ratio: 0.98,
        }
    }
}

impl CompactionPolicy {
    /// Token ceiling for recent exchanges; independent of message sizes.
    pub(crate) fn tail_budget(&self, usable: u32) -> u32 {
        let ceiling = (usable as f32 * self.tail_ratio) as u32;
        ceiling.min((usable / 40).clamp(10_000, 25_000))
    }

    /// Effective prune floor for a given usable window. Configured value wins
    /// verbatim; the auto default is 1% of usable, clamped to ≥200 tokens so
    /// tiny windows don't churn outputs barely bigger than the ~30-token
    /// placeholder that replaces them. (~1,000 on a 128k model, 200 on 8k.)
    pub fn prune_floor(&self, usable: u32) -> u32 {
        self.prune_min_tool_tokens
            .unwrap_or_else(|| ((usable as f32 * 0.01) as u32).max(200))
    }
}

/// What action the policy selects for the current pressure level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionAction {
    /// Plenty of room — do nothing.
    None,
    /// Cheap, deterministic, no LLM: prune stale tool outputs.
    Prune,
    /// Prune then summarize the middle with the compressor model.
    Full,
}
