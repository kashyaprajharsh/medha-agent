//! Human approval for policy-gated actions; headless runs deny automatically.

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

/// The human's answer to a network-grant prompt. Distinct from [`Approval`] so a
/// third "session" tier does not have to be forced onto every other card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkDecision {
    /// Open the network for this one retried command only.
    Once,
    /// Open the network for the rest of this process run (in memory only).
    Session,
    /// Open the network and remember it in the machine-local trust file.
    Persistent,
    /// Reject.
    Deny,
}

#[async_trait]
pub trait HumanGate: Send + Sync {
    /// Ask about a specific action. Trust-flow escalations must not be remembered.
    async fn confirm(&self, action: &str, detail: Option<&str>, escalated: bool) -> Approval;

    /// Ask whether to grant the sandbox network access and retry. Gates that do
    /// not override this map their [`confirm`](Self::confirm) answer: a plain
    /// `Once` stays once, `Always` becomes a durable grant, `Deny` denies.
    async fn confirm_network(&self, detail: Option<&str>, escalated: bool) -> NetworkDecision {
        match self.confirm("grant network access and retry", detail, escalated).await {
            Approval::Once => NetworkDecision::Once,
            Approval::Always => NetworkDecision::Persistent,
            Approval::Deny => NetworkDecision::Deny,
        }
    }
}

tokio::task_local! {
    static NETWORK_ONCE: bool;
}

/// Run `fut` with a one-shot network grant in scope. The value is visible to any
/// synchronous `build_command` polled inside `fut` — including across the tool
/// boundary, which carries no intent id — and is isolated per future, so it never
/// leaks to a concurrently dispatched intent.
pub async fn network_once_scope<F: std::future::Future>(fut: F) -> F::Output {
    NETWORK_ONCE.scope(true, fut).await
}

/// True when the current task is inside a [`network_once_scope`].
pub fn network_once_active() -> bool {
    NETWORK_ONCE.try_with(|granted| *granted).unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Approval);

    #[async_trait]
    impl HumanGate for Fixed {
        async fn confirm(&self, _a: &str, _d: Option<&str>, _e: bool) -> Approval {
            self.0
        }
    }

    #[tokio::test]
    async fn confirm_network_default_maps_confirm_answer() {
        assert_eq!(
            Fixed(Approval::Once).confirm_network(None, false).await,
            NetworkDecision::Once
        );
        // A gate that does not opt in treats "always" as a durable grant.
        assert_eq!(
            Fixed(Approval::Always).confirm_network(None, false).await,
            NetworkDecision::Persistent
        );
        assert_eq!(
            Fixed(Approval::Deny).confirm_network(None, false).await,
            NetworkDecision::Deny
        );
        // Headless auto-deny denies the grant with no code of its own.
        assert_eq!(
            AutoDeny.confirm_network(None, true).await,
            NetworkDecision::Deny
        );
    }

    #[tokio::test]
    async fn network_once_scope_is_isolated_and_defaults_off() {
        assert!(!network_once_active());
        network_once_scope(async {
            assert!(network_once_active());
        })
        .await;
        assert!(!network_once_active());
    }
}
