//! Budget-aware context compilation and compaction.

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
