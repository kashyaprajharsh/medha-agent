//! Provider configuration and credential storage.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const KEYRING_SERVICE: &str = "medha";

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

pub(crate) fn provider_presets() -> &'static [(&'static str, &'static str)] {
    PRESETS
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Load-only legacy connection, migrated into `models`.
    #[serde(default, skip_serializing)]
    provider: Option<ProviderConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub search: SearchConfig,
    /// User-scoped, secret-free MCP definitions.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServer>,
}

impl McpServer {
    pub fn target(&self) -> String {
        if self.url.is_empty() {
            self.command.join(" ")
        } else {
            self.url.clone()
        }
    }
}

/// Secret-free user MCP definition; `${key}` resolves only in explicit env.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpServer {
    /// Local stdio server. Mutually exclusive with `url`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Mutually exclusive with `command`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Credential scheme for `url`: empty, `bearer`, or `oauth`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub auth: String,
    /// Values may reference `${key}`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `workspace` (default) requires approval to start; `trusted` auto-connects.
    #[serde(default)]
    pub trust: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Exact or `prefix*` tool filters; deny applies after allow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_tools: Vec<String>,
    /// Per-server network override; unset falls back to the host `[mcp]` default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parallel_calls: bool,
}

pub const MCP_SECRET_FLAGS: &[&str] = &["--key", "--bearer", "--token", "--password"];

/// Carries `NAME=VALUE`; the value is how MCP servers are handed a token.
pub const MCP_ENV_FLAG: &str = "--env";

pub struct ParsedMcpAdd {
    pub id: String,
    pub server: McpServer,
    pub key: Option<String>,
}

