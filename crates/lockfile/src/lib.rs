//! `medha.lock` — the harness artifact (§6). The entire cognitive
//! configuration — routing, budget, context/compaction tuning, policy
//! approvals, verifier command — as one declarative, diffable, portable TOML
//! file, instead of scattered struct defaults and ad-hoc env vars.
//!
//! Absence of a file is not an error: every section defaults to exactly the
//! behavior MEDHA already had before this artifact existed, so introducing
//! `medha.lock` never changes behavior for anyone who doesn't create one.
//! Env vars remain valid as *session-level overrides* on top of the loaded
//! lock (precedence: env > medha.lock > built-in default) — they're for quick
//! one-off tweaks; the lock file is the durable, versioned source of truth.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Permission type for trusted paths
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionType {
    Read,
    Write,
}

/// A trusted path entry in medha.lock
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPath {
    pub path: PathBuf,
    pub permission: PermissionType,
    #[serde(with = "serde_ts")]
    pub granted_at: SystemTime,
}

mod serde_ts {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(time.duration_since(UNIX_EPOCH).unwrap().as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(UNIX_EPOCH + std::time::Duration::from_secs(secs))
    }
}

/// Permissions configuration section in medha.lock
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub trusted_paths: Vec<TrustedPath>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MedhaLock {
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub budget: BudgetConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub verify: VerifyConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub reasoning: ReasoningLockConfig,
    #[serde(default)]
    pub sandbox: SandboxLockConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub pricing: PricingConfig,
    #[serde(default)]
    pub gate: GateConfig,
}

/// Eval-gate policy (§4.11–4.12, Vol 5 §5). Thresholds live here so the pass/fail
/// bar is part of the portable, diffable harness artifact — a harness A/B can
/// change the gate policy and that change is itself reviewable. `medha gate`
/// reads these; `#[serde(default)]` keeps existing locks parsing unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GateConfig {
    /// Where `medha gate` looks when handed a bare id or no path. Relative to cwd.
    pub scenarios_dir: String,
    /// Minimum pass-rate (0.0–1.0) for a `promote` verdict. Goldens default to
    /// 1.0 — a golden that regresses at all is a regression.
    pub pass_threshold: f64,
    /// Repeats per scenario. Agents are stochastic; >1 turns a single noisy
    /// verdict into a pass-rate with a confidence interval (Vol 5 §5). Kept at 1
    /// by default so local runs stay cheap; CI raises it.
    pub seeds: u32,
    /// Max tolerated per-scenario pass-rate drop vs a baseline before a
    /// regression is called (reserved for the global non-inferiority check).
    pub regression_epsilon: f64,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            scenarios_dir: "scenarios".into(),
            pass_threshold: 1.0,
            seeds: 1,
            regression_epsilon: 0.0,
        }
    }
}

/// Operator-declared per-token pricing for the executor model (P1-12), USD per
/// million tokens. Set this on self-hosted/custom routes where a vendor list
/// price doesn't apply (or to your negotiated rate). When unset, the models.dev
/// list price is used as an *indicative* figure (shown "est."); if that's
/// unknown too, the cost meter stays off — never a silent $0.00.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PricingConfig {
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
}

/// Execution sandbox for shell/build/VCS commands (§4.8). This makes the
/// containment posture part of the portable harness artifact: "this repo runs
/// shell in an OS-native jail with network allowed" travels, diffs, and is
/// ablatable — security-as-artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxLockConfig {
    /// `"native"` = OS-native jail (macOS Seatbelt; Linux Landlock); `"host"` =
    /// no OS isolation; `"container"` = throwaway docker/podman container (opt-in
    /// heavy tier); `"ssh"` = run on a remote host.
    pub backend: String,
    /// `"allow"` (default — builds/fetches work) or `"deny"` (stronger
    /// containment; blocks all network from confined commands).
    pub network: String,
    /// Extra absolute paths the jail may write to, beyond the workspace + temp
    /// + the built-in dev-cache set (e.g. a shared build directory).
    pub extra_writable: Vec<String>,
    /// Container backend: image to run (required for `container`).
    pub image: Option<String>,
    /// Container backend: runtime binary (`docker`/`podman`); auto-detected if unset.
    pub runtime: Option<String>,
    /// Container backend: memory cap (e.g. "2g") and max process count.
    pub memory: Option<String>,
    pub pids: Option<u32>,
    /// SSH backend: `user@host` (required for `ssh`), and remote working dir.
    pub host: Option<String>,
    pub remote_dir: Option<String>,
}

impl Default for SandboxLockConfig {
    fn default() -> Self {
        // OS-native containment by default where available; degrades to host on
        // platforms without a native backend (the CLI warns when it does).
        Self {
            backend: "native".into(),
            network: "allow".into(),
            extra_writable: Vec::new(),
            image: None,
            runtime: None,
            memory: None,
            pids: None,
            host: None,
            remote_dir: None,
        }
    }
}

