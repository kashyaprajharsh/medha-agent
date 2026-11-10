//! What a finished turn used and cost, told when it ends.

use serde::{Deserialize, Serialize};

/// Where a turn's cost comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    /// The operator's own rate, from `[pricing]`.
    Own,
    /// The model's list price: indicative only.
    List,
    /// Some calls had a price and some did not; the cost covers the priced ones.
    Partly,
    /// No call had a price.
    None,
}

/// The model calls a turn made, its sub-agents' included, as the log recorded them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnSummary {
    pub seconds: f64,
    pub calls: u32,
    /// Prompt tokens, cached ones included.
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub priced: CostSource,
}
