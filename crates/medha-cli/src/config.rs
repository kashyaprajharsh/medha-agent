//! Provider configuration. The CLI *resolves* config; it never defines it.
//!
//! Resolution order (highest wins):
//!   CLI flag  >  MEDHA_* env  >  ~/.medha/config.toml  >  TUI first-run model setup
//!
//! Only the `MEDHA_*` env namespace is read — never generic `OPENAI_*` /
//! `OPENAI_COMPATIBLE_*` names, and never a project `.env`. Those belong to the
//! app that owns the working directory; reading them let a repo's environment
//! silently hijack medha's model/credentials. `medha nadi` reports provenance.
//!
//! Nothing is hardcoded as a product default — a value first comes to exist
//! when the user saves a model profile in the TUI (the single interactive
//! setup surface; the old terminal wizard is gone). This file is the Phase-0
//! precursor to the
//! `medha.lock` `[routing]` table (§4.4). Secrets never live in `config.toml`
//! (§9); they resolve through a layered store (see [`store_key`]):
//!
//!   env var  >  ~/.medha/credentials.toml (owner-only, 0600)  >  OS keychain
//!
//! The owner-only credentials file is the default store — the same convention
//! as the other agent CLIs (Codex, OpenCode, gh, gcloud all keep an
//! auth/credentials file under the user's home). The OS keychain is NOT the
//! default because macOS binds keychain ACLs to the binary's code signature:
//! every rebuilt (ad-hoc-signed) dev binary looks like a new app and throws a
//! password dialog on each read — the exact prompt-fatigue this design
//! removes. Keys stored in the keychain by older builds are migrated into the
//! file on first read (one final OS prompt, then never again).
//! `MEDHA_CRED_STORE=keychain` opts back into keychain-first for users who
//! prefer it. Found keys are cached in-process, so the store is consulted at
//! most once per endpoint per run.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const KEYRING_SERVICE: &str = "medha";

/// Common OpenAI-compatible endpoints offered in the wizard. Suggestions only —
/// never silent defaults; the user always confirms or types their own.
const PRESETS: &[(&str, &str)] = &[
    ("Ollama (local)", "http://localhost:11434/v1"),
    ("LM Studio (local)", "http://localhost:1234/v1"),
    ("llama.cpp server (local)", "http://localhost:8080/v1"),
    ("vLLM / SGLang (local)", "http://localhost:8000/v1"),
    ("OpenRouter", "https://openrouter.ai/api/v1"),
    ("Together", "https://api.together.xyz/v1"),
    ("Groq", "https://api.groq.com/openai/v1"),
    ("OpenAI", "https://api.openai.com/v1"),
];

/// Provider suggestions shared by first-run setup and the in-TUI model
/// manager. They are suggestions only; Custom always remains available.
pub(crate) fn provider_presets() -> &'static [(&'static str, &'static str)] {
    PRESETS
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Legacy single-provider connection written by the removed terminal
    /// wizard. Load-only: [`load`] folds it into `models` (one-time migration)
    /// and it is never serialized again. Nothing else may read it.
    #[serde(default, skip_serializing)]
    provider: Option<ProviderConfig>,
    /// User-named model connections — the ONLY place connections live. A
    /// connection is more than a model id: it includes the provider endpoint
    /// and its discovered context window.
    #[serde(default)]
    pub models: BTreeMap<String, ProviderConfig>,
    /// Which saved model starts new sessions. `None` (or a stale name) falls
    /// back to the first saved model; no models at all → the TUI opens its
    /// first-run model setup.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Agent-level settings (K1 identity, etc.). Optional so existing configs
    /// without an `[agent]` section still parse.
    #[serde(default)]
    pub agent: AgentConfig,
    /// Web-search provider selection (set via `/search`). Optional so existing
    /// configs without a `[search]` section still parse.
    #[serde(default)]
    pub search: SearchConfig,
    /// User-scoped MCP servers (machine-local, never committed). The API key is
    /// NOT stored here — it lives in the credential store (`mcp://<id>`), and the
    /// command references it as `${key}`, substituted at spawn.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServer>,
}

impl McpServer {
    /// What the server points at — URL for a hosted server, command line for a
    /// local one. Used wherever a definition is shown to the user.
    pub fn target(&self) -> String {
        if self.url.is_empty() {
            self.command.join(" ")
        } else {
            self.url.clone()
        }
    }
}

/// A user-scoped MCP server definition. Secret-free: any API key is referenced
/// as the literal `${key}` in `command` and resolved from the credential store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpServer {
    /// Local stdio server. Mutually exclusive with `url`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Hosted server reached over Streamable HTTP. Takes precedence over `command`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Credential scheme for `url`: `""` (none), `bearer`, or `oauth`. A bearer
    /// token lives under `mcp://<id>`; OAuth tokens under `mcp-oauth://<id>`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub auth: String,
    /// Extra environment for the server. Values may reference `${key}` (resolved
    /// from the credential store) — e.g. `GITHUB_TOKEN = "${key}"`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `workspace` (default) requires approval to start; `trusted` auto-connects.
    #[serde(default)]
    pub trust: String,
    /// Switched off: kept here with its credentials, but never connected. Lets a
    /// server be parked without losing its definition.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Tools exposed to the model. `allow` (when set) whitelists, then `deny`
    /// subtracts; entries are exact names or a `prefix*` glob. A big server can
    /// publish 100+ schemas — filtering keeps them out of every model request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_tools: Vec<String>,
    /// Per-server network override; unset falls back to the host `[mcp]` default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
    /// Opt in to concurrent calls. Off by default: most servers hold per-session
    /// state, and a server's own "read only" annotation is a hint, not a promise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parallel_calls: bool,
}

/// Persisted web-search choice. API keys are secrets and live in the credential
/// store (keyed `search://<provider>`), never here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Chosen provider id: `tavily` | `brave` | `searxng` | `duckduckgo`.
    /// `None` = never configured → auto-detect from env for back-compat.
    #[serde(default)]
    pub provider: Option<String>,
    /// SearXNG instance base URL, used only when the provider is `searxng`.
    #[serde(default)]
    pub searxng_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Persona override for the K1 identity sheath. `None` → built-in default.
    #[serde(default)]
    pub identity: Option<String>,
}

/// Compatibility name for the provider-owned deployment profile. Keeping one
/// type prevents config, model switching, and the HTTP client from disagreeing
/// about protocol, authentication, headers, or limits.
pub type ProviderConfig = providers::ProviderProfile;

/// A display-safe saved model. API keys remain in the OS keychain and are never
/// included here or serialized into config.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProfile {
    pub name: String,
    pub provider: ProviderConfig,
    pub is_default: bool,
}

/// Where a resolved configuration value came from. Tracked so `medha nadi` and
/// the startup line can answer "why this model?" without guesswork — the exact
/// class of question that the `.env` hijack made impossible to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A `--model` / `--base-url` CLI flag (one-session override).
    Flag,
    /// A `MEDHA_*` environment variable.
    Env,
    /// A saved profile in `~/.medha/config.toml`.
    Config,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Flag => "CLI flag",
            Source::Env => "MEDHA_* env",
            Source::Config => "~/.medha/config.toml",
        }
    }
}

/// Where the API key resolved from. Distinct from [`Source`] because credentials
/// have their own layered store (env → credentials file → keychain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredSource {
    /// `MEDHA_API_KEY` environment variable.
    Env,
    /// `~/.medha/credentials.toml` (owner-only file store).
    CredentialsFile,
    /// The OS keychain (or nowhere — resolved lazily at connect time).
    KeychainOrNone,
}

impl CredSource {
    pub fn label(self) -> &'static str {
        match self {
            CredSource::Env => "MEDHA_API_KEY env",
            CredSource::CredentialsFile => "~/.medha/credentials.toml",
            CredSource::KeychainOrNone => "OS keychain (or unset)",
        }
    }
}

