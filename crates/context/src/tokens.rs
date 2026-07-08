//! Token counting. The compactor needs to estimate how full the window is.
//!
//! The `TokenCounter` trait is the swap point (P8). Production uses a real BPE
//! tokenizer ([`BpeCounter`]); the heuristic remains as a zero-dependency
//! fallback and for deterministic tests. Note the *authoritative* count for the
//! running model still comes from the provider's reported `usage` — the counter
//! governs the pre-flight estimate (before the first response) and the per-item
//! compaction boundaries, where good relative accuracy matters most.

pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str) -> u32;
}

/// ~4 characters per token (English-ish). Deliberately slightly conservative
/// (rounds up) so we trigger compaction a little early rather than overflow.
/// Kept as a dependency-free fallback and for tests that need exact arithmetic.
pub struct HeuristicCounter;

impl TokenCounter for HeuristicCounter {
    fn count(&self, text: &str) -> u32 {
        (text.chars().count() as u32).div_ceil(4)
    }
}

/// A real BPE tokenizer (OpenAI's `o200k_base`). MEDHA is provider-agnostic, so
/// this is a strong *estimate* rather than an exact per-model count: for a
/// Llama/Nemotron-family model the true tokenizer differs, but a real BPE is far
/// closer than chars/4 (especially for code), and exact counts still arrive via
/// the provider's `usage`. Fully offline — the vocabulary is embedded in the
/// binary at compile time, so there is no network call or file to ship.
pub struct BpeCounter {
    bpe: tiktoken_rs::CoreBPE,
}

impl BpeCounter {
    /// Build the `o200k_base` counter. The vocab is embedded, so this is
    /// effectively infallible at runtime; a load failure is a build/packaging
    /// bug, not a recoverable condition.
    pub fn o200k() -> Self {
        Self { bpe: tiktoken_rs::o200k_base().expect("embedded o200k_base vocabulary loads") }
    }
}

impl TokenCounter for BpeCounter {
    fn count(&self, text: &str) -> u32 {
        // `encode_ordinary` treats the input as plain content (no special-token
        // parsing), so text/code containing "<|…|>" is counted literally rather
        // than collapsing to a single special token.
        self.bpe.encode_ordinary(text).len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_up_and_handles_empty() {
        let c = HeuristicCounter;
        assert_eq!(c.count(""), 0);
        assert_eq!(c.count("abcd"), 1);
        assert_eq!(c.count("abcde"), 2); // ceil(5/4)
    }

    #[test]
    fn bpe_counts_are_sane_and_deterministic() {
        let c = BpeCounter::o200k();
        assert_eq!(c.count(""), 0);
        // A short English phrase: a handful of tokens, and stable across calls.
        let phrase = "The quick brown fox jumps over the lazy dog";
        let n = c.count(phrase);
        assert!((5..=15).contains(&n), "expected a few tokens, got {n}");
        assert_eq!(n, c.count(phrase), "counting must be deterministic");
        // A real tokenizer counts fewer tokens than raw characters for prose.
        assert!(n < phrase.chars().count() as u32);
    }
}
