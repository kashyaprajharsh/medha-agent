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