impl SandboxLockConfig {
    pub fn to_config(&self) -> sandbox::SandboxConfig {
        let backend = match self.backend.trim().to_lowercase().as_str() {
            "host" | "none" | "off" | "" => sandbox::BackendKind::Host,
            "container" | "docker" | "podman" => sandbox::BackendKind::Container,
            "ssh" | "remote" => sandbox::BackendKind::Ssh,
            _ => sandbox::BackendKind::Native,
        };
        let net = match self.network.trim().to_lowercase().as_str() {
            "deny" | "off" | "none" => sandbox::NetPolicy::Deny,
            _ => sandbox::NetPolicy::Allow,
        };
        // `backend = "docker"/"podman"` is shorthand that also picks the runtime.
        let runtime =
            self.runtime
                .clone()
                .or_else(|| match self.backend.trim().to_lowercase().as_str() {
                    "docker" => Some("docker".into()),
                    "podman" => Some("podman".into()),
                    _ => None,
                });
        sandbox::SandboxConfig {
            backend,
            net,
            image: self.image.clone(),
            runtime,
            memory: self.memory.clone(),
            pids: self.pids,
            host: self.host.clone(),
            remote_dir: self.remote_dir.clone(),
        }
    }

    pub fn extra_writable_paths(&self) -> Vec<PathBuf> {
        self.extra_writable.iter().map(PathBuf::from).collect()
    }
}

/// Model routing by role (§4.4). Only `executor` is consulted today (the CLI
/// already resolves the provider from config/env — this documents intent and
/// is the seat the provider router lands in once a second model is wired for
/// adversarial cross-vendor verification).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub executor: Option<String>,
    #[serde(default)]
    pub verifier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_wall_s: Option<u64>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        let d = kernel::Budget::default();
        Self {
            max_turns: d.max_turns,
            max_tokens: d.max_tokens,
            max_cost_usd: d.max_cost_usd,
            max_wall_s: d.max_wall_s,
        }
    }
}

impl BudgetConfig {
    pub fn to_budget(&self) -> kernel::Budget {
        kernel::Budget {
            max_turns: self.max_turns,
            max_tokens: self.max_tokens,
            max_cost_usd: self.max_cost_usd,
            max_wall_s: self.max_wall_s,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    pub trigger_ratio: f32,
    pub microcompact_ratio: f32,
    pub tail_ratio: f32,
    pub protect_first_n: usize,
    pub protect_last_n: usize,
    /// Absent (default) = auto: scales with the context window (~1% of usable,
    /// min 200). Set an explicit token count to override.
    pub prune_min_tool_tokens: Option<u32>,
    pub emergency_ratio: f32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        let d = context::CompactionPolicy::default();
        Self {
            trigger_ratio: d.trigger_ratio,
            microcompact_ratio: d.microcompact_ratio,
            tail_ratio: d.tail_ratio,
            protect_first_n: d.protect_first_n,
            protect_last_n: d.protect_last_n,
            prune_min_tool_tokens: d.prune_min_tool_tokens,
            emergency_ratio: d.emergency_ratio,
        }
    }
}

impl ContextConfig {
    pub fn to_policy(&self) -> context::CompactionPolicy {
        context::CompactionPolicy {
            trigger_ratio: self.trigger_ratio,
            microcompact_ratio: self.microcompact_ratio,
            tail_ratio: self.tail_ratio,
            protect_first_n: self.protect_first_n,
            protect_last_n: self.protect_last_n,
            prune_min_tool_tokens: self.prune_min_tool_tokens,
            emergency_ratio: self.emergency_ratio,
        }
    }
}

/// Tool classes requiring human approval before execution (§4.7). File
/// mutations ask first by default (reversible-local but worth a nod). Shell is
/// deliberately *not* in this default set: per the spec's own policy model
/// (§4.6), shell commands are gated by the deterministic dangerous-pattern
/// scanner instead of a blanket ask-every-time gate — an ordinary `open
/// file.html` or `ls` shouldn't need a human in the loop, but `rm -rf /`,
/// `sudo`, credential reads, etc. are hard-blocked regardless of this list.
/// Set to `[]` for full autonomy, or add "shell.exec" to also gate shell.
fn default_approve() -> Vec<String> {
    // skill.save always shows an approval card so the user sees the full SKILL.md
    // being written before it lands (Phase A skills).
    vec![
        "fs.write".into(),
        "fs.edit".into(),
        "multi_edit".into(),
        "skill.save".into(),
    ]
}

fn default_autonomy() -> String {
    "careful".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    #[serde(default = "default_approve")]
    pub approve: Vec<String>,
    /// Starting autonomy dial: `careful` (edits+shell ask) · `normal` (edits
    /// auto, shell asks) · `yolo` (everything in-workspace auto). The safety
    /// floor (dangerous-command scanner, external actions, out-of-workspace
    /// access) is gated at every level. Live-switchable via `/mode` in the TUI
    /// or the `MEDHA_MODE` env override.
    #[serde(default = "default_autonomy")]
    pub autonomy: String,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            approve: default_approve(),
            autonomy: default_autonomy(),
        }
    }
}

