//! Typed application resources used by any local frontend. These records
//! contain display data and explicit user choices, never service handles.

use super::{Command, ScopeKind};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(pub String);
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelProtocol {
    OpenAiChat,
    GeminiInteractions,
    AnthropicMessages,
    OpenAiResponses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchProvider {
    #[default]
    Duckduckgo,
    Tavily,
    Brave,
    Searxng,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfile {
    pub name: String,
    pub model: String,
    pub protocol: ModelProtocol,
    pub base_url: String,
    pub context_limit: Option<u32>,
    pub default: bool,
    pub key_present: bool,
    pub requires_key: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogue {
    pub profiles: Vec<ModelProfile>,
    pub search: SearchProvider,
    pub searxng_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDraft {
    pub name: Option<String>,
    pub protocol: ModelProtocol,
    pub base_url: String,
    pub model: String,
    pub context_limit: Option<u32>,
    pub key: Option<Secret>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub context_length: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceScope {
    Project,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub scope: String,
    pub available: bool,
    pub enabled: bool,
    pub missing_tools: Vec<String>,
    pub verdict: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCatalogue {
    pub skills: Vec<SkillSummary>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub scope: String,
    pub version: u64,
    pub path: PathBuf,
    pub dir: PathBuf,
    pub available: bool,
    pub required_tools: Vec<String>,
    pub missing_tools: Vec<String>,
    pub bundled_files: Vec<String>,
    pub source: Option<String>,
    pub source_kind: Option<String>,
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSource {
    pub key: String,
    pub repo: String,
    pub path: String,
    pub built_in: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillHit {
    pub name: String,
    pub description: String,
    pub repo: String,
    pub install_url: String,
    pub installed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSearch {
    pub hits: Vec<SkillHit>,
    pub errors: Vec<String>,
    pub sources: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillUpdate {
    pub name: String,
    pub status: String,
    pub detail: Option<String>,
    pub to: Option<String>,
    pub caution: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceNotice {
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Grant {
    pub network: bool,
    pub read_paths: Vec<String>,
    pub write_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginComponent {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginAction {
    pub id: String,
    pub title: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginActivation {
    Enabled,
    Disabled,
    Changed,
    Collision,
    Shadowed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSummary {
    pub id: String,
    pub version: String,
    pub scope: ResourceScope,
    pub activation: PluginActivation,
    pub hash: String,
    pub root: PathBuf,
    pub components: Vec<PluginComponent>,
    pub actions: Vec<PluginAction>,
    pub updatable: bool,
    pub can_rollback: bool,
    pub health: Option<String>,
    pub last_failure: Option<String>,
    pub grant: Option<Grant>,
    pub blocked: Option<String>,
    pub access_description: String,
    pub hooks: Vec<HookDescription>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDescription {
    pub id: String,
    pub points: Vec<String>,
    pub matcher: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommand {
    pub name: String,
    pub description: String,
    pub action_id: String,
    pub has_snippets: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCatalogue {
    pub plugins: Vec<PluginSummary>,
    pub notices: Vec<String>,
    pub commands: Vec<PluginCommand>,
    pub hook_review: Option<PluginSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePlugin {
    pub name: String,
    pub description: String,
    pub category: Option<String>,
    pub source: String,
    pub installed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marketplace {
    pub name: String,
    pub url: String,
    pub commit: String,
    pub plugins: Vec<MarketplacePlugin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marketplaces {
    pub marketplaces: Vec<Marketplace>,
    pub unfetched: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookFile {
    pub event: String,
    pub file: String,
    pub matcher: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookCatalogue {
    pub hooks: Vec<HookFile>,
    pub project: Option<PluginSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub started: f64,
    pub updated: f64,
    pub title: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub events: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "resource",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ReadResource {
    Models,
    Pulse,
    Sessions,
    Skills,
    Skill { name: String },
    SkillSources,
    SkillSearch { query: String },
    SkillUpdates { name: Option<String> },
    SkillLockfile,
    Plugins,
    PluginDoctor,
    Marketplaces,
    Hooks,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resource", content = "data", rename_all = "snake_case")]
pub enum ResourceResult {
    Models(ModelCatalogue),
    Pulse(ResourceNotice),
    Sessions(Vec<SessionSummary>),
    Skills(SkillCatalogue),
    Skill(SkillInfo),
    SkillSources(Vec<SkillSource>),
    SkillSearch(SkillSearch),
    SkillUpdates(Vec<SkillUpdate>),
    SkillLockfile {
        path: PathBuf,
        exists: bool,
        entries: usize,
    },
    Plugins(PluginCatalogue),
    Marketplaces(Marketplaces),
    Hooks(HookCatalogue),
    Changed(ResourceNotice),
    ModelSaved {
        name: String,
        catalogue: ModelCatalogue,
    },
    SkillUsed {
        name: String,
        description: String,
        message: String,
    },
    SkillInstalled {
        name: String,
        enabled: bool,
        findings: Vec<String>,
    },
    PluginInstalled {
        id: String,
    },
}

impl Command for ReadResource {
    const METHOD: &'static str = "application.resources";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ResourceResult;
}

/// The same resource projection using this chat's live stores and tool set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChatResource(pub ReadResource);
impl Command for ChatResource {
    const METHOD: &'static str = "session.resources";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = ResourceResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ChangeResource {
    SaveModel(ModelDraft),
    SetDefaultModel {
        name: String,
    },
    RemoveModel {
        name: String,
    },
    UpdateModelKey {
        name: String,
        key: Secret,
    },
    Search {
        provider: SearchProvider,
        key: Option<Secret>,
        url: Option<String>,
    },
    PulseFix,
    InstallSkill {
        source: String,
    },
    RemoveSkill {
        name: String,
    },
    ConfigureSkill {
        name: String,
        enabled: bool,
    },
    AddSkillSource {
        spec: String,
        path: Option<String>,
    },
    RemoveSkillSource {
        key: String,
    },
    UpdateSkills {
        name: Option<String>,
    },
    LockSkills,
    SyncSkills,
    InstallPlugin {
        source: String,
    },
    UpdatePlugin {
        id: String,
    },
    RollbackPlugin {
        id: String,
    },
    EnablePlugin {
        id: String,
        scope: ResourceScope,
        hash: String,
        grant: Grant,
    },
    DisablePlugin {
        id: String,
        scope: ResourceScope,
    },
    RemovePlugin {
        id: String,
    },
    AddMarketplace {
        source: String,
    },
    RemoveMarketplace {
        name: String,
    },
    RefreshMarketplaces,
    AddHook {
        event: String,
        matcher: String,
        command: String,
    },
    RemoveHook {
        event: String,
        file: String,
    },
}

impl Command for ChangeResource {
    const METHOD: &'static str = "application.resources.change";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ResourceResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverModels {
    pub protocol: ModelProtocol,
    pub base_url: String,
    pub key: Secret,
}
impl Command for DiscoverModels {
    const METHOD: &'static str = "application.models.discover";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = Vec<DiscoveredModel>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UseSkill {
    pub name: String,
}
impl Command for UseSkill {
    const METHOD: &'static str = "application.skill.use";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = ResourceResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandCommand {
    pub typed: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandedCommand {
    pub typed: String,
    pub prompt: String,
}
impl Command for ExpandCommand {
    const METHOD: &'static str = "application.command.expand";
    const SCOPE: ScopeKind = ScopeKind::Chat;
    type Output = ExpandedCommand;
}
