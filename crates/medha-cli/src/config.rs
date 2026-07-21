//! Provider configuration. The CLI *resolves* config; it never defines it.
//!
//! Resolution order (highest wins):
//!   CLI flag  >  env override  >  ~/.medha/config.toml  >  TUI first-run model setup
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

/// What the kernel actually runs with, after resolution + secret lookup.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub name: String,
    pub provider: providers::ProviderProfile,
    /// Resolved from environment/credential storage; never serialized.
    pub credential: String,
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
/// Env var names accept MEDHA_* plus the common `OPENAI_COMPATIBLE_*` / `OPENAI_*`
/// spellings, so an existing environment works without renaming anything.
pub fn resolve(
    cfg: Option<&Config>,
    flag_base_url: Option<String>,
    flag_model: Option<String>,
) -> Result<Option<Resolved>> {
    // An explicit endpoint/model is a one-session override, even if only one
    // half of the connection came from a saved profile. Do not label that as a
    // persisted profile in the TUI — selecting it again must be unambiguous.
    let has_override = flag_base_url.is_some()
        || flag_model.is_some()
        || first_env(&[
            "MEDHA_BASE_URL",
            "OPENAI_COMPATIBLE_BASE_URL",
            "OPENAI_BASE_URL",
        ])
        .is_some()
        || first_env(&["MEDHA_MODEL", "OPENAI_COMPATIBLE_MODEL", "OPENAI_MODEL"]).is_some();
    let (profile, configured) = cfg.and_then(|c| c.selected_model()).unzip();
    let base_url = flag_base_url
        .or_else(|| {
            first_env(&[
                "MEDHA_BASE_URL",
                "OPENAI_COMPATIBLE_BASE_URL",
                "OPENAI_BASE_URL",
            ])
        })
        .or_else(|| configured.map(|p| p.base_url.clone()));
    let model = flag_model
        .or_else(|| first_env(&["MEDHA_MODEL", "OPENAI_COMPATIBLE_MODEL", "OPENAI_MODEL"]))
        .or_else(|| configured.map(|p| p.model.clone()));
    let (Some(base_url), Some(model)) = (base_url, model) else {
        return Ok(None);
    };

    // Environment first: besides honoring documented precedence, this avoids
    // touching macOS Keychain at all during `cargo run` development when a
    // `.env` credential already exists.
    let env_key = first_env(&[
        "MEDHA_API_KEY",
        "OPENAI_COMPATIBLE_API_KEY",
        "OPENAI_API_KEY",
    ]);
    let api_key = normalize_api_key(&env_key.or_else(|| load_key(&base_url)).unwrap_or_default());

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
    }))
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
    // merely reading a keychain item can show an authorization dialog.
    let api_key = normalize_api_key(
        &first_env(&[
            "MEDHA_API_KEY",
            "OPENAI_COMPATIBLE_API_KEY",
            "OPENAI_API_KEY",
        ])
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
    Ok(Resolved {
        name: name.to_string(),
        provider: provider.clone(),
        credential: api_key,
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