/// What the kernel actually runs with, after resolution + secret lookup.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub name: String,
    pub provider: providers::ProviderProfile,
    /// Resolved from environment/credential storage; never serialized.
    pub credential: String,
    /// Where the model id resolved from (provenance for diagnostics).
    pub model_source: Source,
    /// Where the base URL resolved from.
    pub base_url_source: Source,
    /// Where the credential resolved from.
    pub credential_source: CredSource,
}

impl Config {
    /// All selectable models in stable display order. Every entry is an
    /// ordinary, removable profile — there is no reserved name.
    pub fn model_profiles(&self) -> Vec<ModelProfile> {
        let active = self.startup_model();
        self.models
            .iter()
            .map(|(name, provider)| ModelProfile {
                name: name.clone(),
                provider: provider.clone(),
                is_default: active == Some(name.as_str()),
            })
            .collect()
    }

    pub fn model_profile(&self, name: &str) -> Option<&ProviderConfig> {
        self.models.get(name)
    }

    /// Effective startup model name: the configured default when it still
    /// exists, else the first saved model (a removed default must never make
    /// Medha unstartable), else `None` (no models — first-run setup).
    fn startup_model(&self) -> Option<&str> {
        self.default_model
            .as_deref()
            .filter(|n| self.models.contains_key(*n))
            .or_else(|| self.models.keys().next().map(String::as_str))
    }

    fn selected_model(&self) -> Option<(&str, &ProviderConfig)> {
        let name = self.startup_model()?;
        Some((name, self.models.get(name)?))
    }

    pub fn add_model(
        &mut self,
        name: String,
        provider: ProviderConfig,
        make_default: bool,
    ) -> Result<()> {
        self.validate_new_model_name(&name)?;
        provider.validate().map_err(anyhow::Error::msg)?;
        // The first saved model is implicitly the startup default.
        if make_default || self.models.is_empty() {
            self.default_model = Some(name.clone());
        }
        self.models.insert(name, provider);
        Ok(())
    }

    /// Validate a new profile name before any related secret is persisted.
    /// Kept separate from [`Self::add_model`] so an interactive form can give
    /// immediate feedback without writing an API key for an invalid profile.
    pub fn validate_new_model_name(&self, name: &str) -> Result<()> {
        validate_model_name(name)?;
        if self.models.contains_key(name) {
            return Err(anyhow::anyhow!(
                "a model profile named '{name}' already exists; choose a different name"
            ));
        }
        Ok(())
    }

    pub fn set_default_model(&mut self, name: &str) -> Result<()> {
        if self.model_profile(name).is_none() {
            return Err(anyhow::anyhow!("no saved model named '{name}'"));
        }
        self.default_model = Some(name.to_string());
        Ok(())
    }

    /// Remove a saved profile. Removing the startup default promotes the first
    /// remaining model. Stored credentials are retained because multiple
    /// profiles may share the same endpoint.
    pub fn remove_model(&mut self, name: &str) -> Result<ProviderConfig> {
        let removed = self
            .models
            .remove(name)
            .ok_or_else(|| anyhow::anyhow!("no saved model named '{name}'"))?;
        if self.default_model.as_deref() == Some(name) {
            self.default_model = self.models.keys().next().cloned();
        }
        Ok(removed)
    }

    /// The currently-selected search provider for display/preselection. An
    /// unset `[search]` shows as DuckDuckGo (the effective default).
    pub fn search_provider(&self) -> tools::SearchProvider {
        self.search
            .provider
            .as_deref()
            .map(tools::SearchProvider::from_id)
            .unwrap_or_default()
    }

    /// Record the chosen search provider (and, for SearXNG, its instance URL).
    /// The API key, when the provider needs one, is stored separately via
    /// [`store_key`] — never in config.toml.
    pub fn set_search(&mut self, provider: tools::SearchProvider, searxng_url: Option<String>) {
        self.search.provider = Some(provider.as_str().to_string());
        // Keep a stale URL from leaking into a non-SearXNG choice.
        self.search.searxng_url = match provider {
            tools::SearchProvider::Searxng => searxng_url,
            _ => None,
        };
    }
}

/// Credential-store id holding a keyed search provider's API key. Providers that
/// need no key (DuckDuckGo, SearXNG) return `None`.
pub(crate) fn search_cred_id(provider: tools::SearchProvider) -> Option<&'static str> {
    match provider {
        tools::SearchProvider::Tavily => Some("search://tavily"),
        tools::SearchProvider::Brave => Some("search://brave"),
        tools::SearchProvider::DuckDuckGo | tools::SearchProvider::Searxng => None,
    }
}

/// Auto-detect a provider from the environment when `[search]` is unset, in the
/// legacy priority order. This preserves behavior for setups that only ever
/// exported `TAVILY_API_KEY`/`BRAVE_API_KEY`/`MEDHA_SEARXNG_URL`.
fn auto_detect_search_provider() -> tools::SearchProvider {
    use tools::SearchProvider as P;
    let has = |k: &str| std::env::var(k).ok().is_some_and(|v| !v.trim().is_empty());
    if has("TAVILY_API_KEY") {
        P::Tavily
    } else if has("BRAVE_API_KEY") {
        P::Brave
    } else if has("MEDHA_SEARXNG_URL") {
        P::Searxng
    } else {
        P::DuckDuckGo
    }
}

/// Build the live [`tools::SearchSettings`] from saved config + the credential
/// store. Both keyed providers' keys are loaded regardless of the chosen search
/// backend, because `web.fetch`/`web.crawl` use the Tavily key independently.
/// Keys still absent here fall back to env vars tool-side.
pub fn resolve_search(cfg: &Config) -> tools::SearchSettings {
    let provider = match cfg.search.provider.as_deref() {
        Some(p) => tools::SearchProvider::from_id(p),
        None => auto_detect_search_provider(),
    };
    tools::SearchSettings {
        provider,
        tavily_key: load_key("search://tavily"),
        brave_key: load_key("search://brave"),
        searxng_url: cfg.search.searxng_url.clone(),
    }
}

/// One-time migration of a wizard-era `[provider]` block into `models`.
/// Returns true when the config changed and should be re-saved. The migrated
/// connection becomes a normal named (and removable) profile; the startup
/// default is preserved — legacy configs without one start on the migrated
/// connection, exactly as before.
fn migrate_legacy_provider(cfg: &mut Config) -> bool {
    let Some(legacy) = cfg.provider.take() else {
        return false;
    };
    if legacy.base_url.is_empty() || legacy.model.is_empty() {
        return true; // empty stub — drop it, rewrite without [provider]
    }
    if cfg.models.values().any(|m| *m == legacy) {
        return true; // identical connection already saved under a name
    }
    let name = unique_profile_name(&cfg.models, &profile_name_from_model(&legacy.model));
    if cfg.default_model.is_none() {
        cfg.default_model = Some(name.clone());
    }
    cfg.models.insert(name, legacy);
    true
}

/// Profile name for a new connection, derived from its model id and unique
/// among the saved profiles. The setup form deliberately never asks the user
/// to invent a name — this is the single naming path.
pub(crate) fn derive_profile_name(cfg: &Config, model_id: &str) -> String {
    unique_profile_name(&cfg.models, &profile_name_from_model(model_id))
}

/// Derive a valid kebab-case profile name from a model id, e.g.
/// `Qwen/Qwen3.5-397B-A17B` → `qwen3-5-397b-a17b`.
fn profile_name_from_model(model: &str) -> String {
    let last = model.rsplit('/').next().unwrap_or(model);
    let mut out = String::new();
    for c in last.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "imported".into()
    } else {
        out
    }
}

fn unique_profile_name(models: &BTreeMap<String, ProviderConfig>, base: &str) -> String {
    if !models.contains_key(base) {
        return base.to_string();
    }
    (2..)
        .map(|i| format!("{base}-{i}"))
        .find(|cand| !models.contains_key(cand))
        .expect("some suffix is free")
}

fn validate_model_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if valid {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "model name must be kebab-case (lowercase letters, digits, single hyphens)"
        ))
    }
}

/// The MEDHA home directory — `$MEDHA_HOME` if set, else `~/.medha`. Holds
/// user-global config (`config.toml`), user skills, and all per-workspace
/// runtime state under `projects/` (see [`state_dir`]).
pub fn medha_home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("MEDHA_HOME") {
        return Ok(PathBuf::from(h));
    }
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".medha"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(medha_home()?.join("config.toml"))
}

