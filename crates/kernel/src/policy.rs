//! Deny-first authorization interface for validated tool intents.

use crate::types::{AutonomyLevel, BlastRadius, Decision, ToolIntent};

pub trait Policy: Send + Sync {
    /// Authorize an intent. `None` radius is unregistered; autonomy never
    /// weakens a base `Human` or `Deny` decision.
    fn authorize(
        &self,
        autonomy: AutonomyLevel,
        intent: &ToolIntent,
        blast_radius: Option<BlastRadius>,
    ) -> Decision;
}

/// Permissive policy (tests / explicit opt-out). The real default is deny-first
/// (see the `policy` crate).
pub struct AllowAll;
impl Policy for AllowAll {
    fn authorize(
        &self,
        _autonomy: AutonomyLevel,
        _intent: &ToolIntent,
        _blast_radius: Option<BlastRadius>,
    ) -> Decision {
        Decision::Allow
    }
}
