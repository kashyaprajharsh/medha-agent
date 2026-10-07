//! Human approval for policy-gated actions; headless runs deny automatically.

use async_trait::async_trait;

/// The human's answer to an approval prompt. Callers interpret `Always` in their
/// own context: a tool approval treats it as "don't ask again this session", while
/// a file-permission prompt treats it as "remember this path in this machine's
/// trust file", which is kept outside the repository.
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

/// The human's answer to a network or command-access prompt. Distinct from [`Approval`] so a
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

/// The human's answer about one path outside the workspace. Remembering the
/// folder is a wider grant than remembering the path, so it is an answer of
/// its own that only a person choosing it can give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathApproval {
    Once,
    /// Remember this path alone.
    Path,
    /// Remember the folder it is in, and so everything in that folder.
    Folder,
    Deny,
}

impl From<Approval> for PathApproval {
    fn from(approval: Approval) -> Self {
        match approval {
            Approval::Once => Self::Once,
            Approval::Always => Self::Path,
            Approval::Deny => Self::Deny,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    File,
    Directory,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    Read,
    Write,
}

/// Resolved permission target. Presentation must not infer scope from an action
/// string or from the absence of a parent-folder choice.
#[derive(Clone, Copy)]
pub struct PathRequest<'a> {
    pub action: &'a str,
    pub detail: Option<&'a str>,
    pub path: &'a std::path::Path,
    pub kind: PathKind,
    pub access: PathAccess,
    pub folder: Option<&'a std::path::Path>,
}

/// Capabilities requested together for one command. Paths are resolved by the
/// executor before review; these are grants, never a replacement workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionAccess {
    pub network: bool,
    pub read_paths: Vec<std::path::PathBuf>,
    pub write_paths: Vec<std::path::PathBuf>,
    /// Run this one command without the OS jail; never remembered, reviewed in every mode.
    pub outside_sandbox: bool,
}

impl ExecutionAccess {
    pub fn is_empty(&self) -> bool {
        !self.network
            && !self.outside_sandbox
            && self.read_paths.is_empty()
            && self.write_paths.is_empty()
    }
}

#[async_trait]
pub trait HumanGate: Send + Sync {
    /// Ask about a specific action. Trust-flow escalations must not be remembered.
    async fn confirm(&self, action: &str, detail: Option<&str>, escalated: bool) -> Approval;

    /// Why a denial happened, so the model is never told a person refused when
    /// no person was asked. Gates backed by a human keep the default.
    fn denial_reason(&self) -> &'static str {
        "rejected by human"
    }

    /// Ask whether to grant the sandbox network access and retry. Gates that do
    /// not override this map their [`confirm`](Self::confirm) answer: a plain
    /// `Once` stays once, `Always` becomes a durable grant, `Deny` denies.
    async fn confirm_network(&self, detail: Option<&str>, escalated: bool) -> NetworkDecision {
        match self
            .confirm("grant network access and retry", detail, escalated)
            .await
        {
            Approval::Once => NetworkDecision::Once,
            Approval::Always => NetworkDecision::Persistent,
            Approval::Deny => NetworkDecision::Deny,
        }
    }

    /// One review for a command and its missing network/filesystem capabilities.
    async fn confirm_access(&self, detail: Option<&str>, escalated: bool) -> NetworkDecision {
        // Some plain/editor gates remember Standard approvals by action. Include
        // the reviewed request so remembering one card cannot approve new roots.
        let action = format!("command access: {}", detail.unwrap_or_default());
        match self.confirm(&action, detail, escalated).await {
            Approval::Once => NetworkDecision::Once,
            Approval::Always if !escalated => NetworkDecision::Persistent,
            Approval::Always => NetworkDecision::Once,
            Approval::Deny => NetworkDecision::Deny,
        }
    }

    /// Ask about one path outside the workspace. `request.folder` is the folder that
    /// may be remembered in its place, when there is one fit to offer. A gate
    /// that does not override this never answers with the folder: its
    /// `Always` remembers the path alone, as it did before there was a choice.
    async fn confirm_path(&self, request: PathRequest<'_>) -> PathApproval {
        self.confirm(request.action, request.detail, false)
            .await
            .into()
    }
}

tokio::task_local! {
    static NETWORK_ONCE: bool;
    static EXECUTION_ACCESS: ExecutionAccess;
}

pub async fn execution_access_scope<F: std::future::Future>(
    access: ExecutionAccess,
    fut: F,
) -> F::Output {
    let network = access.network || network_once_active();
    EXECUTION_ACCESS
        .scope(access, NETWORK_ONCE.scope(network, fut))
        .await
}

pub fn execution_access() -> ExecutionAccess {
    EXECUTION_ACCESS.try_with(Clone::clone).unwrap_or_default()
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

    fn denial_reason(&self) -> &'static str {
        "this action needs approval and the run is headless, so no one could be asked"
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

    #[tokio::test]
    async fn execution_access_is_isolated_across_concurrent_futures() {
        let access = ExecutionAccess {
            network: true,
            read_paths: vec![std::path::PathBuf::from("/approved")],
            ..Default::default()
        };
        tokio::join!(
            execution_access_scope(access.clone(), async {
                tokio::task::yield_now().await;
                assert_eq!(execution_access(), access);
                assert!(network_once_active());
            }),
            async {
                tokio::task::yield_now().await;
                assert!(execution_access().is_empty());
                assert!(!network_once_active());
            }
        );
        assert!(execution_access().is_empty());
        assert!(!network_once_active());
    }

    #[tokio::test]
    async fn escalated_access_never_defaults_to_a_remembered_grant() {
        assert_eq!(
            Fixed(Approval::Always)
                .confirm_access(Some("network"), true)
                .await,
            NetworkDecision::Once
        );
        assert_eq!(
            Fixed(Approval::Always)
                .confirm_access(Some("network"), false)
                .await,
            NetworkDecision::Persistent
        );
    }
}