/// User-scoped reusable procedures. Kept beneath [`medha_home`] so
/// `MEDHA_HOME` relocates config, skills, and runtime state together.
pub fn user_skills_dir() -> Result<PathBuf> {
    Ok(user_skills_dir_in(&medha_home()?))
}

fn user_skills_dir_in(home: &std::path::Path) -> PathBuf {
    home.join("skills")
}

/// Registered skill sources ("taps"). Lives beside the user skills dir so
/// sources relocate with `MEDHA_HOME` like everything else. Not a skill folder
/// (no `SKILL.md`), so discovery ignores it.
pub fn user_taps_path() -> Result<PathBuf> {
    Ok(user_skills_dir()?.join("taps.toml"))
}

/// The skills lockfile for reproducible team setups. Lives in the workspace
/// (committed with the repo) — not the user home — so a team shares one file
/// and `/skill sync` reproduces the same skill set.
pub fn skills_lock_path() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join("medha-skills.lock"))
}

/// Per-workspace runtime state directory: `~/.medha/projects/<encoded-cwd>/`
/// (Claude Code style). Runtime state — the event log, artifacts, snapshots,
/// logs — lives HERE, out of the working tree, so it never clutters or gets
/// committed to the user's repos. Only committed config (`.medha/skills`,
/// `medha.lock`) stays in the workspace. Creates the dir. `workspace` should be
/// the canonicalized cwd so the same project always maps to the same dir.
pub fn state_dir(workspace: &std::path::Path) -> Result<PathBuf> {
    let dir = state_dir_in(&medha_home()?, workspace);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Pure path computation behind [`state_dir`] (no I/O) — testable without env.
fn state_dir_in(home: &std::path::Path, workspace: &std::path::Path) -> PathBuf {
    home.join("projects").join(encode_workspace(workspace))
}

/// Encode an absolute workspace path into one readable directory name, the way
/// Claude Code does: every path separator becomes `-`, so
/// `/Users/x/proj` → `-Users-x-proj`. Existing hyphens are left as-is; a
/// Windows drive colon becomes `-`.
fn encode_workspace(p: &std::path::Path) -> String {
    p.to_string_lossy()
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect()
}

pub fn load() -> Result<Option<Config>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut cfg: Config = toml::from_str(&text).context("parsing config.toml")?;
    // Wizard-era [provider] block → ordinary named profile, rewritten once so
    // the legacy shape disappears from disk. Best-effort save: a read-only FS
    // still gets a working in-memory config.
    if migrate_legacy_provider(&mut cfg) {
        let _ = save(&cfg);
    }
    Ok(Some(cfg))
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = toml::to_string_pretty(cfg).context("serializing config")?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Resolve effective provider settings from (in order) CLI flags → env → config
/// file. `cfg` may be `None` (no config saved yet); returns `None` if base URL
/// or model can't be determined from any source — the TUI then opens its
/// first-run model setup (headless callers get an actionable error instead).
///
/// Only the `MEDHA_*` env namespace is honored. Generic third-party spellings
/// (`OPENAI_*`, `OPENAI_COMPATIBLE_*`) are deliberately NOT read: they belong to
/// whatever app owns the working directory, and reading them let a project's
/// `.env`/environment silently hijack medha's model and credentials. medha's
/// config is a function of medha's own state only.
pub fn resolve(
    cfg: Option<&Config>,
    flag_base_url: Option<String>,
    flag_model: Option<String>,
) -> Result<Option<Resolved>> {
    resolve_inner(cfg, flag_base_url, flag_model, true)
}

fn resolve_inner(
    cfg: Option<&Config>,
    flag_base_url: Option<String>,
    flag_model: Option<String>,
    allow_keychain: bool,
) -> Result<Option<Resolved>> {
    // An explicit endpoint/model is a one-session override, even if only one
    // half of the connection came from a saved profile. Do not label that as a
    // persisted profile in the TUI — selecting it again must be unambiguous.
    let has_override = flag_base_url.is_some()
        || flag_model.is_some()
        || first_env(&["MEDHA_BASE_URL"]).is_some()
        || first_env(&["MEDHA_MODEL"]).is_some();
    let (profile, configured) = cfg.and_then(|c| c.selected_model()).unzip();
    // flag > MEDHA_* env > saved profile — each layer also records its provenance.
    let (base_url, base_url_source) = pick_source(
        flag_base_url,
        "MEDHA_BASE_URL",
        configured.map(|p| p.base_url.clone()),
    );
    let (model, model_source) = pick_source(
        flag_model,
        "MEDHA_MODEL",
        configured.map(|p| p.model.clone()),
    );
    let (Some(base_url), Some(model)) = (base_url, model) else {
        return Ok(None);
    };
    let base_url_source = base_url_source.unwrap_or(Source::Config);
    let model_source = model_source.unwrap_or(Source::Config);

    // Environment first, then the layered credential store. The short circuit
    // avoids touching the macOS keychain when a `MEDHA_API_KEY` is present.
    let env_key = first_env(&["MEDHA_API_KEY"]);
    let file_key = if env_key.is_none() {
        file_load_key(&base_url)
    } else {
        None
    };
    let credential_source = if env_key.is_some() {
        CredSource::Env
    } else if file_key.is_some() {
        CredSource::CredentialsFile
    } else {
        CredSource::KeychainOrNone
    };
    let api_key = normalize_api_key(
        &env_key
            .or(file_key)
            .or_else(|| allow_keychain.then(|| load_key(&base_url)).flatten())
            .unwrap_or_default(),
    );

    let max_ctx = first_env(&["MEDHA_MAX_CTX"])
        .map(|value| parse_positive_u32("MEDHA_MAX_CTX", &value))
        .transpose()?
        .or_else(|| configured.and_then(|p| p.max_ctx));
    let protocol = first_env(&["MEDHA_PROTOCOL"])
        .map(|value| parse_protocol(&value))
        .transpose()?
        .or_else(|| configured.map(|profile| profile.protocol))
        .unwrap_or_default();
    let configured_auth = configured.map(|profile| profile.auth).unwrap_or_default();
    let auth = first_env(&["MEDHA_AUTH"])
        .map(|value| parse_auth(&value))
        .transpose()?
        .unwrap_or_else(|| {
            if configured_auth.requires_credential() || api_key.is_empty() {
                configured_auth
            } else {
                default_auth(protocol)
            }
        });
    let headers = first_env(&["MEDHA_HEADERS_JSON"])
        .map(|value| parse_headers(&value))
        .transpose()?
        .or_else(|| configured.map(|profile| profile.headers.clone()))
        .unwrap_or_default();
    let max_output_tokens = first_env(&["MEDHA_MAX_OUTPUT_TOKENS"])
        .map(|value| parse_positive_u64("MEDHA_MAX_OUTPUT_TOKENS", &value))
        .transpose()?
        .or_else(|| configured.and_then(|profile| profile.max_output_tokens));
    let token_counter = first_env(&["MEDHA_TOKEN_COUNTER"])
        .map(|value| parse_token_counter(&value))
        .transpose()?
        .or_else(|| configured.map(|profile| profile.token_counter))
        .unwrap_or_default();
    let token_accounting = first_env(&["MEDHA_TOKEN_ACCOUNTING"])
        .map(|value| parse_token_accounting(&value))
        .transpose()?
        .or_else(|| configured.map(|profile| profile.token_accounting))
        .unwrap_or_default();
    let reasoning = first_env(&["MEDHA_REASONING_SUPPORT"])
        .map(|value| parse_reasoning_support(&value))
        .transpose()?
        .or_else(|| configured.map(|profile| profile.reasoning))
        .unwrap_or_default();

    let provider = providers::ProviderProfile {
        protocol,
        base_url,
        model,
        auth,
        headers,
        max_ctx,
        max_output_tokens,
        token_counter,
        token_accounting,
        reasoning,
    };
    provider.validate().map_err(anyhow::Error::msg)?;
    if provider.auth.requires_credential() && api_key.is_empty() {
        anyhow::bail!(
            "model profile requires a credential for '{}' authentication",
            auth_label(provider.auth)
        );
    }

    Ok(Some(Resolved {
        name: if has_override {
            "override".to_string()
        } else {
            profile.unwrap_or("override").to_string()
        },
        provider,
        credential: api_key,
        model_source,
        base_url_source,
        credential_source,
    }))
}

/// flag → `MEDHA_*` env → configured value, returning the chosen value with its
/// provenance. The configured tier reports no `Source` (the caller maps a bare
/// configured value to [`Source::Config`]) so a missing value stays `None`.
fn pick_source(
    flag: Option<String>,
    env_name: &str,
    configured: Option<String>,
) -> (Option<String>, Option<Source>) {
    if let Some(v) = flag {
        return (Some(v), Some(Source::Flag));
    }
    if let Some(v) = first_env(&[env_name]) {
        return (Some(v), Some(Source::Env));
    }
    (configured, None)
}

/// Generic third-party env prefixes medha intentionally IGNORES. Surfaced by
/// `/pulse` so a stray `OPENAI_MODEL` (e.g. from a project this shell was set up
/// for) is visibly acknowledged-and-ignored rather than a silent mystery.
const IGNORED_ENV_PREFIXES: &[&str] = &[
    "OPENAI_",
    "GOOGLE_",
    "GEMINI_",
    "ANTHROPIC_",
    "AZURE_OPENAI_",
];

/// Severity of a [`Check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Warn,
    Error,
}

