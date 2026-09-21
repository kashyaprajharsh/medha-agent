//! Portable TOML configuration for routing, budgets, policy, and verification.
//! Precedence is environment, `medha.lock`, then built-in defaults.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use thiserror::Error;

pub mod trust;
pub use trust::{AcceptedLocks, RiskySetting};

/// Legacy permission type accepted when parsing old `medha.lock` files. Never a
/// source of runtime authority — retained only to warn about obsolete grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionType {
    Read,
    Write,
}

/// A legacy repository-provided permission entry. It is untrusted input.
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
        let elapsed = time
            .duration_since(UNIX_EPOCH)
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_u64(elapsed.as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        UNIX_EPOCH
            .checked_add(std::time::Duration::from_secs(secs))
            .ok_or_else(|| serde::de::Error::custom("timestamp is out of range"))
    }
}

/// Legacy `[permissions]` section in `medha.lock`.
///
/// These entries are parsed only so surfaces can warn that they were ignored.
/// Explicit approvals are stored separately in machine-local `trust.lock`.
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
    #[serde(default)]
    pub tools: ToolsConfig,
}

/// Which tools a session exposes.
///
/// Every tool's name, description and schema is re-sent on every request of
/// every turn, so the catalogue is a fixed tax whether or not a session uses
/// it. `minimal` drops it to the five tools that can still reach everything —
/// a shell is a general-purpose escape hatch — for cheap models, short runs,
/// and anything where the tax outweighs the convenience. `full` is the default
/// because the specialised tools are faster, safer and cheaper *per use*.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub preset: String,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            preset: "full".into(),
        }
    }
}

/// The smallest set that can still do the work: read a file, change one, run a
/// command, and find things by content or by name.
pub const MINIMAL_TOOLS: [&str; 5] = ["read", "edit", "shell.exec", "grep", "glob"];

impl ToolsConfig {
    pub fn validate(&self) -> Result<(), String> {
        match self.preset.as_str() {
            "full" | "minimal" => Ok(()),
            _ => Err(format!(
                "unknown tools preset {:?}; expected full or minimal",
                self.preset
            )),
        }
    }
    /// The names to expose, or `None` for everything registered.
    pub fn exposed(&self) -> Option<Vec<String>> {
        match self.preset.as_str() {
            "minimal" => Some(MINIMAL_TOOLS.map(String::from).into()),
            _ => None,
        }
    }
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

/// Portable sub-agent concurrency, depth, and spend limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsConfig {
    pub enabled: bool,
    pub max_active: usize,
    pub max_depth: u32,
    /// Whether isolated children may produce patches for human-reviewed application.
    pub write: bool,
    pub max_turns: u32,
    /// Bounds on one `agent.wait`, in seconds. The floor exists because a
    /// zero-length wait is a poll, and a model that can poll will.
    pub min_wait_secs: u64,
    pub default_wait_secs: u64,
    pub max_wait_secs: u64,
    /// Default transcript tail when no explicit limit is supplied.
    pub transcript_tail: usize,
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
            max_active: 3,
            max_depth: 1,
            write: true,
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
    pub health_interval_ms: u64,
    pub max_reconnects: u32,
    pub park_probe_ms: u64,
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

/// Eval-gate thresholds read by `medha gate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GateConfig {
    /// Where `medha gate` looks when handed a bare id or no path. Relative to cwd.
    pub scenarios_dir: String,
    /// Minimum pass-rate in (0.0, 1.0] for a `promote` verdict. Goldens default to
    /// 1.0 — a golden that regresses at all is a regression.
    pub pass_threshold: f64,
    /// Repeats per scenario, capped at 100; values above 10 require `--yes`.
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

/// Operator-declared executor pricing in USD per million tokens.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PricingConfig {
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
    /// Rate for prompt tokens the provider serves from its cache. Left unset,
    /// cached tokens bill at `input_per_mtok`, which overstates every turn after
    /// the first rather than understating it.
    pub cached_input_per_mtok: Option<f64>,
}

