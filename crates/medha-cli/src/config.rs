//! Provider configuration. The CLI *resolves* config; it never defines it.
//!
//! Resolution order (highest wins):
//!   CLI flag  >  env override  >  ~/.medha/config.toml  >  first-run wizard
//!
//! Nothing is hardcoded as a product default — a value first comes to exist via
//! the interactive wizard. This file is the Phase-0 precursor to the
//! `medha.lock` `[routing]` table (§4.4). Secrets live in the OS keychain
//! (Vol 4 §1), never in the TOML (§9).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{IsTerminal, Write};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub provider: ProviderConfig,
    /// Agent-level settings (K1 identity, etc.). Optional so existing configs
    /// without an `[agent]` section still parse.
    #[serde(default)]
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Persona override for the K1 identity sheath. `None` → built-in default.
    #[serde(default)]
    pub identity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub model: String,
    /// Whether this endpoint needs an API key (local servers usually don't).
    /// The key itself is in the keychain, never here.
    #[serde(default)]
    pub needs_key: bool,
    /// Context window, captured at setup from `/v1/models` when the server
    /// reports it. `None` = unknown (never guessed); the budget falls back
    /// conservatively in that case.
    #[serde(default)]
    pub max_ctx: Option<u32>,
}

/// What the kernel actually runs with, after resolution + secret lookup.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub max_ctx: Option<u32>,
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
        .map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c })
        .collect()
}

pub fn load() -> Result<Option<Config>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).context("parsing config.toml")?;
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

/// Interactive first-run wizard. Returns the chosen config and (if the endpoint
/// needs one) stores the API key in the OS keychain. Async because it queries
/// the endpoint's `/v1/models` to offer a picker instead of asking the user to
/// type a model id blind.
pub async fn run_wizard() -> Result<Config> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "no configuration found and not running in a terminal.\n\
             Run `medha --setup` interactively, or pass --base-url/--model (and set MEDHA_API_KEY)."
        );
    }

    println!("\n  medha — first-run setup\n  ───────────────────────");
    println!("  Pick an OpenAI-compatible provider (or choose Custom):\n");
    for (i, (name, url)) in PRESETS.iter().enumerate() {
        println!("    {:>2}. {:<28} {}", i + 1, name, url);
    }
    println!("    {:>2}. Custom (enter your own base URL)", PRESETS.len() + 1);

    let choice = prompt(&format!("\n  Provider [1-{}]: ", PRESETS.len() + 1))?;
    let idx: usize = choice.trim().parse().unwrap_or(0);

    let base_url = if idx >= 1 && idx <= PRESETS.len() {
        PRESETS[idx - 1].1.to_string()
    } else {
        let url = prompt("  Base URL (e.g. http://localhost:8000/v1): ")?;
        let url = url.trim().to_string();
        if url.is_empty() {
            anyhow::bail!("a base URL is required");
        }
        url
    };

    // Key first — some endpoints require it even to list models.
    let key = prompt("  API key (leave blank for local servers): ")?
        .trim()
        .to_string();
    let needs_key = !key.is_empty();

    // Auto-discover models from the endpoint; fall back to manual entry.
    let (model, mut max_ctx) = select_model(&base_url, &key).await?;

    // Some gateways strip `max_model_len` from /v1/models even though the
    // deployment has a real limit (e.g. a 250k-served model). If the server
    // didn't report it, ask once and save it with the provider config — it's a
    // property of THIS endpoint, so it lives here (not in the portable medha.lock).
    if max_ctx.is_none() {
        let ans = prompt(&format!(
            "  Context window for '{model}' wasn't reported by the server. \
             Enter it in tokens (e.g. 250000), or leave blank to disable compaction: "
        ))?;
        max_ctx = ans.trim().parse::<u32>().ok();
    }

    if needs_key {
        store_key(&base_url, &key)?;
    }

    let cfg = Config {
        provider: ProviderConfig { base_url, model, needs_key, max_ctx },
        agent: AgentConfig::default(),
    };
    save(&cfg)?;
    match max_ctx {
        Some(n) => println!("  Context window: {n} tokens."),
        None => println!("  Context window: unknown — compaction disabled (set MEDHA_MAX_CTX later to enable)."),
    }
    println!("\n  Saved to {}\n", config_path()?.display());
    Ok(cfg)
}