pub fn parse_mcp_add_args<I, S>(args: I) -> Result<ParsedMcpAdd>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    fn value(
        args: &[String],
        index: &mut usize,
        joined: Option<&str>,
        flag: &str,
    ) -> Result<String> {
        let value = match joined {
            Some(value) => value.to_string(),
            None => {
                *index += 1;
                args.get(*index)
                    .filter(|value| !value.starts_with("--"))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))?
            }
        };
        if value.is_empty() {
            anyhow::bail!("{flag} needs a non-empty value");
        }
        Ok(value)
    }

    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    let mut id = None;
    let mut trust = "workspace".to_string();
    let mut key = None;
    let mut url = String::new();
    let mut auth = String::new();
    let mut env = BTreeMap::new();
    let mut allow_tools = Vec::new();
    let mut deny_tools = Vec::new();
    let mut network = None;
    let mut parallel_calls = false;
    let mut command = Vec::new();

    let mut index = 0;
    while index < args.len() {
        let raw = &args[index];
        if raw == "--" {
            command.extend(args[index + 1..].iter().cloned());
            break;
        }
        let (flag, joined) = raw
            .split_once('=')
            .filter(|(flag, _)| flag.starts_with("--"))
            .map_or((raw.as_str(), None), |(flag, value)| (flag, Some(value)));
        match flag {
            "--key" => key = Some(value(&args, &mut index, joined, flag)?),
            "--url" => url = value(&args, &mut index, joined, flag)?,
            "--bearer" => {
                auth = "bearer".into();
                key = Some(value(&args, &mut index, joined, flag)?);
            }
            "--oauth" => {
                if joined.is_some() {
                    anyhow::bail!("--oauth does not take a value");
                }
                auth = "oauth".into();
            }
            "--trust" => trust = value(&args, &mut index, joined, flag)?,
            "--env" => {
                let pair = value(&args, &mut index, joined, flag)?;
                let (name, value) = pair
                    .split_once('=')
                    .filter(|(name, _)| !name.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("--env needs NAME=VALUE"))?;
                env.insert(name.to_string(), value.to_string());
            }
            "--allow-tool" => allow_tools.push(value(&args, &mut index, joined, flag)?),
            "--deny-tool" => deny_tools.push(value(&args, &mut index, joined, flag)?),
            "--no-network" => {
                if joined.is_some() {
                    anyhow::bail!("--no-network does not take a value");
                }
                network = Some(false);
            }
            "--parallel" => {
                if joined.is_some() {
                    anyhow::bail!("--parallel does not take a value");
                }
                parallel_calls = true;
            }
            unknown if unknown.starts_with("--") => {
                anyhow::bail!("unknown MCP option '{unknown}' (put server arguments after --)");
            }
            _ if mcp::is_url(raw) => url = raw.clone(),
            _ if id.is_none() => id = Some(raw.clone()),
            _ => command.push(raw.clone()),
        }
        index += 1;
    }

    let id = id
        .or_else(|| mcp::id_from_url(&url))
        .ok_or_else(|| anyhow::anyhow!("MCP server needs an id or URL"))?;
    if url.is_empty() == command.is_empty() {
        anyhow::bail!("server '{id}' needs exactly one of --url <https://…> or -- <command>");
    }
    if !url.is_empty() {
        mcp::validate_remote_url(&url)?;
    }
    if command.iter().any(|arg| arg.contains("${key}")) {
        anyhow::bail!("server '{id}' puts `${{key}}` in argv; use --env NAME=${{key}} instead");
    }
    Ok(ParsedMcpAdd {
        id,
        server: McpServer {
            command,
            url,
            auth,
            env,
            trust,
            disabled: false,
            allow_tools,
            deny_tools,
            network,
            parallel_calls,
        },
        key,
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Provider id; `None` retains legacy environment auto-detection.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub searxng_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub identity: Option<String>,
}

pub type ProviderConfig = providers::ProviderProfile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProfile {
    pub name: String,
    pub provider: ProviderConfig,
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Flag,
    Env,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredSource {
    Env,
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

#[derive(Debug, Clone)]
pub struct Resolved {
    pub name: String,
    pub provider: providers::ProviderProfile,
    /// Resolved from environment/credential storage; never serialized.
    pub credential: String,
    pub model_source: Source,
    pub base_url_source: Source,
    pub credential_source: CredSource,
}

impl Config {
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
        if make_default || self.models.is_empty() {
            self.default_model = Some(name.clone());
        }
        self.models.insert(name, provider);
        Ok(())
    }

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

    pub fn search_provider(&self) -> tools::SearchProvider {
        self.search
            .provider
            .as_deref()
            .map(tools::SearchProvider::from_id)
            .unwrap_or_default()
    }

    pub fn set_search(&mut self, provider: tools::SearchProvider, searxng_url: Option<String>) {
        self.search.provider = Some(provider.as_str().to_string());
        self.search.searxng_url = match provider {
            tools::SearchProvider::Searxng => searxng_url,
            _ => None,
        };
    }
}

pub(crate) fn search_cred_id(provider: tools::SearchProvider) -> Option<&'static str> {
    match provider {
        tools::SearchProvider::Tavily => Some("search://tavily"),
        tools::SearchProvider::Brave => Some("search://brave"),
        tools::SearchProvider::DuckDuckGo | tools::SearchProvider::Searxng => None,
    }
}

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

pub(crate) fn derive_profile_name(cfg: &Config, model_id: &str) -> String {
    unique_profile_name(&cfg.models, &profile_name_from_model(model_id))
}

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

pub fn user_skills_dir() -> Result<PathBuf> {
    Ok(user_skills_dir_in(&medha_home()?))
}

fn user_skills_dir_in(home: &std::path::Path) -> PathBuf {
    home.join("skills")
}

pub fn user_taps_path() -> Result<PathBuf> {
    Ok(user_skills_dir()?.join("taps.toml"))
}

pub fn skills_lock_path() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join("medha-skills.lock"))
}

pub fn state_dir(workspace: &std::path::Path) -> Result<PathBuf> {
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("canonicalizing workspace {}", workspace.display()))?;
    let home = medha_home()?;
    let projects = home.join("projects");
    std::fs::create_dir_all(&projects)
        .with_context(|| format!("creating {}", projects.display()))?;

    // Legacy slugs can collide, so never import their trust state automatically.
    let legacy = home
        .join("projects")
        .join(readable_workspace_slug(&workspace));
    let current = state_dir_in(&home, &workspace);
    if legacy.exists() && !current.exists() && legacy != current {
        eprintln!(
            "warning: legacy workspace state at {} has an ambiguous path identity and was not \
             imported automatically; re-approve external paths and recover non-trust data manually",
            legacy.display()
        );
    }

    select_state_dir(&home, &workspace)
}

