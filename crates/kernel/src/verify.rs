//! Deterministic verifier (§4.7). After a turn modifies files, the kernel runs
//! a configured check (tests / lint / typecheck) and feeds the result back to
//! the model, so a broken build is caught and self-corrected within the loop —
//! "verify before commit" applied post-hoc to reversible-local edits (P5/P2).
//! Deterministic checks come first (cheap, exact); the adversarial model-review
//! verifier layers on top later.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub ok: bool,
    pub summary: String,
    pub output: String,
}

#[async_trait]
pub trait Verifier: Send + Sync {
    /// Run checks after file-modifying tools ran this turn. `None` = nothing
    /// configured (skip silently). Implementations must observe `cancel`: this
    /// runs inside the interactive turn, so Esc cannot wait out a build timeout.
    async fn check(&self, cancel: &CancellationToken) -> Option<VerifyReport>;
}

/// No verifier configured.
pub struct NoVerify;

#[async_trait]
impl Verifier for NoVerify {
    async fn check(&self, _cancel: &CancellationToken) -> Option<VerifyReport> {
        None
    }
}
