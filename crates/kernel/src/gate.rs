//! Human gate (§4.7, the verifier layer's interactive verifier). When policy
//! returns `Decision::Human`, the kernel asks the human to approve the action
//! before it commits (draft → approve → commit, P5). The kernel knows only this
//! trait; the surface provides the UI (a terminal y/N prompt, later an approval
//! card). Headless runs use `AutoDeny` — no human, no approval.

use async_trait::async_trait;

/// The human's answer to an approval prompt. Callers interpret `Always` in their
/// own context: a tool approval treats it as "don't ask again this session", while
/// a file-permission prompt treats it as "persist this path to medha.lock".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Allow this one operation; do not remember the decision.
    Once,
    /// Allow and remember (session auto-approve, or persisted trust for paths).
    Always,
    /// Reject.
    Deny,
}

impl Approval {
    /// True if the operation may proceed (`Once` or `Always`).
    pub fn approved(self) -> bool {
        matches!(self, Approval::Once | Approval::Always)
    }
}

#[async_trait]
pub trait HumanGate: Send + Sync {
    /// Ask the human to approve `action`; `detail` is a preview (command/diff).
    /// `action` doubles as the auto-approve scope key, so it should identify the
    /// *specific* action (e.g. "shell.exec: cargo build"), not just the tool —
    /// otherwise "always allow" blanket-approves every call of that tool (K9).
    /// `escalated` is true when this gate exists only because of a trust-flow
    /// escalation (a web-tainted consequential action, §4.6); such prompts must
    /// NEVER be remembered/auto-approved — each one is asked afresh.
    async fn confirm(&self, action: &str, detail: Option<&str>, escalated: bool) -> Approval;
}

/// No human available (headless / non-interactive): reject anything that needs
/// approval rather than silently proceeding.
pub struct AutoDeny;

#[async_trait]
impl HumanGate for AutoDeny {
    async fn confirm(&self, _action: &str, _detail: Option<&str>, _escalated: bool) -> Approval {
        Approval::Deny
    }
}
