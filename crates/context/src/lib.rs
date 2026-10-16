//! Budget-aware context compilation and compaction.

pub mod budget;
pub mod compactor;
pub mod ctxfiles;
pub mod engine;
mod handoff;
pub mod identity;
pub mod policy;
pub mod prompts;
mod prune_batch;
pub mod tokens;
pub mod workspace;

pub use budget::ContextBudget;
pub use compactor::{
    CompactionResult, ExtractiveSummarizer, HistoryItem, ItemKind, LlmSummarizer, SummarizeError,
    Summarizer, compact, decide, total_tokens,
};
pub use engine::PipelineEngine;
pub use policy::{CompactionAction, CompactionPolicy};
pub use tokens::{BpeCounter, HeuristicCounter, TokenCounter};