impl Health {
    fn icon(self) -> &'static str {
        match self {
            Health::Ok => "✔",
            Health::Warn => "⚠",
            Health::Error => "✗",
        }
    }
}

/// One diagnosed condition. `auto_fixable` marks the ones `/pulse fix` /
/// `medha pulse --fix` can repair without asking (all are non-destructive).
pub struct Check {
    pub health: Health,
    pub title: String,
    pub detail: String,
    pub auto_fixable: bool,
}

/// A prompt-free snapshot of how medha resolves its configuration right now,
/// with the provenance of every value and a list of health checks — the data
/// behind `medha pulse` / `/pulse`. (Distinct from the agent's `diagnostics`
/// tool, which reports compiler/linter errors about the user's code.)
pub struct Pulse {
    pub medha_home: String,
    pub config_path: String,
    pub config_exists: bool,
    /// `Ok(Some)` = a model resolved; `Ok(None)` = nothing configured yet;
    /// `Err` = a resolution error (e.g. a bad `MEDHA_*` value).
    pub resolved: std::result::Result<Option<Resolved>, String>,
    /// Names (only) of `MEDHA_*` env vars currently set — never their values.
    pub medha_env: Vec<String>,
    /// Names of generic third-party LLM env vars present but ignored by medha.
    pub ignored_env: Vec<String>,
    /// Path to a project `medha.lock` if one exists in the cwd.
    pub project_lock: Option<String>,
    /// `[routing] executor` from that lock, if set.
    pub lock_executor: Option<String>,
    /// Diagnosed conditions, most-severe first.
    pub checks: Vec<Check>,
}

/// Build the pulse snapshot. Prompt-free: credential provenance is derived from
/// env + the credentials file only (the keychain is never probed here, so
/// running `pulse` can't trigger an OS auth dialog or spend API budget).
pub fn pulse(
    cfg: Option<&Config>,
    flag_base_url: Option<String>,
    flag_model: Option<String>,
) -> Pulse {
    let scan_prefixed = |prefixes: &[&str]| -> Vec<String> {
        let mut names: Vec<String> = std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| prefixes.iter().any(|p| k.starts_with(p)))
            .collect();
        names.sort();
        names
    };
    let medha_env = scan_prefixed(&["MEDHA_"]);
    let ignored_env = scan_prefixed(IGNORED_ENV_PREFIXES);

    let cwd_lock = std::env::current_dir().ok().map(|d| d.join("medha.lock"));
    let (project_lock, lock_executor) = match cwd_lock {
        Some(p) if p.exists() => {
            let executor = lockfile::MedhaLock::load(&p).and_then(|l| l.routing.executor);
            (Some(p.display().to_string()), executor)
        }
        _ => (None, None),
    };

    let resolved = resolve_inner(cfg, flag_base_url, flag_model, false).map_err(|e| e.to_string());
    let checks = diagnose_checks(cfg, &resolved, &ignored_env);

    Pulse {
        medha_home: medha_home()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("<unresolved: {e}>")),
        config_path: config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("<unresolved: {e}>")),
        config_exists: config_path().map(|p| p.exists()).unwrap_or(false),
        resolved,
        medha_env,
        ignored_env,
        project_lock,
        lock_executor,
        checks,
    }
}

/// Compute the health checks. Ordered most-severe-first for display.
fn diagnose_checks(
    cfg: Option<&Config>,
    resolved: &std::result::Result<Option<Resolved>, String>,
    ignored_env: &[String],
) -> Vec<Check> {
    let mut checks = Vec::new();

    match resolved {
        Ok(Some(r)) => {
            // Credential required by the profile but none resolvable.
            if r.provider.auth.requires_credential() && r.credential.is_empty() {
                checks.push(Check {
                    health: Health::Error,
                    title: "Credential missing".into(),
                    detail: format!(
                        "'{}' uses {} auth but no key was found (env MEDHA_API_KEY or credentials file). \
                         Add one via /model, or export MEDHA_API_KEY.",
                        r.provider.model,
                        r.provider.auth.as_str()
                    ),
                    auto_fixable: false,
                });
            } else {
                checks.push(Check {
                    health: Health::Ok,
                    title: "Credential resolves".into(),
                    detail: format!(
                        "{} auth, key {}",
                        r.provider.auth.as_str(),
                        if r.credential.is_empty() {
                            "not required"
                        } else {
                            "present"
                        }
                    ),
                    auto_fixable: false,
                });
            }

            // Endpoint/protocol mismatch — the exact class that produced the 404 /
            // API_KEY_INVALID confusion (a Gemini endpoint spoken over OpenAI, or
            // vice-versa).
            let url = r.provider.base_url.to_ascii_lowercase();
            let proto = r.provider.protocol.as_str();
            let proto_is_gemini = proto.contains("gemini");
            let url_is_gemini = url.contains("generativelanguage.googleapis.com");
            if url_is_gemini && !proto_is_gemini {
                checks.push(Check {
                    health: Health::Warn,
                    title: "Endpoint/protocol mismatch".into(),
                    detail: format!(
                        "base_url looks like Google Gemini but protocol is '{proto}'. \
                         Requests will likely 404 or reject the key. Expected a gemini protocol."
                    ),
                    auto_fixable: false,
                });
            } else if !url_is_gemini && proto_is_gemini && url.contains("/v1") {
                checks.push(Check {
                    health: Health::Warn,
                    title: "Endpoint/protocol mismatch".into(),
                    detail: format!(
                        "protocol is '{proto}' (Gemini-style) but base_url looks OpenAI-compatible. \
                         Consider an open-ai-chat protocol for this endpoint."
                    ),
                    auto_fixable: false,
                });
            }

            // Compaction needs a context window.
            if r.provider.max_ctx.is_none() {
                checks.push(Check {
                    health: Health::Warn,
                    title: "Context window unknown".into(),
                    detail: format!(
                        "no max_ctx for '{}'. Compaction stays off until it's found on models.dev \
                         or you set MEDHA_MAX_CTX=<tokens>.",
                        r.provider.model
                    ),
                    auto_fixable: false,
                });
            }
        }
        Ok(None) => checks.push(Check {
            health: Health::Error,
            title: "No model configured".into(),
            detail: "run `medha` and add one in /model, or set MEDHA_MODEL + MEDHA_BASE_URL."
                .into(),
            auto_fixable: false,
        }),
        Err(e) => checks.push(Check {
            health: Health::Error,
            title: "Resolution error".into(),
            detail: e.clone(),
            auto_fixable: false,
        }),
    }

    // Stale default_model — auto-fixable (promote the first saved profile).
    if let Some(cfg) = cfg {
        if let Some(def) = &cfg.default_model {
            if !cfg.models.contains_key(def) {
                let has_others = !cfg.models.is_empty();
                checks.push(Check {
                    health: Health::Warn,
                    title: "Stale default model".into(),
                    detail: format!(
                        "default_model = '{def}' but no such profile exists.{}",
                        if has_others {
                            " `pulse --fix` will promote the first saved profile."
                        } else {
                            " Add a model to fix."
                        }
                    ),
                    auto_fixable: has_others,
                });
            }
        }
    }

    // Reassurance: foreign LLM env is present but ignored (acknowledged, not silent).
    if !ignored_env.is_empty() {
        checks.push(Check {
            health: Health::Ok,
            title: "Foreign LLM env ignored".into(),
            detail: format!(
                "{} present but not read by medha (they belong to this directory's app).",
                ignored_env.join(", ")
            ),
            auto_fixable: false,
        });
    }

    // Most severe first: Error, then Warn, then Ok.
    let rank = |h: Health| match h {
        Health::Error => 0,
        Health::Warn => 1,
        Health::Ok => 2,
    };
    checks.sort_by_key(|c| rank(c.health));
    checks
}

