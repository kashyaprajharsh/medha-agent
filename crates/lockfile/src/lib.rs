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
    pub memory: MemoryConfig,
    #[serde(default)]
    pub context_files: ContextFilesConfig,
    #[serde(default)]
    pub lsp: LspConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
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

/// Language-server code intelligence. Built-in adapters start automatically
/// when their installed executable matches a source file. Project-defined
/// commands remain inert until Medha previews them at a human gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LspConfig {
    pub enabled: bool,
    pub startup_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub diagnostics_timeout_ms: u64,
    pub diagnostic_settle_ms: u64,
    pub idle_timeout_ms: u64,
    pub restart_backoff_ms: u64,
    pub max_restart_attempts: u32,
    pub max_servers: usize,
    pub max_results: usize,
    pub max_text_chars: usize,
    pub max_open_documents: usize,
    /// Ceiling on one `lsp.install`. Network-bound, so generous.
    pub install_timeout_ms: u64,
    /// Ceiling on one write to a server's stdin, including waiting for the
    /// writer. A server that stops reading otherwise stalls every later caller.
    pub write_timeout_ms: u64,
    /// Largest single LSP frame accepted. `Content-Length` is the server's word
    /// for how much to allocate, and believing it turns one bad frame into an
    /// out-of-memory abort.
    pub max_frame_bytes: usize,
    pub allow_network: bool,
    pub servers: Vec<LspServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LspServerConfig {
    pub id: String,
    pub languages: Vec<String>,
    pub command: Vec<String>,
    pub root_markers: Vec<String>,
    pub trust: String,
    /// Server settings answered to `workspace/configuration`/sent as
    /// `initializationOptions`. Empty = server defaults. For a built-in `id`
    /// with no `command`, this tunes that built-in server.
    pub settings: toml::Table,
}

impl Default for LspServerConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            languages: Vec::new(),
            command: Vec::new(),
            root_markers: vec![".git".into()],
            trust: "workspace".into(),
            settings: toml::Table::new(),
        }
    }
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            startup_timeout_ms: 10_000,
            request_timeout_ms: 8_000,
            diagnostics_timeout_ms: 4_000,
            diagnostic_settle_ms: 1_000,
            idle_timeout_ms: 600_000,
            restart_backoff_ms: 5_000,
            max_restart_attempts: 5,
            max_servers: 8,
            max_results: 200,
            max_text_chars: 16_000,
            max_open_documents: 64,
            install_timeout_ms: 600_000,
            write_timeout_ms: 30_000,
            max_frame_bytes: 64 * 1024 * 1024,
            allow_network: false,
            servers: Vec::new(),
        }
    }
}

/// Sub-agent limits. These bound blast radius and spend, so they are an
/// operator decision that belongs in the committed lockfile — unlike which MCP
/// servers a machine happens to have.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsConfig {
    pub enabled: bool,
    /// Children alive at once, across the whole session tree.
    pub max_active: usize,
    /// Delegation depth. 1 keeps it flat: a child cannot spawn a child, which
    /// bounds how far a single request can fan out.
    pub max_depth: u32,
    /// Whether children may modify code (§6.4). A writing child works in its
    /// own git worktree and returns a patch that only lands once a human
    /// approves it — the parent's tree is never touched by the child itself.
    /// Off means writers are refused, not downgraded to editing in place.
    pub write: bool,
    /// Turn ceiling for one child. A child's turns are its own session's, but
    /// the tokens are the same wallet, so this is the real spend bound. Set it
    /// generously: a child cut off before it reports has spent everything and
    /// returned nothing, which is the expensive failure.
    pub max_turns: u32,
    /// Bounds on one `agent.wait`, in seconds. The floor exists because a
    /// zero-length wait is a poll, and a model that can poll will.
    pub min_wait_secs: u64,
    pub default_wait_secs: u64,
    pub max_wait_secs: u64,
    /// Steps `agent.transcript` returns when the caller does not say how many.
    /// Enough to see what an agent was doing when it stopped; short of what
    /// spills to an artifact and costs turns to page back in.
    pub transcript_tail: usize,
    /// Ceiling on one patch verification. The command runs whatever build
    /// scripts and tests the writer just edited, so it has to terminate.
    pub verify_timeout_secs: u64,
    /// How long a cancelled child may take to settle itself before its future
    /// is dropped. Long enough for a tool call in flight to finish writing,
    /// short enough that a cancel the user asked for still feels like one.
    pub cancel_grace_secs: u64,
    /// Hard ceiling for one extracted writer diff. Oversized work is preserved
    /// on disk rather than buffered or truncated.
    pub max_patch_bytes: usize,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            //  default, on the reasoning that each child burns tokens
            // independently.
            max_active: 3,
            max_depth: 1,
            // On: the isolation is what makes it safe, and it is structural
            // rather than advisory. Nothing a writing child does reaches the
            // user's files without the merge passing the human gate.
            write: true,
            // A real survey burned all 24 on exploration and was cut off before
            // writing anything; the whole run was wasted. The child is told its
            // budget and to wrap up early, but the ceiling has to leave room to.
            max_turns: 100,
            min_wait_secs: 1,
            default_wait_secs: 120,
            max_wait_secs: 600,
            transcript_tail: 40,
            verify_timeout_secs: 900,
            cancel_grace_secs: 5,
            max_patch_bytes: 16 * 1024 * 1024,
        }
    }
}

