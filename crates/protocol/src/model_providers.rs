//! Secret-free catalogue and explicit deployment/setup commands. Provider
//! metadata never grants permission to reuse a credential or move an endpoint.

use crate::{Command, DiscoveredModel, ModelDraft, ModelProtocol, ScopeKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelAuth {
    None,
    Bearer,
    XApiKey,
    XGoogApiKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProvider {
    pub id: String,
    pub name: String,
    pub url: Option<String>,
    pub parameters: Vec<String>,
    pub protocol: Option<ModelProtocol>,
    pub auth: ModelAuth,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub doc: Option<String>,
    pub env: Vec<String>,
    pub models: Vec<DiscoveredModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCatalogue {
    pub revision: String,
    pub providers: Vec<ModelProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadProviders {}
impl Command for ReadProviders {
    const METHOD: &'static str = "application.providers";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ProviderCatalogue;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshProviders {}
impl Command for RefreshProviders {
    const METHOD: &'static str = "application.providers.refresh";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ProviderCatalogue;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveDeployment {
    pub provider_id: String,
    pub parameters: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDeployment {
    pub provider_id: String,
    pub protocol: ModelProtocol,
    pub auth: ModelAuth,
    pub base_url: String,
}
impl Command for ResolveDeployment {
    const METHOD: &'static str = "application.providers.resolve";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ModelDeployment;
}

/// The user explicitly requests synthetic text, streaming and function/result
/// checks. No workspace text or real tool implementation is sent/executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestModel {
    pub draft: ModelDraft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTestStatus {
    Passed,
    Failed,
    Skipped,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelTestCheck {
    pub status: ModelTestStatus,
    pub message: String,
    pub failure: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelTestResult {
    pub text: ModelTestCheck,
    pub streaming: ModelTestCheck,
    pub tools: ModelTestCheck,
}
impl Command for TestModel {
    const METHOD: &'static str = "application.models.test";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ModelTestResult;
}

/// Read a key's opening letters before it is used at `base_url`; nothing is sent or kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckModelKey {
    pub base_url: String,
    pub key: crate::Secret,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelKeyCheck {
    /// Why the key would be held back: it looks like another provider's.
    pub held_back: Option<String>,
    /// Shown, but the key may still be used (an organisation admin key).
    pub warning: Option<String>,
}
impl Command for CheckModelKey {
    const METHOD: &'static str = "application.models.check_key";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ModelKeyCheck;
}