/// Apply the non-destructive fixes `/pulse fix` / `medha pulse --fix` offers,
/// mutating `cfg` in place. Returns a human-readable line per applied fix; an
/// empty vec means nothing needed fixing. The caller persists with [`save`].
pub fn apply_safe_fixes(cfg: &mut Config) -> Vec<String> {
    let mut fixed = Vec::new();

    // Repair a default_model pointing at a removed/renamed profile.
    if let Some(def) = cfg.default_model.clone() {
        if !cfg.models.contains_key(&def) {
            match cfg.models.keys().next().cloned() {
                Some(first) => {
                    cfg.default_model = Some(first.clone());
                    fixed.push(format!("stale default model '{def}' → promoted '{first}'"));
                }
                None => {
                    cfg.default_model = None;
                    fixed.push(format!(
                        "stale default model '{def}' cleared (no saved profiles)"
                    ));
                }
            }
        }
    }

    fixed
}

impl Pulse {
    /// True when at least one check is auto-fixable.
    pub fn has_fixes(&self) -> bool {
        self.checks.iter().any(|c| c.auto_fixable)
    }

    /// Overall verdict icon+word for the summary line.
    fn verdict(&self) -> (&'static str, &'static str) {
        if self.checks.iter().any(|c| c.health == Health::Error) {
            ("✗", "needs attention")
        } else if self.checks.iter().any(|c| c.health == Health::Warn) {
            ("⚠", "ok with warnings")
        } else {
            ("✔", "healthy")
        }
    }

    /// Render a human-readable report for `medha pulse` (stdout) and the `/pulse`
    /// TUI message. No secrets ever appear — only sources and non-secret ids.
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut o = String::new();
        let (icon, word) = self.verdict();
        let _ = writeln!(o, "medha pulse — {icon} {word}\n");

        let _ = writeln!(o, "checks");
        for c in &self.checks {
            let _ = writeln!(o, "  {} {}", c.health.icon(), c.title);
            let _ = writeln!(o, "      {}", c.detail);
        }
        if self.has_fixes() {
            let _ = writeln!(
                o,
                "\n  → run `medha pulse --fix` (or `/pulse fix`) to auto-repair the fixable items."
            );
        }

        let _ = writeln!(o, "\npaths");
        let _ = writeln!(o, "  MEDHA_HOME   {}", self.medha_home);
        let _ = writeln!(
            o,
            "  config.toml  {} {}",
            self.config_path,
            if self.config_exists {
                "(present)"
            } else {
                "(absent — first-run setup will open)"
            }
        );

        let _ = writeln!(o, "\nactive model");
        match &self.resolved {
            Ok(Some(r)) => {
                let profile = if r.name == "override" {
                    "(one-session override)".to_string()
                } else {
                    format!("'{}'", r.name)
                };
                let _ = writeln!(o, "  profile      {profile}");
                let _ = writeln!(
                    o,
                    "  model        {}   [source: {}]",
                    r.provider.model,
                    r.model_source.label()
                );
                let _ = writeln!(
                    o,
                    "  base_url     {}   [source: {}]",
                    r.provider.base_url,
                    r.base_url_source.label()
                );
                let _ = writeln!(o, "  protocol     {}", r.provider.protocol.as_str());
                let _ = writeln!(o, "  auth         {}", r.provider.auth.as_str());
                let cred = if r.credential.is_empty() {
                    "none found".to_string()
                } else {
                    format!("present [source: {}]", r.credential_source.label())
                };
                let _ = writeln!(o, "  credential   {cred}");
            }
            Ok(None) => {
                let _ = writeln!(
                    o,
                    "  none configured yet — run `medha` and add one in /model"
                );
            }
            Err(e) => {
                let _ = writeln!(o, "  resolution error: {e}");
            }
        }

        let _ = writeln!(o, "\nenvironment");
        if self.medha_env.is_empty() {
            let _ = writeln!(o, "  MEDHA_* set   (none)");
        } else {
            let _ = writeln!(o, "  MEDHA_* set   {}", self.medha_env.join(", "));
        }
        if !self.ignored_env.is_empty() {
            let _ = writeln!(
                o,
                "  ignored      {}  ← present but NOT read by medha (they belong to the",
                self.ignored_env.join(", ")
            );
            let _ = writeln!(o, "               app that owns this directory, not medha)");
        }
        let _ = writeln!(
            o,
            "  .env         never read by medha (a project's .env cannot change medha's model/key)"
        );

        let _ = writeln!(o, "\nproject medha.lock");
        match (&self.project_lock, &self.lock_executor) {
            (Some(path), Some(exec)) => {
                let _ = writeln!(o, "  {path}");
                let _ = writeln!(o, "  [routing] executor = {exec}");
            }
            (Some(path), None) => {
                let _ = writeln!(o, "  {path} (no [routing] executor set)");
            }
            _ => {
                let _ = writeln!(o, "  (none in this directory)");
            }
        }
        o
    }
}

fn parse_positive_u32(name: &str, value: &str) -> Result<u32> {
    let parsed = value
        .trim()
        .parse::<u32>()
        .with_context(|| format!("invalid {name} '{value}'; expected a positive integer"))?;
    if parsed == 0 {
        anyhow::bail!("invalid {name} '0'; expected a positive integer");
    }
    Ok(parsed)
}

fn parse_positive_u64(name: &str, value: &str) -> Result<u64> {
    let parsed = value
        .trim()
        .parse::<u64>()
        .with_context(|| format!("invalid {name} '{value}'; expected a positive integer"))?;
    if parsed == 0 {
        anyhow::bail!("invalid {name} '0'; expected a positive integer");
    }
    Ok(parsed)
}

fn parse_protocol(value: &str) -> Result<kernel::Protocol> {
    value
        .parse()
        .map_err(|error: String| anyhow::anyhow!("invalid MEDHA_PROTOCOL: {error}"))
}

fn parse_auth(value: &str) -> Result<providers::AuthKind> {
    match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "none" | "off" => Ok(providers::AuthKind::None),
        "bearer" => Ok(providers::AuthKind::Bearer),
        "x-api-key" | "anthropic" => Ok(providers::AuthKind::XApiKey),
        "x-goog-api-key" | "google" | "gemini" => Ok(providers::AuthKind::XGoogApiKey),
        other => anyhow::bail!(
            "invalid MEDHA_AUTH '{other}'; expected 'none', 'bearer', 'x-api-key', or 'x-goog-api-key'"
        ),
    }
}

fn parse_headers(value: &str) -> Result<BTreeMap<String, String>> {
    serde_json::from_str(value).with_context(
        || "invalid MEDHA_HEADERS_JSON; expected a JSON object of non-secret header strings",
    )
}

fn default_auth(protocol: kernel::Protocol) -> providers::AuthKind {
    providers::AuthKind::for_protocol(protocol)
}