fn state_dir_in(home: &std::path::Path, workspace: &std::path::Path) -> PathBuf {
    home.join("projects").join(encode_workspace(workspace))
}

const WORKSPACE_ID_MARKER: &str = ".medha-workspace-id-v2";

fn select_state_dir(home: &std::path::Path, workspace: &std::path::Path) -> Result<PathBuf> {
    let projects = home.join("projects");
    std::fs::create_dir_all(&projects)
        .with_context(|| format!("creating {}", projects.display()))?;
    let candidate = projects.join(encode_workspace(workspace));
    let identity = workspace_path_identity(workspace);
    match std::fs::create_dir(&candidate) {
        Ok(()) => {
            let marker = candidate.join(WORKSPACE_ID_MARKER);
            if let Err(error) = std::fs::write(&marker, &identity) {
                let _ = std::fs::remove_dir(&candidate);
                return Err(error).with_context(|| format!("writing {}", marker.display()));
            }
            Ok(candidate)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let marker = candidate.join(WORKSPACE_ID_MARKER);
            // A concurrent first launch may still be writing the marker.
            let attempts = if cfg!(test) { 2 } else { 50 };
            for attempt in 0..attempts {
                match std::fs::read_to_string(&marker) {
                    Ok(stored) if stored == identity => return Ok(candidate),
                    Ok(_) => break,
                    Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => {
                        if attempt + 1 < attempts {
                            std::thread::sleep(std::time::Duration::from_millis(if cfg!(test) {
                                1
                            } else {
                                20
                            }));
                        }
                    }
                    Err(_) => break,
                }
            }
            anyhow::bail!(
                "refusing unbound or mismatched workspace state at {}; move it aside after \
                 inspection, then restart so trust/history cannot cross projects",
                candidate.display()
            )
        }
        Err(error) => Err(error).with_context(|| format!("creating {}", candidate.display())),
    }
}

fn encode_workspace(p: &std::path::Path) -> String {
    let readable = readable_workspace_slug(p);
    // Leave room below common 255-byte component limits.
    let mut prefix = String::with_capacity(readable.len().min(96));
    for character in readable.chars() {
        if prefix.len() + character.len_utf8() > 96 {
            break;
        }
        prefix.push(character);
    }
    format!("{prefix}--{}", workspace_path_fingerprint(p))
}

/// Windows verbatim prefixes are removed because `?` is invalid in filenames.
fn readable_workspace_slug(p: &std::path::Path) -> String {
    let raw = p.to_string_lossy();
    let path = raw
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| raw.strip_prefix(r"\\?\"))
        .unwrap_or(&raw);
    path.chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect()
}

fn workspace_path_fingerprint(p: &std::path::Path) -> String {
    workspace_path_digest(p)[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn workspace_path_identity(p: &std::path::Path) -> String {
    let digest: String = workspace_path_digest(p)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("medha-workspace-v2\nsha256={digest}\n")
}

fn workspace_path_digest(p: &std::path::Path) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(p.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in p.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(p.as_os_str().to_string_lossy().as_bytes());

    hasher.finalize().into()
}

/// Narrow a config written before MCP `env` values counted as secret-bearing.
#[cfg(unix)]
fn tighten_config_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o700));
    }
}

#[cfg(not(unix))]
fn tighten_config_permissions(_path: &std::path::Path) {}

pub fn load() -> Result<Option<Config>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    tighten_config_permissions(&path);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut cfg: Config = toml::from_str(&text).context("parsing config.toml")?;
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
    with_config_lock(&path, || write_config_file(&path, &text))
}