/// Execution sandbox for shell, build, and VCS commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxLockConfig {
    /// `"native"` = OS-native jail (macOS Seatbelt; Linux Landlock); `"host"` =
    /// no OS isolation; `"container"` = throwaway docker/podman container (opt-in
    /// heavy tier); `"ssh"` = run on a remote host.
    pub backend: String,
    /// `"deny"` (default; blocks exfiltration) or explicit `"allow"` for
    /// projects whose confined commands genuinely need downloads.
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
            network: "deny".into(),
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
    fn validate(&self) -> Result<(), String> {
        let backend = self.backend.trim().to_lowercase();
        if !matches!(
            backend.as_str(),
            "native"
                | "host"
                | "none"
                | "off"
                | "container"
                | "docker"
                | "podman"
                | "ssh"
                | "remote"
        ) {
            return Err(format!(
                "sandbox.backend has unknown value {:?}; expected native, host, container, docker, podman, ssh, or remote",
                self.backend
            ));
        }
        let network = self.network.trim().to_lowercase();
        if !matches!(network.as_str(), "allow" | "on" | "deny" | "off") {
            return Err(format!(
                "sandbox.network has unknown value {:?}; expected allow or deny",
                self.network
            ));
        }
        for path in &self.extra_writable {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                return Err("sandbox.extra_writable entries must not be empty".into());
            }
            let parsed = Path::new(trimmed);
            if !parsed.is_absolute() {
                return Err(format!(
                    "sandbox.extra_writable {trimmed:?} must be an absolute path"
                ));
            }
            if trimmed.contains("..") {
                return Err(format!(
                    "sandbox.extra_writable {trimmed:?} must not traverse with '..'"
                ));
            }
            // A jail that may write a filesystem root, an entire UNC share, or
            // the platform's top-level user directory is not a jail. Inspect
            // parsed components so this works for `/`, `C:\`, and UNC paths.
            let normal_components = parsed
                .components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(name) => {
                        Some(name.to_string_lossy().to_ascii_lowercase())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let is_broad_user_root = normal_components.len() == 1
                && matches!(normal_components[0].as_str(), "users" | "home");
            if normal_components.is_empty() || is_broad_user_root {
                return Err(format!(
                    "sandbox.extra_writable {trimmed:?} would widen the jail to the whole filesystem"
                ));
            }
        }
        Ok(())
    }

    pub fn to_config(&self) -> sandbox::SandboxConfig {
        let backend = match self.backend.trim().to_lowercase().as_str() {
            "host" | "none" | "off" | "" => sandbox::BackendKind::Host,
            "container" | "docker" | "podman" => sandbox::BackendKind::Container,
            "ssh" | "remote" => sandbox::BackendKind::Ssh,
            _ => sandbox::BackendKind::Native,
        };
        let net = match self.network.trim().to_lowercase().as_str() {
            "allow" | "on" => sandbox::NetPolicy::Allow,
            _ => sandbox::NetPolicy::Deny,
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

/// Parsed for forward compatibility and reported by `medha pulse`; no runtime
/// path selects a model from it yet, so setting it changes nothing.
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
    /// Per-turn tool concurrency; `None` uses the kernel default.
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

/// Default human-gated tools. Shell commands use deterministic scanning unless
/// `shell.exec` is explicitly added.
fn default_approve() -> Vec<String> {
    vec!["edit".into(), "skill.save".into()]
}

fn default_autonomy() -> String {
    "careful".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    #[serde(default = "default_approve")]
    pub approve: Vec<String>,
    /// Starting autonomy dial: `careful` (edits+shell ask) · `normal` (edits auto)
    /// · `yolo` (everything in-workspace auto). The safety floor is gated at every
    /// level. Switchable via `/mode` or `MEDHA_MODE`.
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

/// Optional deterministic post-edit command.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifyConfig {
    #[serde(default)]
    pub command: Option<String>,
    /// Require passing checks before the session can report completion.
    #[serde(default)]
    pub required: bool,
    /// Override the shared agent verification timeout, in seconds.
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

/// Initial TUI presentation settings; live toggles remain session-scoped.
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

/// Request-side reasoning defaults; absent values preserve server defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReasoningLockConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// auto, none, minimal, low, medium, high, xhigh, max, or ultra.
    /// Accepted levels depend on the selected protocol/model profile.
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
    pub fn to_config(&self) -> Result<kernel::ReasoningConfig, String> {
        let mut config = self
            .effort
            .as_deref()
            .map(kernel::ReasoningConfig::from_effort_text)
            .transpose()?
            .unwrap_or_default();
        if let Some(enabled) = self.enabled {
            if self.effort.is_some() && config.enabled.is_some_and(|value| value != enabled) {
                return Err("reasoning.enabled conflicts with reasoning.effort; use none to disable or auto for server default".into());
            }
            config.enabled = Some(enabled);
        }
        Ok(config)
    }
}

impl MedhaLock {
    /// Parse a lock file's TOML text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let lock: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        lock.sandbox.validate()?;
        lock.tools.validate()?;
        lock.reasoning.to_config()?;
        kernel::AutonomyLevel::parse(&lock.policy.autonomy)?;
        if lock.verify.timeout_s == Some(0) {
            return Err("verify.timeout_s must be greater than zero".into());
        }
        if lock
            .verify
            .command
            .as_ref()
            .is_some_and(|s| s.trim().is_empty())
        {
            return Err("verify.command must not be empty".into());
        }
        if lock
            .budget
            .max_cost_usd
            .is_some_and(|n| !n.is_finite() || n < 0.0)
        {
            return Err("budget.max_cost_usd must be finite and non-negative".into());
        }
        match (lock.pricing.input_per_mtok, lock.pricing.output_per_mtok) {
            (None, None) => {}
            (Some(input), Some(output))
                if input.is_finite() && output.is_finite() && input >= 0.0 && output >= 0.0 => {}
            _ => return Err(
                "pricing requires both input_per_mtok and output_per_mtok, finite and non-negative"
                    .into(),
            ),
        }
        for (name, value) in [
            ("trigger_ratio", lock.context.trigger_ratio),
            ("microcompact_ratio", lock.context.microcompact_ratio),
            ("tail_ratio", lock.context.tail_ratio),
            ("emergency_ratio", lock.context.emergency_ratio),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(format!("context.{name} must be finite and between 0 and 1"));
            }
        }
        Ok(lock)
    }

    /// Load from an explicit path. Absence is optional; a present file that
    /// cannot be read or parsed is a hard configuration error because silently
    /// falling back would relax budgets, sandboxing, approvals, and network
    /// policy the operator intended to enforce.
    pub fn load(path: impl AsRef<Path>) -> Result<Option<Self>, LockfileError> {
        let path = path.as_ref();
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(LockfileError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Self::parse(&text)
            .map(Some)
            .map_err(|message| LockfileError::Parse {
                path: path.to_path_buf(),
                message,
            })
    }

    /// Load `./medha.lock` from the current directory. Only a genuinely absent
    /// file falls back to defaults.
    pub fn load_default() -> Result<Self, LockfileError> {
        let directory =
            std::env::current_dir().map_err(|source| LockfileError::CurrentDirectory { source })?;
        Ok(Self::load(directory.join("medha.lock"))?.unwrap_or_default())
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }
}

#[derive(Debug, Error)]
pub enum LockfileError {
    #[error("could not resolve the current directory while locating medha.lock: {source}")]
    CurrentDirectory {
        #[source]
        source: std::io::Error,
    },
    #[error("could not read medha.lock at {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse medha.lock at {}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_permissions_parse_only_for_an_ignored_grant_warning() {
        let lock = MedhaLock::parse(
            "[[permissions.trusted_paths]]\n\
             path = \"/\"\n\
             permission = \"Read\"\n\
             granted_at = 123\n",
        )
        .unwrap();

        assert_eq!(lock.permissions.trusted_paths.len(), 1);
        assert_eq!(lock.permissions.trusted_paths[0].path, PathBuf::from("/"));
    }

    #[test]
    fn invalid_numeric_limits_are_configuration_errors() {
        for text in [
            "[budget]\nmax_cost_usd = nan",
            "[budget]\nmax_cost_usd = inf",
            "[budget]\nmax_cost_usd = -1.0",
            "[pricing]\ninput_per_mtok = 1.0",
            "[pricing]\ninput_per_mtok = -1.0\noutput_per_mtok = 1.0",
            "[context]\ntrigger_ratio = nan",
            "[context]\nemergency_ratio = 1.5",
            "[context]\ntail_ratio = -0.5",
        ] {
            assert!(MedhaLock::parse(text).is_err(), "accepted {text}");
        }
        assert!(MedhaLock::parse("[budget]\nmax_cost_usd = 0.0").is_ok());
        assert!(MedhaLock::parse("[pricing]\ninput_per_mtok = 0.0\noutput_per_mtok = 0.0").is_ok());
    }

    #[test]
    fn absent_file_yields_secure_defaults() {
        let lock = MedhaLock::default();
        assert_eq!(
            lock.budget.to_budget().max_turns,
            kernel::Budget::default().max_turns
        );
        assert_eq!(
            lock.context.to_policy().trigger_ratio,
            context::CompactionPolicy::default().trigger_ratio
        );
        assert_eq!(lock.policy.approve, vec!["edit", "skill.save"]);
        assert!(lock.verify.command.is_none());
        assert!(lock.memory.enabled);
        assert_eq!(lock.memory.k3_budget_tokens, 3_000);
        assert_eq!(lock.memory.write_approval, "user-scope");
        assert_eq!(lock.memory.stale_after_days, 30);
        assert!(lock.context_files.enabled);
        assert_eq!(lock.context_files.max_chars, 20_000);
        assert!(lock.context_files.progressive_discovery);
        assert_eq!(lock.sandbox.network, "deny");
        assert_eq!(lock.sandbox.to_config().net, sandbox::NetPolicy::Deny);
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
            approve = ["edit", "shell.exec"]

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
        assert_eq!(lock.budget.max_tokens, None);
        assert_eq!(lock.policy.approve, vec!["edit", "shell.exec"]);
        assert_eq!(lock.verify.command, Some("cargo check".to_string()));
        assert_eq!(lock.memory.k3_budget_tokens, 900);
        assert_eq!(lock.memory.write_approval, "all");
        assert_eq!(lock.memory.stale_after_days, 30);
        assert!(!lock.context_files.progressive_discovery);
        assert_eq!(lock.context_files.max_chars, 20_000);
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
        lock.policy.approve = vec!["edit".to_string()]; // explicit opt-down from the default set
        lock.save(&path).unwrap();

        let loaded = MedhaLock::load(&path).unwrap().unwrap();
        assert_eq!(loaded.budget.max_turns, Some(999));
        assert_eq!(loaded.policy.approve, vec!["edit"]);
    }

    #[test]
    fn missing_file_falls_back_to_defaults_not_an_error() {
        let loaded = MedhaLock::load("/nonexistent/path/medha.lock").unwrap();
        assert!(loaded.is_none()); // load() is explicit Option; load_default() covers the fallback
        let default_used = MedhaLock::load("/nonexistent/path/medha.lock")
            .unwrap()
            .unwrap_or_default();
        assert_eq!(default_used.policy.approve, vec!["edit", "skill.save"]);
    }

    #[test]
    fn malformed_present_file_fails_with_its_path_and_parse_location() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("medha.lock");
        std::fs::write(&path, "[sandbox\nnetwork = \"deny\"\n").unwrap();

        let error = MedhaLock::load(&path).unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("could not parse"));
        assert!(error.contains("line"));
    }

    #[test]
    fn unknown_sandbox_values_are_rejected_instead_of_coerced() {
        for text in [
            "[sandbox]\nbackend = \"contaner\"\n",
            "[sandbox]\nbackend = \"\"\n",
            "[sandbox]\nnetwork = \"true\"\n",
        ] {
            let error = MedhaLock::parse(text).unwrap_err();
            assert!(
                error.contains("sandbox."),
                "unexpected validation error for {text:?}: {error}"
            );
        }
    }

    #[test]
    fn unreadable_present_path_is_not_treated_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("medha.lock");
        std::fs::create_dir(&path).unwrap();

        let error = MedhaLock::load(&path).unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("could not read"));
    }

    #[test]
    fn explicit_empty_list_opts_into_full_autonomy() {
        let toml = "[policy]\napprove = []\n";
        let lock = MedhaLock::parse(toml).unwrap();
        assert!(lock.policy.approve.is_empty());
    }

    #[test]
    fn reasoning_effort_maps_known_values_and_rejects_unknown_levels() {
        let toml = r#"
            [reasoning]
            enabled = true
            effort = "medium"
        "#;
        let lock = MedhaLock::parse(toml).unwrap();
        let cfg = lock.reasoning.to_config().unwrap();
        assert_eq!(cfg.enabled, Some(true));
        assert_eq!(cfg.effort, Some(kernel::ReasoningEffort::Medium));

        let minimal = MedhaLock::parse("[reasoning]\neffort = \"minimal\"\n")
            .unwrap()
            .reasoning
            .to_config()
            .unwrap();
        assert_eq!(minimal.effort, Some(kernel::ReasoningEffort::Minimal));

        let toml2 = "[reasoning]\neffort = \"extreme\"\n";
        assert!(MedhaLock::parse(toml2).is_err());
        for level in kernel::ReasoningEffort::ALL {
            let text = format!("[reasoning]\neffort = \"{}\"\n", level.as_str());
            assert!(MedhaLock::parse(&text).is_ok(), "{text}");
        }
        assert!(MedhaLock::parse("[reasoning]\nenabled = false\neffort = \"high\"\n").is_err());

        assert_eq!(
            MedhaLock::default().reasoning.to_config().unwrap(),
            kernel::ReasoningConfig::default()
        );
    }
}