fn auth_label(auth: providers::AuthKind) -> &'static str {
    auth.as_str()
}

fn parse_token_counter(value: &str) -> Result<providers::openai_compat::OpenAiTokenCounter> {
    match value.trim().to_ascii_lowercase().as_str() {
        "vllm" => Ok(providers::openai_compat::OpenAiTokenCounter::Vllm),
        "none" | "off" => Ok(providers::openai_compat::OpenAiTokenCounter::None),
        other => anyhow::bail!("invalid MEDHA_TOKEN_COUNTER '{other}'; expected 'none' or 'vllm'"),
    }
}

fn parse_token_accounting(value: &str) -> Result<kernel::TokenAccountingMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "strict" => Ok(kernel::TokenAccountingMode::Strict),
        "adaptive" => Ok(kernel::TokenAccountingMode::Adaptive),
        other => anyhow::bail!(
            "invalid MEDHA_TOKEN_ACCOUNTING '{other}'; expected 'adaptive' or 'strict'"
        ),
    }
}

fn parse_reasoning_support(value: &str) -> Result<kernel::ReasoningSupport> {
    match value.trim().to_ascii_lowercase().as_str() {
        "unknown" | "unverified" => Ok(kernel::ReasoningSupport::Unknown),
        "unsupported" | "none" | "off" => Ok(kernel::ReasoningSupport::Unsupported),
        "effort" => Ok(kernel::ReasoningSupport::Effort),
        other => anyhow::bail!(
            "invalid MEDHA_REASONING_SUPPORT '{other}'; expected 'unknown', 'unsupported', or 'effort'"
        ),
    }
}

/// Resolve one saved named model for an in-TUI switch. An environment key keeps
/// its documented precedence and avoids an unnecessary keychain prompt.
pub fn resolve_model(cfg: &Config, name: &str) -> Result<Resolved> {
    let provider = cfg
        .model_profile(name)
        .ok_or_else(|| anyhow::anyhow!("no saved model named '{name}'"))?;
    // Environment first, then keychain. The short circuit matters on macOS:
    // merely reading a keychain item can show an authorization dialog. Only the
    // `MEDHA_API_KEY` namespace is read — never generic third-party key names.
    let api_key = normalize_api_key(
        &first_env(&["MEDHA_API_KEY"])
            .or_else(|| load_key(&provider.base_url))
            .unwrap_or_default(),
    );
    resolve_model_with_key(cfg, name, &api_key)
}

/// Resolve a saved model with a credential the user just supplied. This avoids
/// immediately reading Keychain after a write (and therefore avoids a second
/// macOS authorization prompt in the same flow).
pub(crate) fn resolve_model_with_key(cfg: &Config, name: &str, api_key: &str) -> Result<Resolved> {
    let provider = cfg
        .model_profile(name)
        .ok_or_else(|| anyhow::anyhow!("no saved model named '{name}'"))?;
    let api_key = normalize_api_key(api_key);
    provider.validate().map_err(anyhow::Error::msg)?;
    if provider.auth.requires_credential() && api_key.is_empty() {
        anyhow::bail!(
            "model profile '{name}' requires a credential; choose 'Add or update an API key' in /model"
        );
    }
    // A named-profile switch: model and endpoint both come from config.toml. The
    // credential source is reported prompt-free (env → file → keychain/none).
    let credential_source = if first_env(&["MEDHA_API_KEY"]).is_some() {
        CredSource::Env
    } else if file_load_key(&provider.base_url).is_some() {
        CredSource::CredentialsFile
    } else {
        CredSource::KeychainOrNone
    };
    Ok(Resolved {
        name: name.to_string(),
        provider: provider.clone(),
        credential: api_key,
        model_source: Source::Config,
        base_url_source: Source::Config,
        credential_source,
    })
}

/// First non-empty value among the given env var names.
fn first_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()))
}

/// True when keychain-first storage is selected: runtime `MEDHA_CRED_STORE`
/// wins, else the compile-time `MEDHA_DEFAULT_CRED_STORE` (set by a release
/// pipeline whose binaries are stably signed — keychain is prompt-free there),
/// else the silent credentials file.
fn prefer_keychain() -> bool {
    std::env::var("MEDHA_CRED_STORE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            option_env!("MEDHA_DEFAULT_CRED_STORE")
                .unwrap_or("file")
                .to_string()
        })
        .eq_ignore_ascii_case("keychain")
}

/// Per-process key cache. A key is fetched from the OS at most once per
/// endpoint per run — model switches after that are instant and prompt-free.
/// Only found keys are cached; a miss stays a cheap, silent lookup.
fn key_cache() -> &'static std::sync::Mutex<BTreeMap<String, String>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, String>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The default secrets file. Owner-only (0600); holds `[keys]` mapping
/// base URL → API key.
fn credentials_path() -> Result<PathBuf> {
    Ok(medha_home()?.join("credentials.toml"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    keys: BTreeMap<String, String>,
}

fn read_credentials_file(path: &std::path::Path) -> CredentialsFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// Write the credentials file with owner-only permissions. Created 0600 and
/// re-tightened on every write in case an earlier tool loosened it.
fn write_credentials_file(path: &std::path::Path, creds: &CredentialsFile) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = format!(
        "# medha API keys — owner-only (0600). Never commit or share this file.\n\
         # Set MEDHA_CRED_STORE=keychain to use the OS keychain instead.\n{}",
        toml::to_string_pretty(creds).context("serializing credentials")?
    );
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))
            .ok();
        f.write_all(text.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn file_store_key(base_url: &str, key: &str) -> Result<()> {
    let path = credentials_path()?;
    let mut creds = read_credentials_file(&path);
    creds.keys.insert(base_url.to_string(), key.to_string());
    write_credentials_file(&path, &creds)
}

fn file_load_key(base_url: &str) -> Option<String> {
    let path = credentials_path().ok()?;
    read_credentials_file(&path)
        .keys
        .get(base_url)
        .filter(|k| !k.is_empty())
        .cloned()
}

/// Read a key from the OS keychain. A missing item resolves silently; an item
/// written by a differently-signed binary may show one OS authorization prompt.
fn keychain_load_key(base_url: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, base_url)
        .ok()
        .and_then(|e| e.get_password().ok())
        .filter(|k| !k.is_empty())
}

/// Store a profile secret. It is deliberately kept out of `config.toml`;
/// callers only retain it long enough to hand it here. Default target is the
/// owner-only credentials file (silent everywhere, incl. rebuilt dev binaries
/// and headless hosts); keychain-first builds/sessions fall back to the file
/// when the keychain errors rather than losing the key.
pub(crate) fn store_key(base_url: &str, key: &str) -> Result<()> {
    let key = normalize_api_key(key);
    if key.is_empty() {
        anyhow::bail!("API key cannot be empty");
    }
    let stored = if prefer_keychain() {
        keyring::Entry::new(KEYRING_SERVICE, base_url)
            .and_then(|entry| entry.set_password(&key))
            .map_err(anyhow::Error::from)
            .or_else(|keychain_err| {
                file_store_key(base_url, &key).map_err(|file_err| {
                    anyhow::anyhow!(
                        "could not store the key in the OS keychain ({keychain_err}) or \
                         ~/.medha/credentials.toml ({file_err}); set MEDHA_API_KEY instead"
                    )
                })
            })
    } else {
        file_store_key(base_url, &key)
    };
    stored?;
    if let Ok(mut cache) = key_cache().lock() {
        cache.insert(base_url.to_string(), key);
    }
    Ok(())
}

/// Credential-store id holding an MCP server's API key.
fn mcp_key_id(id: &str) -> String {
    format!("mcp://{id}")
}

/// Credential-store id holding a remote server's OAuth credentials.
fn mcp_oauth_id(id: &str) -> String {
    format!("mcp-oauth://{id}")
}

/// Keychain-backed persistence for remote OAuth credentials, so an authorized
/// server reconnects at launch instead of demanding a browser every time.
#[derive(Debug)]
pub struct McpTokens;