fn write_config_file(path: &std::path::Path, text: &str) -> Result<()> {
    let temporary = path.with_extension(format!("tmp{}-{}", std::process::id(), ulid::Ulid::new()));
    let write_result = (|| {
        use std::io::Write as _;
        // 0600: MCP `env` values may carry an inlined token.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("opening {}", temporary.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
        drop(file);
        atomic_replace_file(&temporary, path)
            .with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .with_context(|| format!("syncing {}", parent.display()))?;
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result
}

/// Serialize config writes using a stable lock beside the replaced data file.
fn with_config_lock<T>(path: &std::path::Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(std::path::PathBuf::from(lock_path))
        .with_context(|| format!("opening config lock beside {}", path.display()))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock
        .write()
        .with_context(|| format!("locking config beside {}", path.display()))?;
    operation()
}

/// Only `MEDHA_*` environment variables may override saved settings.
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
    let has_override = flag_base_url.is_some()
        || flag_model.is_some()
        || first_env(&["MEDHA_BASE_URL"]).is_some()
        || first_env(&["MEDHA_MODEL"]).is_some();
    let (profile, configured) = cfg.and_then(|c| c.selected_model()).unzip();
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

    // Avoid a macOS keychain prompt when the environment already supplies a key.
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
        reasoning_efforts: configured.and_then(|p| p.reasoning_efforts.clone()),
        chat_token_limit: configured.map(|p| p.chat_token_limit).unwrap_or_default(),
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

const IGNORED_ENV_PREFIXES: &[&str] = &[
    "OPENAI_",
    "GOOGLE_",
    "GEMINI_",
    "ANTHROPIC_",
    "AZURE_OPENAI_",
];

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

pub struct Check {
    pub health: Health,
    pub title: String,
    pub detail: String,
    pub auto_fixable: bool,
}

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
    pub project_lock: Option<String>,
    pub lock_executor: Option<String>,
    pub checks: Vec<Check>,
}

/// Build diagnostics without probing the keychain or external services.
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
            let executor = lockfile::MedhaLock::load(&p)
                .ok()
                .flatten()
                .and_then(|lock| lock.routing.executor);
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

fn diagnose_checks(
    cfg: Option<&Config>,
    resolved: &std::result::Result<Option<Resolved>, String>,
    ignored_env: &[String],
) -> Vec<Check> {
    let mut checks = Vec::new();

    match resolved {
        Ok(Some(r)) => {
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

    if let Some(cfg) = cfg
        && let Some(def) = &cfg.default_model
        && !cfg.models.contains_key(def)
    {
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

    let rank = |h: Health| match h {
        Health::Error => 0,
        Health::Warn => 1,
        Health::Ok => 2,
    };
    checks.sort_by_key(|c| rank(c.health));
    checks
}

pub fn apply_safe_fixes(cfg: &mut Config) -> Vec<String> {
    let mut fixed = Vec::new();

    if let Some(def) = cfg.default_model.clone()
        && !cfg.models.contains_key(&def)
    {
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

    fixed
}

impl Pulse {
    pub fn has_fixes(&self) -> bool {
        self.checks.iter().any(|c| c.auto_fixable)
    }

    fn verdict(&self) -> (&'static str, &'static str) {
        if self.checks.iter().any(|c| c.health == Health::Error) {
            ("✗", "needs attention")
        } else if self.checks.iter().any(|c| c.health == Health::Warn) {
            ("⚠", "ok with warnings")
        } else {
            ("✔", "healthy")
        }
    }

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

pub fn resolve_model(cfg: &Config, name: &str) -> Result<Resolved> {
    let provider = cfg
        .model_profile(name)
        .ok_or_else(|| anyhow::anyhow!("no saved model named '{name}'"))?;
    // Prefer MEDHA_API_KEY to avoid an unnecessary macOS keychain prompt.
    let api_key = normalize_api_key(
        &first_env(&["MEDHA_API_KEY"])
            .or_else(|| load_key(&provider.base_url))
            .unwrap_or_default(),
    );
    resolve_model_with_key(cfg, name, &api_key)
}

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

fn first_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()))
}

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

fn write_credentials_file(path: &std::path::Path, creds: &CredentialsFile) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = format!(
        "# medha API keys — owner-only (0600). Never commit or share this file.\n\
         # Set MEDHA_CRED_STORE=keychain to use the OS keychain instead.\n{}",
        toml::to_string_pretty(creds).context("serializing credentials")?
    );
    // A same-directory rename prevents crashes from publishing a partial file.
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("opening {}", temporary.display()))?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))
            .ok();
        f.write_all(text.as_bytes())
            .with_context(|| format!("writing {}", temporary.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
    }
    #[cfg(not(unix))]
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&temporary)
            .with_context(|| format!("opening {}", temporary.display()))?;
        f.write_all(text.as_bytes())
            .with_context(|| format!("writing {}", temporary.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
    }
    atomic_replace_file(&temporary, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace_file(source: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(windows)]
fn atomic_replace_file(source: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn credentials_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// The lock uses a stable sibling because publication replaces the data inode.
fn with_credentials_lock<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _process = credentials_lock()
        .lock()
        .map_err(|_| anyhow::anyhow!("credential lock is poisoned"))?;
    let dir = medha_home()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("credentials.lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    let mut lock = fd_lock::RwLock::new(file);
    let _cross_process = lock
        .write()
        .with_context(|| format!("locking {}", path.display()))?;
    operation()
}

fn write_key_file_locked(base_url: &str, key: &str) -> Result<()> {
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

fn keychain_load_key(base_url: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, base_url)
        .ok()
        .and_then(|e| e.get_password().ok())
        .filter(|k| !k.is_empty())
}

/// Secrets are stored outside `config.toml`.
pub(crate) fn store_key(base_url: &str, key: &str) -> Result<()> {
    let key = normalize_api_key(key);
    if key.is_empty() {
        anyhow::bail!("API key cannot be empty");
    }
    // Store and purge share one lock across both persistence layers.
    with_credentials_lock(|| {
        if prefer_keychain() {
            keyring::Entry::new(KEYRING_SERVICE, base_url)
                .and_then(|entry| entry.set_password(&key))
                .map_err(anyhow::Error::from)
                .or_else(|keychain_err| {
                    write_key_file_locked(base_url, &key).map_err(|file_err| {
                        anyhow::anyhow!(
                            "could not store the key in the OS keychain ({keychain_err}) or \
                             ~/.medha/credentials.toml ({file_err}); set MEDHA_API_KEY instead"
                        )
                    })
                })
        } else {
            write_key_file_locked(base_url, &key)
        }
    })
}

/// Credential id bound to both an MCP server name and destination.
fn mcp_key_id(id: &str, server: &McpServer) -> String {
    format!("mcp://{id}#{}", target_fingerprint(server))
}

fn mcp_oauth_id(id: &str, url: &str) -> String {
    format!("mcp-oauth://{id}#{}", fingerprint(url.as_bytes()))
}

/// Return a privacy-preserving digest of an MCP destination.
fn target_fingerprint(server: &McpServer) -> String {
    // Structured encoding binds stdio credentials to arguments and environment.
    let identity = if server.url.is_empty() {
        serde_json::to_vec(&("stdio", &server.command, &server.env))
    } else {
        serde_json::to_vec(&("remote", &server.url))
    }
    .unwrap_or_default();
    fingerprint(&identity)
}

fn fingerprint(value: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value);
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug)]
pub struct McpTokens;

impl mcp::TokenStore for McpTokens {
    fn load(&self, server: &str, url: &str) -> Option<String> {
        load_key(&mcp_oauth_id(server, url))
    }

    fn save(&self, server: &str, url: &str, blob: &str) {
        if let Err(error) = store_key(&mcp_oauth_id(server, url), blob) {
            tracing::warn!(target: "medha_mcp", server, %error, "could not persist MCP OAuth credentials");
        }
    }

    fn clear(&self, server: &str, url: &str) {
        purge_credential(&mcp_oauth_id(server, url));
    }
}

fn purge_credential(cred_id: &str) {
    // Keep removal atomic with concurrent loads and stores across both layers.
    let result = with_credentials_lock(|| {
        if let Ok(path) = credentials_path() {
            let mut creds = read_credentials_file(&path);
            if creds.keys.remove(cred_id).is_some() {
                write_credentials_file(&path, &creds)?;
            }
        }
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, cred_id) {
            let _ = entry.delete_credential();
        }
        Ok(())
    });
    if let Err(error) = result {
        tracing::warn!(target: "medha_credentials", credential = cred_id, %error, "could not purge credential");
    }
}

