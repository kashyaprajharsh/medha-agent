//! Context compiler & compaction (§4.3). Phase 1 ships the budget-aware,
//! two-phase compactor; the full five-sheath compiler and the six-stage
//! pipeline build on these primitives. `ContextEngine` is the swap point (P8).

pub mod budget;
pub mod compactor;
pub mod ctxfiles;
pub mod engine;
pub mod identity;
pub mod policy;
pub mod prompts;
pub mod tokens;

pub use budget::ContextBudget;
pub use compactor::{
    CompactionResult, ExtractiveSummarizer, HistoryItem, ItemKind, LlmSummarizer, SummarizeError,
    Summarizer, compact, decide, total_tokens,
};
pub use engine::PipelineEngine;
pub use policy::{CompactionAction, CompactionPolicy};
pub use tokens::{BpeCounter, HeuristicCounter, TokenCounter};