impl mcp::TokenStore for McpTokens {
    fn load(&self, server: &str) -> Option<String> {
        load_key(&mcp_oauth_id(server))
    }

    fn save(&self, server: &str, blob: &str) {
        if let Err(error) = store_key(&mcp_oauth_id(server), blob) {
            tracing::warn!(target: "medha_mcp", server, %error, "could not persist MCP OAuth credentials");
        }
    }

    fn clear(&self, server: &str) {
        purge_credential(&mcp_oauth_id(server));
    }
}

/// Best-effort removal of one credential from every layer (cache, file, keychain).
fn purge_credential(cred_id: &str) {
    if let Ok(mut cache) = key_cache().lock() {
        cache.remove(cred_id);
    }
    if let Ok(path) = credentials_path() {
        let mut creds = read_credentials_file(&path);
        if creds.keys.remove(cred_id).is_some() {
            let _ = write_credentials_file(&path, &creds);
        }
    }
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, cred_id) {
        let _ = entry.delete_credential();
    }
}

/// Store an MCP server's API key in the credential store — never in `config.toml`
/// or `medha.lock`. Empty key is a no-op (server needs no secret).
pub fn store_mcp_key(id: &str, key: &str) -> Result<()> {
    if key.trim().is_empty() {
        return Ok(());
    }
    store_key(&mcp_key_id(id), key)
}

/// True when a stored key exists for this server (display only — never returns
/// the secret itself).
pub fn mcp_key_present(id: &str) -> bool {
    load_key(&mcp_key_id(id)).is_some()
}

/// Best-effort purge of an MCP server's key from every layer (cache, credentials
/// file, keychain) — so `mcp remove` leaves no orphaned secret behind.
pub fn delete_mcp_key(id: &str) {
    purge_credential(&mcp_key_id(id));
    // Remote servers also hold OAuth credentials; removing the server drops both.
    purge_credential(&mcp_oauth_id(id));
}

/// Resolve a user-scoped server into a connectable definition, substituting the
/// literal `${key}` in the command/env with the stored secret at spawn time.
pub fn resolve_mcp_server(id: &str, server: &McpServer) -> mcp::ServerConfig {
    let key = load_key(&mcp_key_id(id));
    let sub = |value: &str| match &key {
        Some(secret) => value.replace("${key}", secret),
        None => value.to_string(),
    };
    let transport = if server.url.is_empty() {
        mcp::Transport::Stdio {
            command: server.command.iter().map(|arg| sub(arg)).collect(),
            env: server
                .env
                .iter()
                .map(|(k, v)| (k.clone(), sub(v)))
                .collect(),
        }
    } else {
        mcp::Transport::Remote {
            url: server.url.clone(),
            auth: match server.auth.as_str() {
                "oauth" => mcp::RemoteAuth::OAuth,
                // A bearer server without a stored token is a config mistake, not
                // a secret-free server: keep it explicit rather than silently
                // connecting unauthenticated.
                "bearer" => mcp::RemoteAuth::Bearer(key.clone().unwrap_or_default()),
                "none" => mcp::RemoteAuth::None,
                // Unset: let the server say what it wants on first connect.
                _ => mcp::RemoteAuth::Auto,
            },
        }
    };
    mcp::ServerConfig {
        id: id.to_string(),
        transport,
        requires_approval: server.trust != "trusted",
        disabled: server.disabled,
        allow_network: server.network,
        tools: mcp::ToolFilter {
            allow: server.allow_tools.clone(),
            deny: server.deny_tools.clone(),
        },
        parallel_calls: server.parallel_calls,
    }
}

/// Users sometimes paste the entire Authorization value. Reqwest adds the
/// scheme itself, so persist only the token and avoid `Bearer Bearer …`.
fn normalize_api_key(value: &str) -> String {
    let value = value.trim();
    if value.eq_ignore_ascii_case("bearer") {
        return String::new();
    }
    match value.split_once(char::is_whitespace) {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => token.trim().to_string(),
        _ => value.to_string(),
    }
}

