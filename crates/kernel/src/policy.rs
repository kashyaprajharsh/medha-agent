//! The policy interface (§4.6). Deny-first authorization: every validated tool
//! intent passes through `authorize` before execution. The kernel knows only
//! this trait; the declarative rules + command scanner live in the policy crate
//! (P8). Returns a `Decision` (allow / deny / verify / human).

use crate::types::{BlastRadius, Decision, ToolIntent};

pub trait Policy: Send + Sync {
    /// Authorize an intent. `blast_radius` is the tool's declared radius (§4.7),
    /// looked up by the kernel from the executor — `None` means the tool isn't
    /// registered (deny-first). The policy decides from the radius plus any
    /// tool-specific rules (e.g. a shell scanner).
    fn authorize(&self, intent: &ToolIntent, blast_radius: Option<BlastRadius>) -> Decision;
}

/// Permissive policy (tests / explicit opt-out). The real default is deny-first
/// (see the `policy` crate).
pub struct AllowAll;
impl Policy for AllowAll {
    fn authorize(&self, _intent: &ToolIntent, _blast_radius: Option<BlastRadius>) -> Decision {
        Decision::Allow
    }
}
