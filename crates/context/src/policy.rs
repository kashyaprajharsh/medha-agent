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
    pub prune_min_tool_tokens: u32,
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
            trigger_ratio: 0.85,
            microcompact_ratio: 0.60,
            tail_ratio: 0.20,
            protect_first_n: 3,
            protect_last_n: 20,
            prune_min_tool_tokens: 8_000,
            emergency_ratio: 0.95,
        }
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