/// Deterministic post-edit check (§4.7), e.g. `"cargo check"`. Empty = none.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifyConfig {
    #[serde(default)]
    pub command: Option<String>,
}

/// TUI presentation defaults (§4.13-adjacent surface config). A session can
/// still toggle these live with a keyboard shortcut; this only sets what the
/// TUI opens with.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Show the model's live reasoning/thinking stream, when the endpoint
    /// sends one. Off by default — reasoning is scratch content and can be
    /// verbose; configure live with the unified `/reasoning` command.
    pub show_thinking: bool,
    /// Show full, untruncated tool inputs/outputs instead of the summarized
    /// one-line view. Off by default for a readable stream; toggle live with
    /// the `/detail` command ("complete transparency" on demand, not always-on noise).
    pub full_transparency: bool,
}

/// Reasoning/thinking request-side control (§4.4), config-file counterpart to
/// the live `/reasoning` slash command. `enabled`/`effort` both `None` = don't
/// touch the server's own default. Not every model/server has a "medium" tier
/// (some only expose on/off) — an effort the adapter can't map is silently
/// unused rather than faked; see `kernel::ReasoningConfig`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReasoningLockConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// "low" | "medium" | "high" — unrecognized/absent values map to `None`.
    #[serde(default)]
    pub effort: Option<String>,
}

impl ReasoningLockConfig {
    pub fn to_config(&self) -> kernel::ReasoningConfig {
        let effort = match self.effort.as_deref() {
            Some("low") => Some(kernel::ReasoningEffort::Low),
            Some("medium") => Some(kernel::ReasoningEffort::Medium),
            Some("high") => Some(kernel::ReasoningEffort::High),
            _ => None,
        };
        kernel::ReasoningConfig {
            enabled: self.enabled,
            effort,
        }
    }
}

impl MedhaLock {
    /// Parse a lock file's TOML text.
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// Load from an explicit path, if it exists.
    pub fn load(path: impl AsRef<Path>) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        Self::parse(&text).ok()
    }

    /// Load `./medha.lock` from the current directory (the conventional
    /// project-root location), or fall back to defaults if absent/unparsable
    /// — never an error; `medha.lock` is optional (§6).
    pub fn load_default() -> Self {
        Self::load(default_path()).unwrap_or_default()
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }
}

fn default_path() -> PathBuf {
    PathBuf::from("medha.lock")
}