pub fn store_mcp_key(id: &str, server: &McpServer, key: &str) -> Result<()> {
    if key.trim().is_empty() {
        return Ok(());
    }
    store_key(&mcp_key_id(id, server), key)
}

pub fn mcp_key_present(id: &str, server: &McpServer) -> bool {
    load_key(&mcp_key_id(id, server)).is_some()
}

pub fn delete_mcp_key(id: &str, server: &McpServer) {
    purge_credential(&mcp_key_id(id, server));
    purge_credential(&mcp_oauth_id(id, &server.url));
}

pub fn resolve_mcp_server(id: &str, server: &McpServer) -> mcp::ServerConfig {
    let key = load_key(&mcp_key_id(id, server));
    // Resolve `${key}` at spawn time so previews and argv never contain the secret.
    let transport = if server.url.is_empty() {
        mcp::Transport::Stdio {
            command: server.command.clone(),
            env: server
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    } else {
        mcp::Transport::Remote {
            url: server.url.clone(),
            auth: match server.auth.as_str() {
                "oauth" => mcp::RemoteAuth::OAuth,
                "bearer" => mcp::RemoteAuth::Bearer(key.clone().unwrap_or_default()),
                "none" => mcp::RemoteAuth::None,
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
        secret: key,
    }
}

/// Normalize pasted bearer authorization values to their token.
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

/// Load a key from the configured credential layers, migrating legacy keychain data.
fn load_key(base_url: &str) -> Option<String> {
    // Avoid caching secrets that another Medha process may remove.
    with_credentials_lock(|| {
        Ok(if prefer_keychain() {
            keychain_load_key(base_url).or_else(|| file_load_key(base_url))
        } else {
            file_load_key(base_url).or_else(|| {
                let legacy = keychain_load_key(base_url);
                if let Some(key) = &legacy {
                    let _ = write_key_file_locked(base_url, key);
                }
                legacy
            })
        })
    })
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn encodes_workspace_path_with_readable_prefix_and_hash() {
        let encoded = encode_workspace(Path::new("/Users/x/proj"));
        assert!(encoded.starts_with("-Users-x-proj--"));
        assert_eq!(
            encoded.rsplit_once("--").unwrap().1.len(),
            32,
            "workspace identity uses 128 bits of SHA-256"
        );
        assert!(
            encode_workspace(Path::new("/a/my-repo")).starts_with("-a-my-repo--"),
            "existing hyphens remain readable"
        );
    }

    #[test]
    fn formerly_colliding_workspace_paths_have_distinct_state_identity() {
        let flat = Path::new("/w/a-b");
        let nested = Path::new("/w/a/b");
        assert_eq!(
            readable_workspace_slug(flat),
            readable_workspace_slug(nested),
            "documents the old ambiguous mapping"
        );
        assert_ne!(encode_workspace(flat), encode_workspace(nested));
        assert_ne!(
            state_dir_in(Path::new("/home/u/.medha"), flat),
            state_dir_in(Path::new("/home/u/.medha"), nested),
            "different workspaces must never share event state or trust.lock"
        );
    }

    #[test]
    fn colliding_legacy_workspace_cannot_inherit_machine_local_trust() {
        let home = std::env::temp_dir().join(format!("medha-state-id-{}", ulid::Ulid::new()));
        let legitimate = Path::new("/w/a-b");
        let other = Path::new("/w/a/b");
        let legitimate_state = state_dir_in(&home, legitimate);
        let other_state = state_dir_in(&home, other);
        std::fs::create_dir_all(&legitimate_state).unwrap();
        std::fs::write(
            legitimate_state.join("trust.lock"),
            "[[permissions.trusted_paths]]\npath = \"/\"\npermission = \"Read\"\n",
        )
        .unwrap();

        assert_ne!(legitimate_state, other_state);
        assert!(
            !other_state.join("trust.lock").exists(),
            "another canonical workspace identity must not inherit prompt-free grants"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn state_directory_is_bound_to_the_full_workspace_identity() {
        let home = std::env::temp_dir().join(format!("medha-state-marker-{}", ulid::Ulid::new()));
        let workspace = Path::new("/workspace/marker-test");

        let selected = select_state_dir(&home, workspace).unwrap();
        assert_eq!(
            std::fs::read_to_string(selected.join(WORKSPACE_ID_MARKER)).unwrap(),
            workspace_path_identity(workspace)
        );
        assert_eq!(
            select_state_dir(&home, workspace).unwrap(),
            selected,
            "a directory is reused only after its exact identity marker matches"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unmarked_hash_looking_legacy_directory_is_never_adopted() {
        let home = std::env::temp_dir().join(format!("medha-state-unmarked-{}", ulid::Ulid::new()));
        let workspace = Path::new("/workspace/unmarked-test");
        let unbound = state_dir_in(&home, workspace);
        std::fs::create_dir_all(&unbound).unwrap();
        std::fs::write(
            unbound.join("trust.lock"),
            "[[permissions.trusted_paths]]\npath = \"/\"\npermission = \"Read\"\n",
        )
        .unwrap();

        let error = select_state_dir(&home, workspace).unwrap_err();
        assert!(
            error.to_string().contains("refusing unbound or mismatched"),
            "{error:#}"
        );
        assert!(
            !unbound.join(WORKSPACE_ID_MARKER).exists(),
            "Medha must not claim an existing directory by adding its own marker"
        );
        assert_eq!(
            std::fs::read_to_string(unbound.join("trust.lock")).unwrap(),
            "[[permissions.trusted_paths]]\npath = \"/\"\npermission = \"Read\"\n"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn encodes_a_windows_verbatim_path_without_illegal_characters() {
        let enc = readable_workspace_slug(Path::new(r"\\?\C:\Users\ASUS"));
        assert_eq!(enc, "C--Users-ASUS");

        assert_eq!(readable_workspace_slug(Path::new(r"C:\Users\ASUS")), enc);

        assert_eq!(
            readable_workspace_slug(Path::new(r"\\?\UNC\server\share\proj")),
            "server-share-proj"
        );

        for p in [
            r"\\?\C:\Users\ASUS",
            r"\\?\UNC\server\share",
            r"C:\a\b",
            "/Users/x/proj",
        ] {
            let enc = readable_workspace_slug(Path::new(p));
            for bad in ['<', '>', ':', '"', '/', '\\', '|', '?', '*'] {
                assert!(
                    !enc.contains(bad),
                    "{p} encoded to {enc}, which keeps {bad}"
                );
            }
        }
    }

    #[test]
    fn unix_paths_are_untouched_by_the_verbatim_strip() {
        assert_eq!(
            readable_workspace_slug(Path::new("/home/u/proj")),
            "-home-u-proj"
        );
        assert_eq!(
            readable_workspace_slug(Path::new("/home/u/odd?name")),
            "-home-u-odd?name",
            "a legal Linux filename must not be rewritten"
        );
    }

    #[test]
    fn state_dir_is_per_workspace_under_home_projects() {
        let home = Path::new("/home/u/.medha");
        let a = state_dir_in(home, Path::new("/w/one"));
        let b = state_dir_in(home, Path::new("/w/two"));
        let projects = home.join("projects");
        let workspace_dir = a
            .file_name()
            .expect("workspace state directory has a final component")
            .to_string_lossy();
        assert_eq!(
            a.parent(),
            Some(projects.as_path()),
            "workspace state is directly under MEDHA_HOME/projects"
        );
        assert!(
            workspace_dir.starts_with("-w-one--"),
            "workspace state keeps its readable slug prefix: {}",
            a.display()
        );
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
            reasoning_efforts: None,
            chat_token_limit: Default::default(),
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
        assert!(cfg.remove_model("qwen3-5-397b-a17b").is_ok());
        assert!(!toml::to_string(&cfg).unwrap().contains("[provider]"));
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
    fn config_write_replaces_existing_content_atomically() {
        let dir = std::env::temp_dir().join(format!("medha-config-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "old = true\n").unwrap();

        write_config_file(&path, "new = true\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new = true\n");
        assert!(
            !std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
        );
        std::fs::remove_dir_all(&dir).ok();
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
        assert_eq!(cfg.search_provider(), tools::SearchProvider::DuckDuckGo);

        cfg.set_search(
            tools::SearchProvider::Searxng,
            Some("https://searx.example".into()),
        );
        assert_eq!(cfg.search_provider(), tools::SearchProvider::Searxng);
        assert_eq!(
            cfg.search.searxng_url.as_deref(),
            Some("https://searx.example")
        );

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
        assert_eq!(
            tools::SearchProvider::from_id("nonsense"),
            tools::SearchProvider::DuckDuckGo
        );
    }
}