/// MCP host runtime tuning. The servers themselves live in the user config
/// (`~/.medha/config.toml`) and their API keys in the credential store — never
/// in this committable lockfile — so `[mcp]` only carries connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub startup_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub max_text_chars: usize,
    /// Default network policy; a server may override it with `network` in the
    /// user config.
    pub allow_network: bool,
    /// Supervisor sweep period: liveness probe and reconnect scheduling.
    pub health_interval_ms: u64,
    /// Consecutive connect failures tolerated before a server is parked.
    pub max_reconnects: u32,
    /// How long a parked server waits before one slow self-probe.
    pub park_probe_ms: u64,
    /// How long the interactive OAuth flow waits for the browser redirect.
    pub auth_timeout_ms: u64,
    /// Total deadline on one HTTP request to a remote server, discovery and
    /// token exchange included. A host that accepts the connection and then
    /// never answers otherwise hangs the flow.
    pub http_timeout_ms: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            startup_timeout_ms: 10_000,
            request_timeout_ms: 60_000,
            max_text_chars: 16_000,
            allow_network: true,
            health_interval_ms: 5_000,
            max_reconnects: 5,
            park_probe_ms: 300_000,
            auth_timeout_ms: 300_000,
            http_timeout_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub k3_budget_tokens: u32,
    pub write_approval: String,
    pub stale_after_days: u32,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            k3_budget_tokens: 3_000,
            write_approval: "user-scope".into(),
            stale_after_days: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextFilesConfig {
    pub enabled: bool,
    pub max_chars: usize,
    pub progressive_discovery: bool,
}

impl Default for ContextFilesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_chars: 20_000,
            progressive_discovery: true,
        }
    }
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
    /// Concurrent tool-call cap within one turn (§12). `None` = the kernel's
    /// built-in default (`kernel::DEFAULT_MAX_PARALLEL_TOOLS`).
    pub max_parallel_tools: Option<usize>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        let d = kernel::Budget::default();
        Self {
            max_turns: d.max_turns,
            max_tokens: d.max_tokens,
            max_cost_usd: d.max_cost_usd,
            max_wall_s: d.max_wall_s,
            max_parallel_tools: None,
        }
    }
}

impl BudgetConfig {
    pub fn to_budget(&self) -> kernel::Budget {
        // Unpooled: a pool is per *task*, and the caller starts one when a task
        // does. Pooling here would share one tally across every task the
        // process ever runs, so a long session would exhaust the ceiling and
        // never recover it.
        kernel::Budget {
            max_turns: self.max_turns,
            max_tokens: self.max_tokens,
            max_cost_usd: self.max_cost_usd,
            max_wall_s: self.max_wall_s,
            pooled: None,
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
    /// "minimal" | "low" | "medium" | "high" — unrecognized/absent values
    /// map to `None`.
    #[serde(default)]
    pub effort: Option<String>,
    /// SSE streaming. Absent/true → stream token-by-token (the norm). `false` →
    /// one blocking request per turn, whole reply at once; surfaces reasoning on
    /// gateways that only populate it in the non-streamed response. Toggle live
    /// with `/stream`.
    #[serde(default)]
    pub stream: Option<bool>,
}

impl ReasoningLockConfig {
    pub fn to_config(&self) -> kernel::ReasoningConfig {
        let effort = match self.effort.as_deref() {
            Some("minimal") => Some(kernel::ReasoningEffort::Minimal),
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
        assert!(lock.memory.enabled);
        assert_eq!(lock.memory.k3_budget_tokens, 3_000);
        assert_eq!(lock.memory.write_approval, "user-scope");
        assert_eq!(lock.memory.stale_after_days, 30);
        assert!(lock.context_files.enabled);
        assert_eq!(lock.context_files.max_chars, 20_000);
        assert!(lock.context_files.progressive_discovery);
        assert!(
            lock.lsp.enabled,
            "LSP code intelligence is automatic unless explicitly disabled"
        );
        assert_eq!(lock.lsp.startup_timeout_ms, 10_000);
        assert_eq!(lock.lsp.request_timeout_ms, 8_000);
        assert_eq!(lock.lsp.diagnostics_timeout_ms, 4_000);
        assert_eq!(lock.lsp.diagnostic_settle_ms, 1_000);
        assert_eq!(lock.lsp.idle_timeout_ms, 600_000);
        assert_eq!(lock.lsp.restart_backoff_ms, 5_000);
        assert_eq!(lock.lsp.max_restart_attempts, 5);
        assert_eq!(lock.lsp.max_servers, 8);
        assert_eq!(lock.lsp.max_results, 200);
        assert_eq!(lock.lsp.max_text_chars, 16_000);
        assert_eq!(lock.lsp.max_open_documents, 64);
        assert!(!lock.lsp.allow_network);
        assert!(lock.lsp.servers.is_empty());
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

            [memory]
            k3_budget_tokens = 900
            write_approval = "all"

            [context_files]
            progressive_discovery = false
        "#;
        let lock = MedhaLock::parse(toml).unwrap();
        assert_eq!(lock.budget.max_turns, Some(50));
        // Unspecified budget fields keep the built-in default (None = unbounded).
        assert_eq!(lock.budget.max_tokens, None);
        assert_eq!(lock.policy.approve, vec!["fs.write", "shell.exec"]);
        assert_eq!(lock.verify.command, Some("cargo check".to_string()));
        assert_eq!(lock.memory.k3_budget_tokens, 900);
        assert_eq!(lock.memory.write_approval, "all");
        assert_eq!(lock.memory.stale_after_days, 30);
        assert!(!lock.context_files.progressive_discovery);
        assert_eq!(lock.context_files.max_chars, 20_000);
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

        let minimal = MedhaLock::parse("[reasoning]\neffort = \"minimal\"\n")
            .unwrap()
            .reasoning
            .to_config();
        assert_eq!(minimal.effort, Some(kernel::ReasoningEffort::Minimal));

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