/// Look a key up through the layered store: process cache → credentials file
/// → keychain (order inverted under keychain-first builds/sessions). Callers
/// handle env-var precedence before reaching here.
///
/// A file miss followed by a keychain hit is a key stored by an older
/// keychain-first build: it is migrated into the credentials file, so the OS
/// prompt that read may have cost is paid at most once, ever.
fn load_key(base_url: &str) -> Option<String> {
    if let Ok(cache) = key_cache().lock() {
        if let Some(hit) = cache.get(base_url) {
            return Some(hit.clone());
        }
    }
    let found = if prefer_keychain() {
        keychain_load_key(base_url).or_else(|| file_load_key(base_url))
    } else {
        file_load_key(base_url).or_else(|| {
            let legacy = keychain_load_key(base_url);
            if let Some(key) = &legacy {
                let _ = file_store_key(base_url, key);
            }
            legacy
        })
    };
    if let Some(key) = &found {
        if let Ok(mut cache) = key_cache().lock() {
            cache.insert(base_url.to_string(), key.clone());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn encodes_workspace_path_claude_code_style() {
        // Leading separator and every '/' become '-'; existing hyphens survive.
        assert_eq!(
            encode_workspace(Path::new("/Users/x/proj")),
            "-Users-x-proj"
        );
        assert_eq!(
            encode_workspace(Path::new("/Users/reeturajharsh/Personal/files/medha")),
            "-Users-reeturajharsh-Personal-files-medha"
        );
        assert_eq!(encode_workspace(Path::new("/a/my-repo")), "-a-my-repo");
    }

    #[test]
    fn state_dir_is_per_workspace_under_home_projects() {
        let home = Path::new("/home/u/.medha");
        let a = state_dir_in(home, Path::new("/w/one"));
        let b = state_dir_in(home, Path::new("/w/two"));
        assert_eq!(a, Path::new("/home/u/.medha/projects/-w-one"));
        assert_ne!(a, b, "different workspaces get different state dirs");
        assert!(
            a.starts_with(home),
            "state stays under MEDHA home, not the workspace"
        );
    }

    #[test]
    fn user_skills_share_the_medha_home_root() {
        let home = Path::new("/home/u/custom-medha");
        assert_eq!(
            user_skills_dir_in(home),
            Path::new("/home/u/custom-medha/skills")
        );
    }

    fn provider(model: &str) -> ProviderConfig {
        ProviderConfig {
            protocol: kernel::Protocol::OpenAiChat,
            base_url: format!("http://{model}.example/v1"),
            model: model.into(),
            auth: providers::AuthKind::None,
            headers: BTreeMap::new(),
            max_ctx: Some(16_384),
            max_output_tokens: None,
            token_counter: providers::openai_compat::OpenAiTokenCounter::None,
            token_accounting: kernel::TokenAccountingMode::Adaptive,
            reasoning: kernel::ReasoningSupport::Unknown,
        }
    }

    #[test]
    fn first_saved_model_becomes_default_and_default_can_move() {
        let mut cfg = Config::default();
        assert!(
            cfg.selected_model().is_none(),
            "no models → first-run setup"
        );

        cfg.add_model("fast-local".into(), provider("fast"), false)
            .unwrap();
        assert_eq!(cfg.selected_model().unwrap().0, "fast-local");

        cfg.add_model("big".into(), provider("big"), false).unwrap();
        assert_eq!(cfg.selected_model().unwrap().0, "fast-local");

        cfg.set_default_model("big").unwrap();
        assert_eq!(cfg.selected_model().unwrap().0, "big");
        assert!(
            cfg.model_profiles()
                .iter()
                .any(|p| p.name == "big" && p.is_default)
        );
        assert_eq!(cfg.model_profiles().len(), 2);
    }

    #[test]
    fn profile_names_are_safe_and_unambiguous() {
        let mut cfg = Config::default();
        assert!(
            cfg.add_model("my-model-2".into(), provider("m"), false)
                .is_ok()
        );
        assert!(
            cfg.add_model("My Model".into(), provider("m"), false)
                .is_err()
        );
        assert!(
            cfg.add_model("bad--name".into(), provider("m"), false)
                .is_err()
        );
        assert!(
            cfg.add_model("my-model-2".into(), provider("m"), false)
                .is_err()
        );
    }

    #[test]
    fn removing_the_default_promotes_the_next_saved_model() {
        let mut cfg = Config::default();
        cfg.add_model("keeper".into(), provider("keep"), false)
            .unwrap();
        cfg.add_model("temporary".into(), provider("tmp"), true)
            .unwrap();
        assert_eq!(cfg.selected_model().unwrap().0, "temporary");

        assert_eq!(cfg.remove_model("temporary").unwrap().model, "tmp");
        assert_eq!(cfg.selected_model().unwrap().0, "keeper");

        cfg.remove_model("keeper").unwrap();
        assert!(cfg.selected_model().is_none(), "empty again → setup");
        assert!(cfg.remove_model("keeper").is_err(), "already gone");
    }

    #[test]
    fn a_stale_default_falls_back_to_the_first_saved_model() {
        let mut cfg = Config::default();
        cfg.add_model("real".into(), provider("real"), false)
            .unwrap();
        cfg.default_model = Some("deleted-by-hand".into());
        assert_eq!(cfg.selected_model().unwrap().0, "real");
    }

    #[test]
    fn wizard_era_provider_block_migrates_to_a_named_removable_profile() {
        let legacy = r#"
            [provider]
            base_url = "https://gw.example/v1"
            model = "Qwen/Qwen3.5-397B-A17B"
            needs_key = true
            max_ctx = 250000
        "#;
        let mut cfg: Config = toml::from_str(legacy).unwrap();
        assert!(migrate_legacy_provider(&mut cfg), "migration must trigger");

        let (name, p) = cfg.selected_model().expect("migrated profile selected");
        assert_eq!(name, "qwen3-5-397b-a17b");
        assert_eq!(p.model, "Qwen/Qwen3.5-397B-A17B");
        assert_eq!(p.auth, providers::AuthKind::Bearer);
        assert_eq!(cfg.default_model.as_deref(), Some("qwen3-5-397b-a17b"));
        // The migrated profile is ordinary: removable like any other.
        assert!(cfg.remove_model("qwen3-5-397b-a17b").is_ok());
        // Serialization never writes [provider] again.
        assert!(!toml::to_string(&cfg).unwrap().contains("[provider]"));
        // Second load is a no-op.
        assert!(!migrate_legacy_provider(&mut cfg));
    }

    #[test]
    fn migration_keeps_an_existing_startup_default_and_dedupes() {
        let legacy = r#"
            default_model = "nemotron"
            [provider]
            base_url = "https://gw.example/v1"
            model = "Qwen/Qwen3.5-397B-A17B"
            needs_key = true
            [models.nemotron]
            base_url = "https://gw.example/v1"
            model = "nvidia/Nemotron"
            needs_key = true
        "#;
        let mut cfg: Config = toml::from_str(legacy).unwrap();
        assert!(migrate_legacy_provider(&mut cfg));
        assert_eq!(cfg.default_model.as_deref(), Some("nemotron"));
        assert_eq!(cfg.selected_model().unwrap().0, "nemotron");
        assert!(cfg.models.contains_key("qwen3-5-397b-a17b"));

        // An identical connection already saved under a name → nothing added.
        let dup = r#"
            [provider]
            base_url = "http://same.example/v1"
            model = "same"
            [models.mine]
            base_url = "http://same.example/v1"
            model = "same"
        "#;
        let mut cfg: Config = toml::from_str(dup).unwrap();
        assert!(migrate_legacy_provider(&mut cfg));
        assert_eq!(cfg.models.len(), 1, "duplicate connection not re-added");
    }

    #[test]
    fn credentials_file_round_trips_and_stays_owner_only() {
        let dir = std::env::temp_dir().join(format!("medha-creds-{}", ulid::Ulid::new()));
        let path = dir.join("credentials.toml");
        assert!(read_credentials_file(&path).keys.is_empty());

        let mut creds = CredentialsFile::default();
        creds
            .keys
            .insert("http://one.example/v1".into(), "sk-one".into());
        write_credentials_file(&path, &creds).unwrap();

        // A second key must not clobber the first.
        let mut creds = read_credentials_file(&path);
        creds
            .keys
            .insert("http://two.example/v1".into(), "sk-two".into());
        write_credentials_file(&path, &creds).unwrap();

        let creds = read_credentials_file(&path);
        assert_eq!(creds.keys.get("http://one.example/v1").unwrap(), "sk-one");
        assert_eq!(creds.keys.get("http://two.example/v1").unwrap(), "sk-two");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credentials file must be owner-only");

            // A loosened file is re-tightened on the next write.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            write_credentials_file(&path, &read_credentials_file(&path)).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "0600 must be restored on write");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn api_key_normalization_accepts_token_or_full_bearer_value() {
        assert_eq!(normalize_api_key("secret"), "secret");
        assert_eq!(normalize_api_key("  Bearer secret  "), "secret");
        assert_eq!(normalize_api_key("bearer secret"), "secret");
        assert_eq!(normalize_api_key("Bearer   "), "");
    }

    #[test]
    fn provider_environment_values_are_validated_instead_of_silently_ignored() {
        assert_eq!(
            parse_protocol("open-ai-chat").unwrap(),
            kernel::Protocol::OpenAiChat
        );
        assert!(parse_protocol("made-up-protocol").is_err());
        assert_eq!(
            parse_auth("x-api-key").unwrap(),
            providers::AuthKind::XApiKey
        );
        assert!(parse_auth("magic-auth").is_err());
        assert_eq!(
            parse_headers(r#"{"X-Provider-Version":"2026-01-01"}"#)
                .unwrap()
                .get("X-Provider-Version")
                .map(String::as_str),
            Some("2026-01-01")
        );
        assert!(parse_headers("not-json").is_err());
        assert_eq!(
            parse_token_counter("vllm").unwrap(),
            providers::openai_compat::OpenAiTokenCounter::Vllm
        );
        assert!(parse_token_counter("guess").is_err());
        assert_eq!(
            parse_token_accounting("strict").unwrap(),
            kernel::TokenAccountingMode::Strict
        );
        assert!(parse_token_accounting("exact-ish").is_err());
        assert_eq!(
            parse_reasoning_support("effort").unwrap(),
            kernel::ReasoningSupport::Effort
        );
        assert!(parse_reasoning_support("all-controls").is_err());
        assert!(parse_positive_u32("MEDHA_MAX_CTX", "0").is_err());
        assert!(parse_positive_u64("MEDHA_MAX_OUTPUT_TOKENS", "0").is_err());
    }

    #[test]
    fn set_search_records_provider_and_scopes_searxng_url() {
        let mut cfg = Config::default();
        // Unset → the effective default is DuckDuckGo.
        assert_eq!(cfg.search_provider(), tools::SearchProvider::DuckDuckGo);

        // A SearXNG choice keeps its URL.
        cfg.set_search(
            tools::SearchProvider::Searxng,
            Some("https://searx.example".into()),
        );
        assert_eq!(cfg.search_provider(), tools::SearchProvider::Searxng);
        assert_eq!(
            cfg.search.searxng_url.as_deref(),
            Some("https://searx.example")
        );

        // Switching to a non-SearXNG provider must not leave a stale URL behind.
        cfg.set_search(
            tools::SearchProvider::Tavily,
            Some("https://leftover".into()),
        );
        assert_eq!(cfg.search_provider(), tools::SearchProvider::Tavily);
        assert_eq!(cfg.search.searxng_url, None);
    }

    #[test]
    fn search_provider_id_round_trips_for_config_serialization() {
        for p in [
            tools::SearchProvider::DuckDuckGo,
            tools::SearchProvider::Tavily,
            tools::SearchProvider::Brave,
            tools::SearchProvider::Searxng,
        ] {
            assert_eq!(tools::SearchProvider::from_id(p.as_str()), p);
        }
        // Unknown ids degrade to the safe default rather than erroring.
        assert_eq!(
            tools::SearchProvider::from_id("nonsense"),
            tools::SearchProvider::DuckDuckGo
        );
    }
}