/// Move a legacy `[permissions]` block out of the portable `medha.lock` into a
/// machine-local trust file (§13.3). Runtime permission grants are per-machine
/// absolute paths — they must not travel with the portable, diffable harness
/// artifact. Runs once: if `trust_path` already exists, nothing happens.
///
/// After migration, `medha.lock` no longer carries `[permissions]` and the
/// trust file holds them in the identical shape the permission manager reads.
pub fn migrate_permissions_to_trust_file(
    medha_lock: &Path,
    trust_path: &Path,
) -> Result<(), String> {
    if trust_path.exists() {
        return Ok(()); // already migrated / trust file is the source of truth
    }
    let Ok(content) = std::fs::read_to_string(medha_lock) else {
        return Ok(()); // no lock file → nothing to migrate
    };
    let Ok(mut value) = toml::from_str::<toml::Value>(&content) else {
        return Ok(());
    };
    let Some(table) = value.as_table_mut() else {
        return Ok(());
    };
    let Some(perms) = table.remove("permissions") else {
        return Ok(()); // no permissions block → nothing to migrate
    };

    // Write the permissions into the trust file (same [permissions] shape).
    let mut trust_root = toml::Table::new();
    trust_root.insert("permissions".into(), perms);
    let trust_doc = toml::Value::Table(trust_root);
    if let Some(parent) = trust_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        trust_path,
        toml::to_string_pretty(&trust_doc).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    // Rewrite medha.lock without the permissions table so the portable artifact
    // stays clean and machine-independent.
    std::fs::write(
        medha_lock,
        toml::to_string_pretty(&value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_permissions_out_of_portable_lock() {
        let dir = std::env::temp_dir().join(format!("medha-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("medha.lock");
        let trust = dir.join(".medha").join("trust.lock");
        std::fs::write(
            &lock,
            "[budget]\nmax_turns = 50\n\n[[permissions.trusted_paths]]\npath = \"/some/abs/path\"\npermission = \"Read\"\ngranted_at = 123\n",
        )
        .unwrap();

        migrate_permissions_to_trust_file(&lock, &trust).unwrap();

        // Grants moved into the machine-local trust file...
        let trust_txt = std::fs::read_to_string(&trust).unwrap();
        assert!(trust_txt.contains("trusted_paths") && trust_txt.contains("/some/abs/path"));
        // ...and out of the portable lock (which keeps its real config).
        let lock_txt = std::fs::read_to_string(&lock).unwrap();
        assert!(lock_txt.contains("max_turns"), "real config preserved");
        assert!(
            !lock_txt.contains("permissions"),
            "portable lock must not carry grants"
        );

        // Idempotent: a second run is a no-op because the trust file now exists.
        migrate_permissions_to_trust_file(&lock, &trust).unwrap();
        assert_eq!(std::fs::read_to_string(&trust).unwrap(), trust_txt);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn absent_file_yields_defaults_matching_prior_hardcoded_behavior() {
        let lock = MedhaLock::default();
        // Matches kernel::Budget::default() / context::CompactionPolicy::default()
        // exactly, so introducing this artifact changes nothing by default.
        assert_eq!(
            lock.budget.to_budget().max_turns,
            kernel::Budget::default().max_turns
        );
        assert_eq!(
            lock.context.to_policy().trigger_ratio,
            context::CompactionPolicy::default().trigger_ratio
        );
        // File-write default (§4.6): writes need approval; shell is scanner-gated.
        assert_eq!(
            lock.policy.approve,
            vec!["fs.write", "fs.edit", "multi_edit", "skill.save"]
        );
        assert!(lock.verify.command.is_none());
    }

    #[test]
    fn partial_toml_only_overrides_specified_fields() {
        let toml = r#"
            [budget]
            max_turns = 50

            [policy]
            approve = ["fs.write", "shell.exec"]

            [verify]
            command = "cargo check"
        "#;
        let lock = MedhaLock::parse(toml).unwrap();
        assert_eq!(lock.budget.max_turns, Some(50));
        // Unspecified budget fields keep the built-in default (None = unbounded).
        assert_eq!(lock.budget.max_tokens, None);
        assert_eq!(lock.policy.approve, vec!["fs.write", "shell.exec"]);
        assert_eq!(lock.verify.command, Some("cargo check".to_string()));
        // context section wasn't in the TOML at all — full default applies.
        assert_eq!(
            lock.context.trigger_ratio,
            context::CompactionPolicy::default().trigger_ratio
        );
    }

    #[test]
    fn roundtrips_through_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("medha.lock");
        let mut lock = MedhaLock::default();
        lock.budget.max_turns = Some(999);
        lock.policy.approve = vec!["fs.edit".to_string()]; // explicit opt-down from the default set
        lock.save(&path).unwrap();

        let loaded = MedhaLock::load(&path).unwrap();
        assert_eq!(loaded.budget.max_turns, Some(999));
        assert_eq!(loaded.policy.approve, vec!["fs.edit"]);
    }

    #[test]
    fn missing_file_falls_back_to_defaults_not_an_error() {
        let loaded = MedhaLock::load("/nonexistent/path/medha.lock");
        assert!(loaded.is_none()); // load() is explicit Option; load_default() covers the fallback
        let default_used = MedhaLock::load("/nonexistent/path/medha.lock").unwrap_or_default();
        // Deny-first default applies even when no file exists at all.
        assert_eq!(
            default_used.policy.approve,
            vec!["fs.write", "fs.edit", "multi_edit", "skill.save"]
        );
    }

    #[test]
    fn explicit_empty_list_opts_into_full_autonomy() {
        let toml = "[policy]\napprove = []\n";
        let lock = MedhaLock::parse(toml).unwrap();
        assert!(lock.policy.approve.is_empty());
    }

    #[test]
    fn reasoning_effort_maps_known_values_and_defaults_unknown_to_none() {
        let toml = r#"
            [reasoning]
            enabled = true
            effort = "medium"
        "#;
        let lock = MedhaLock::parse(toml).unwrap();
        let cfg = lock.reasoning.to_config();
        assert_eq!(cfg.enabled, Some(true));
        assert_eq!(cfg.effort, Some(kernel::ReasoningEffort::Medium));

        // An unrecognized effort string degrades to None, not a panic/guess.
        let toml2 = "[reasoning]\neffort = \"extreme\"\n";
        let cfg2 = MedhaLock::parse(toml2).unwrap().reasoning.to_config();
        assert_eq!(cfg2.effort, None);

        // Absent section entirely -> both None (server default untouched).
        assert_eq!(
            MedhaLock::default().reasoning.to_config(),
            kernel::ReasoningConfig::default()
        );
    }
}
