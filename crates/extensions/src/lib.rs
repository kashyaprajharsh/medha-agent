//! Safe discovery and activation state for local Medha extension packages.
//!
//! Discovery never executes package code. Every package starts disabled, and an
//! enabled package is pinned to its exact content hash and scope. Process
//! supervision and component adapters are separate layers built on this state.

mod compat;
pub mod doctor;
mod hook_sets;
mod hooks;
mod package;
pub mod sources;
mod state;
mod store;

pub use hook_sets::{HOOKS_FILE, PROJECT_HOOKS_ID, USER_HOOKS_ID};
pub use hooks::ProcessHookRunner;
pub use medha_extension_api::{
    ExtensionComponent, HookDecision, HookEnvelope, HookFailureMode, HookPoint, HookResult,
    Manifest, PLUGIN_ROOT_PLACEHOLDER, ProcessEntrypoint, RequestedPermissions,
};
pub use package::{Package, PackageSource};
pub use state::{Grant, Origin};
pub use store::{
    Activation, Discovery, DiscoveryError, HookSource, ListedPlugin, OperatorAction, Scope, Store,
    UpdatePlan,
};

use std::path::Path;

pub const MANIFEST_FILE: &str = "plugin.toml";

pub(crate) fn io_error(action: &str, path: &Path, error: std::io::Error) -> Error {
    Error::Io(format!("{action} {}: {error}", path.display()))
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(String),
    #[error("invalid plugin manifest: {0}")]
    Manifest(String),
    #[error("unsafe plugin package: {0}")]
    UnsafePackage(String),
    #[error("plugin state error: {0}")]
    State(String),
    #[error("plugin '{0}' is already installed or discovered")]
    AlreadyExists(String),
    #[error("plugin '{0}' was not found")]
    NotFound(String),
    #[error("plugin source changed while it was being installed; retry the install")]
    SourceChanged,
    #[error(
        "more than one package claims plugin id '{0}'; remove the collision before enabling it"
    )]
    Collision(String),
    #[error("plugin cannot be enabled because {0}")]
    Ungrantable(String),
    #[error("{0}")]
    GrantRequired(String),
    #[error("{0}")]
    NotManaged(String),
    #[error("{0}")]
    Source(String),
}

#[cfg(test)]
#[path = "test_support.rs"]
mod test_support;
