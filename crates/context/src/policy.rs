//! Compaction policy — the thresholds that decide *when* and *how hard* to
//! compact. Compaction is *graduated*: a cheap prune-only pass relieves
//! moderate pressure before the expensive summarize pass is needed near the
//! limit. Defaults are conservative starting points, kept here so they can be
//! tuned and, later, eval-gated as part of `medha.lock` (§4.3, P4) — compaction
//! settings are themselves an evolvable artifact, not magic constants.

#[derive(Debug, Clone)]
pub struct CompactionPolicy {
    /// At/above this fraction of usable tokens, run full compaction (prune + summarize).
    pub trigger_ratio: f32,
    /// At/above this (but below trigger), run a cheap prune-only pass — the
    /// graduated step that defers expensive summarization.
    pub microcompact_ratio: f32,
    /// Fraction of usable tokens kept verbatim at the tail (most recent turns).
    pub tail_ratio: f32,
    /// Head messages always preserved (system prompt + first exchange).
    pub protect_first_n: usize,
    /// Floor on the number of most-recent messages kept verbatim.
    pub protect_last_n: usize,
    /// Only bother pruning tool outputs if they exceed this many tokens.
    /// `None` (default) scales with the window — see [`Self::prune_floor`]:
    /// an absolute constant can't fit both an 8k local model and a 200k
    /// hosted one (every other threshold here is a ratio for the same reason).
    pub prune_min_tool_tokens: Option<u32>,
    /// Hard safety ceiling, as a fraction of the *true* model window (not the
    /// reduced usable budget). A second, independent layer above the normal
    /// trigger — the pattern real harnesses use two thresholds for: a soft
    /// trigger that compacts early and gracefully, and a hard ceiling that is
    /// the last line of defense if the soft pass couldn't find enough to cut
    /// (e.g. one huge single turn). Crossing this forces compaction even
    /// through the anti-thrash backoff, and if still over afterward, the
    /// kernel refuses to send rather than risk an API context-length error.
    pub emergency_ratio: f32,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            trigger_ratio: 0.99,
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