/// Query `/v1/models` and let the user pick; fall back to manual entry if the
/// endpoint can't be reached or doesn't implement model discovery. Returns the
/// chosen model id and its context window if the server reported one.
async fn select_model(base_url: &str, api_key: &str) -> Result<(String, Option<u32>)> {
    print!("  Discovering models at {base_url} ... ");
    std::io::stdout().flush().ok();

    match providers::openai_compat::list_models(base_url, api_key).await {
        Ok(models) if !models.is_empty() => {
            println!("found {}.\n", models.len());
            for (i, m) in models.iter().enumerate() {
                match m.context_length {
                    Some(c) => println!("    {:>2}. {:<40} {} ctx", i + 1, m.id, c),
                    None => println!("    {:>2}. {}", i + 1, m.id),
                }
            }
            let pick = prompt(&format!("\n  Model [1-{}] (or type a name): ", models.len()))?;
            let pick = pick.trim();
            if let Ok(n) = pick.parse::<usize>() {
                if n >= 1 && n <= models.len() {
                    let m = &models[n - 1];
                    return Ok((m.id.clone(), m.context_length));
                }
            }
            if pick.is_empty() {
                anyhow::bail!("a model is required");
            }
            // User typed a name; carry over its context window if it matches one.
            let ctx = models.iter().find(|m| m.id == pick).and_then(|m| m.context_length);
            Ok((pick.to_string(), ctx))
        }
        Ok(_) => {
            println!("none reported.");
            Ok((manual_model()?, None))
        }
        Err(e) => {
            println!("unavailable ({e}).");
            Ok((manual_model()?, None))
        }
    }
}

fn manual_model() -> Result<String> {
    let model = prompt("  Model id (as your server names it): ")?
        .trim()
        .to_string();
    if model.is_empty() {
        anyhow::bail!("a model id is required");
    }
    Ok(model)
}

/// Resolve effective provider settings from (in order) CLI flags → env → config
/// file. `cfg` may be `None` (no config saved yet); returns `None` if base URL
/// or model can't be determined from any source — the caller then runs setup.
///
/// Env var names accept MEDHA_* plus the common `OPENAI_COMPATIBLE_*` / `OPENAI_*`
/// spellings, so an existing environment works without renaming anything.
pub fn resolve(
    cfg: Option<&Config>,
    flag_base_url: Option<String>,
    flag_model: Option<String>,
) -> Option<Resolved> {
    let base_url = flag_base_url
        .or_else(|| first_env(&["MEDHA_BASE_URL", "OPENAI_COMPATIBLE_BASE_URL", "OPENAI_BASE_URL"]))
        .or_else(|| cfg.map(|c| c.provider.base_url.clone()))?;
    let model = flag_model
        .or_else(|| first_env(&["MEDHA_MODEL", "OPENAI_COMPATIBLE_MODEL", "OPENAI_MODEL"]))
        .or_else(|| cfg.map(|c| c.provider.model.clone()))?;

    // Secret: env first, then keychain. Empty is valid (local servers need none).
    let api_key = first_env(&["MEDHA_API_KEY", "OPENAI_COMPATIBLE_API_KEY", "OPENAI_API_KEY"])
        .or_else(|| load_key(&base_url))
        .unwrap_or_default();

    let max_ctx = first_env(&["MEDHA_MAX_CTX"])
        .and_then(|s| s.parse().ok())
        .or_else(|| cfg.and_then(|c| c.provider.max_ctx));

    Some(Resolved { base_url, model, api_key, max_ctx })
}

/// First non-empty value among the given env var names.
fn first_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()))
}

fn store_key(base_url: &str, key: &str) -> Result<()> {
    match keyring::Entry::new(KEYRING_SERVICE, base_url) {
        Ok(entry) => entry.set_password(key).context("storing key in keychain"),
        Err(e) => {
            eprintln!("  warning: keychain unavailable ({e}); set MEDHA_API_KEY at runtime instead.");
            Ok(())
        }
    }
}

fn load_key(base_url: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, base_url)
        .ok()
        .and_then(|e| e.get_password().ok())
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).context("reading input")?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn encodes_workspace_path_claude_code_style() {
        // Leading separator and every '/' become '-'; existing hyphens survive.
        assert_eq!(encode_workspace(Path::new("/Users/x/proj")), "-Users-x-proj");
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
        assert!(a.starts_with(home), "state stays under MEDHA home, not the workspace");
    }
}
