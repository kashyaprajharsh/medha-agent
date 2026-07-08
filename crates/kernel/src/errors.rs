//! Error taxonomy. Every failure maps to something the model can reason about
//! (Vol 3 §8). Phase 0 keeps a small set; it grows with the dispatch pipeline.

#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("provider error: {0}")]
    Provider(String),

    #[error("event log error: {0}")]
    Log(String),
}
