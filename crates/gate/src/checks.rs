//! Deterministic check evaluation over a finished run (Vol 5 §2).
//!
//! Checks make no model call and introduce no randomness. Filesystem/event
//! checks are pure projections of the artifact; command checks execute the
//! copied artifact in a bounded, no-network container. The stochasticity lives
//! in the agent run that produced the artifact and is handled by seeds (see
//! `verdict.rs`).

use kernel::{Containment, Event, EventKind};
use sandbox::{
    BackendKind, ExecBackend, ExecRequest, NetPolicy, SandboxConfig, ShellOutcome,
    run_command_bounded,
};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::scenario::{
    Check, Scenario, matching_files, validate_glob_pattern, validate_relative_target,
};
use crate::verdict::RunStatus;

/// A repository-authored check is executable code. Keep its resource contract
/// independent of the model run's (potentially much larger) wall-clock budget.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const COMMAND_MAX_OUTPUT: usize = 64 * 1024;
/// Registration never starts repository code, but it still needs a ceiling so
/// an unavailable daemon cannot pin Gate forever.
const CONTAINER_CREATE_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-attempt budget for the synchronous `Drop` fallback. `Drop` blocks, so
/// this cannot be generous — but a budget that a loaded host routinely exceeds
/// converts the force-remove into the container leak this lease exists to
/// prevent, so it has to outlast ordinary contention rather than merely a quiet
/// machine.
const CONTAINER_DROP_TIMEOUT: Duration = Duration::from_secs(2);

/// The only execution posture Gate accepts for repository-authored command
/// checks.
///
/// The ordinary native backend intentionally permits host reads so compiler
/// toolchains keep working, and the host/SSH tiers are not local hermetic
/// isolation. Gate therefore fails command checks closed unless a configured
/// container can give the check a private filesystem and a denied network.
/// Non-command checks remain available under every backend.
pub struct CommandRunner {
    isolation: CommandIsolation,
    timeout: Duration,
    max_output: usize,
}

enum CommandIsolation {
    Container(SandboxConfig),
    Disabled(String),
    #[cfg(test)]
    Test(Arc<dyn ExecBackend>),
    // Only the Unix container tests build this; Windows has no runtime to
    // stub, so defining it there is dead code.
    #[cfg(all(test, unix))]
    TestContainer(PathBuf),
}

enum PreparedCommandBackend {
    #[cfg(test)]
    Direct(Arc<dyn ExecBackend>),
    Container {
        backend: sandbox::exec::ContainerBackend,
        lease: ContainerLease,
    },
}

/// Cancelling/dropping the caller must signal the independently-owned
/// container lifecycle before its JoinHandle detaches.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// The container daemon owns the actual check workload, so killing the attached
/// docker/podman client is not sufficient teardown. This lease force-removes the
/// uniquely named container after every outcome. Its Drop path is the backstop
/// when an outer future cancellation interrupts the async cleanup.
struct ContainerLease {
    runtime: PathBuf,
    name: String,
    registration_confirmed: bool,
    armed: bool,
}

impl ContainerLease {
    fn new(runtime: PathBuf, name: String) -> Self {
        Self {
            runtime,
            name,
            registration_confirmed: false,
            armed: true,
        }
    }

    fn confirm_registration(&mut self) {
        self.registration_confirmed = true;
    }

    async fn cleanup(&mut self) -> Result<(), String> {
        let mut last_error = "container cleanup did not run".to_string();
        for attempt in 0..3 {
            let mut command = tokio::process::Command::new(&self.runtime);
            command.args(container_cleanup_args(&self.name));
            match run_command_bounded(command, Duration::from_secs(5), 4 * 1024, None).await {
                Ok(output) if output.status == Some(0) => {
                    self.armed = false;
                    return Ok(());
                }
                Ok(output)
                    if self.registration_confirmed && cleanup_reports_absent(&output.output) =>
                {
                    self.armed = false;
                    return Ok(());
                }
                Ok(output) => {
                    last_error = if cleanup_reports_absent(&output.output) {
                        // A timed-out `create` client can lose the race with
                        // daemon-side name registration. An initial absence is
                        // therefore not proof of cleanup until the bounded
                        // retry/fallback window has elapsed.
                        "container name is not registered yet after an uncertain create".into()
                    } else {
                        format!(
                            "container cleanup exited {}: {}",
                            output.status.unwrap_or(-1),
                            output.output
                        )
                    };
                }
                Err(error) => last_error = error.to_string(),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await;
            }
        }
        Err(format!(
            "could not prove container `{}` was removed: {last_error}",
            self.name
        ))
    }
}

impl Drop for ContainerLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Drop cannot await. Use a short, bounded synchronous fallback so
        // cancelling the Gate future still sends `rm -f` before control returns.
        // Retry covers cancellation during the daemon's create/name-registration
        // window. A hung client is killed before the next attempt.
        for attempt in 0..3 {
            if cleanup_container_blocking(&self.runtime, &self.name, CONTAINER_DROP_TIMEOUT) {
                self.armed = false;
                return;
            }
            if attempt < 2 {
                std::thread::sleep(Duration::from_millis(100 * (attempt + 1)));
            }
        }
    }
}

fn container_cleanup_args(name: &str) -> [String; 4] {
    // `create` initializes image-declared anonymous volumes. Docker/Podman do
    // not remove those with a plain `rm`, so include `-v` to avoid a persistent
    // daemon-side disk/resource leak.
    ["rm".into(), "-f".into(), "-v".into(), name.into()]
}

fn cleanup_reports_absent(output: &str) -> bool {
    let output = output.to_ascii_lowercase();
    output.contains("no such container")
        || output.contains("no container with name or id")
        || output.contains("does not exist")
}

fn cleanup_container_blocking(runtime: &Path, name: &str, limit: Duration) -> bool {
    let mut command = std::process::Command::new(runtime);
    command
        .args(container_cleanup_args(name))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

impl CommandRunner {
    /// Build the check runner from the repository's configured sandbox.
    ///
    /// Network access is always tightened to `deny`; a scenario cannot loosen
    /// this boundary through `medha.lock`. Unsupported or unconfined backends
    /// produce an explicit failed check instead of falling back to host exec.
    pub fn from_sandbox(config: &SandboxConfig) -> Self {
        let mut restricted = config.clone();
        restricted.net = NetPolicy::Deny;
        let isolation = match restricted.backend {
            BackendKind::Container if restricted.image.as_deref().unwrap_or("").is_empty() => {
                CommandIsolation::Disabled(
                    "configured container has no image; repository command was not executed"
                        .into(),
                )
            }
            BackendKind::Container => CommandIsolation::Container(restricted),
            BackendKind::Host => CommandIsolation::Disabled(
                "host backend has no filesystem/network isolation; repository command was not executed"
                    .into(),
            ),
            BackendKind::Native => CommandIsolation::Disabled(
                "native backend permits host reads; use a container for Gate command checks"
                    .into(),
            ),
            BackendKind::Ssh => CommandIsolation::Disabled(
                "SSH is remote execution, not a hermetic local check sandbox; repository command was not executed"
                    .into(),
            ),
        };
        Self {
            isolation,
            timeout: COMMAND_TIMEOUT,
            max_output: COMMAND_MAX_OUTPUT,
        }
    }

    #[cfg(test)]
    fn for_test(backend: Arc<dyn ExecBackend>, timeout: Duration, max_output: usize) -> Self {
        Self {
            isolation: CommandIsolation::Test(backend),
            timeout,
            max_output,
        }
    }

    #[cfg(all(test, unix))]
    fn for_container_test(runtime: PathBuf, timeout: Duration, max_output: usize) -> Self {
        Self {
            isolation: CommandIsolation::TestContainer(runtime),
            timeout,
            max_output,
        }
    }

    fn backend(&self, workspace: &Path) -> Result<PreparedCommandBackend, String> {
        match &self.isolation {
            CommandIsolation::Disabled(reason) => Err(reason.clone()),
            CommandIsolation::Container(config) => {
                let image = config.image.clone().ok_or_else(|| {
                    "configured container has no image; repository command was not executed"
                        .to_string()
                })?;
                validate_container_image(&image)?;
                let runtime = trusted_container_runtime(config, workspace)?;
                let container_name = format!(
                    "medha-gate-{}",
                    ulid::Ulid::new().to_string().to_ascii_lowercase()
                );
                let backend = sandbox::exec::ContainerBackend::new_hermetic(
                    runtime.display().to_string(),
                    image,
                    Some("4g".into()),
                    Some(config.pids.unwrap_or(256).clamp(1, 512)),
                    container_name.clone(),
                );
                if backend.containment() != Containment::OsFsJailNoNet {
                    return Err(
                        "configured backend does not confine both filesystem and network".into(),
                    );
                }
                Ok(PreparedCommandBackend::Container {
                    backend,
                    lease: ContainerLease::new(runtime, container_name),
                })
            }
            #[cfg(test)]
            CommandIsolation::Test(backend) => {
                Ok(PreparedCommandBackend::Direct(Arc::clone(backend)))
            }
            #[cfg(all(test, unix))]
            CommandIsolation::TestContainer(runtime) => {
                let container_name = format!(
                    "medha-gate-{}",
                    ulid::Ulid::new().to_string().to_ascii_lowercase()
                );
                let backend = sandbox::exec::ContainerBackend::new_hermetic(
                    runtime.display().to_string(),
                    "local-check-image".into(),
                    Some("4g".into()),
                    Some(256),
                    container_name.clone(),
                );
                Ok(PreparedCommandBackend::Container {
                    backend,
                    lease: ContainerLease::new(runtime.clone(), container_name),
                })
            }
        }
    }
}

/// Image references are passed as one argv item, but a leading option would be
/// parsed by docker/podman as a runtime flag rather than as the image boundary.
/// Keep repository configuration in the OCI-reference character set.
fn validate_container_image(image: &str) -> Result<(), String> {
    let valid = !image.is_empty()
        && !image.starts_with('-')
        && image.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-' | b':' | b'@')
        });
    if valid {
        Ok(())
    } else {
        Err("invalid container image reference; repository command was not executed".into())
    }
}

/// Resolve only a real docker/podman client selected by the operator's PATH.
/// `runtime` lives in repository-owned `medha.lock`, so accepting an arbitrary
/// program/path here would merely move the original host-code-execution bug
/// from `sh` to a fake "docker" executable in the checkout.
fn trusted_container_runtime(config: &SandboxConfig, workspace: &Path) -> Result<PathBuf, String> {
    let requested = config.runtime.as_deref().map(str::trim);
    let names: &[&str] = match requested {
        None | Some("") => &["docker", "podman"],
        Some("docker") | Some("docker.exe") => &["docker"],
        Some("podman") | Some("podman.exe") => &["podman"],
        Some(other) => {
            return Err(format!(
                "untrusted container runtime `{other}`; only docker or podman may run Gate checks"
            ));
        }
    };
    let runtime = names
        .iter()
        .find_map(|name| sandbox::exec::locate_on_path(name))
        .ok_or_else(|| {
            format!(
                "{} not found; repository command was not executed",
                names.join("/")
            )
        })?
        .canonicalize()
        .map_err(|error| format!("could not resolve container runtime: {error}"))?;

    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let current = std::env::current_dir()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .and_then(|path| path.canonicalize().ok());
    let temp = std::env::temp_dir().canonicalize().ok();
    if runtime.starts_with(&workspace)
        || current
            .as_ref()
            .is_some_and(|path| runtime.starts_with(path))
        || home.as_ref().is_some_and(|path| runtime.starts_with(path))
        || temp.as_ref().is_some_and(|path| runtime.starts_with(path))
    {
        return Err(format!(
            "container runtime `{}` is in a repository/user-writable location; command was not executed",
            runtime.display()
        ));
    }
    Ok(runtime)
}

/// Everything a finished run leaves behind for scoring: the mutated workspace,
/// a pristine copy of the fixture to diff against, the projected event log, and
/// coarse run metadata.
#[derive(Debug)]
pub struct RunArtifact {
    /// The workspace after the agent ran (fixture + the agent's edits).
    pub workspace: PathBuf,
    /// An untouched copy of the fixture, for `unchanged`/`changed` diffs.
    pub pristine: PathBuf,
    /// The run's events, projected from its isolated event log.
    pub events: Vec<Event>,
    /// Exact process outcome; deterministic checks cannot override a failed
    /// agent exit when the seed is scored.
    pub status: RunStatus,
    /// Wall-clock time the agent run took.
    pub wall_ms: u128,
    /// Owns the whole run tree until scoring finishes. Taking it with
    /// [`RunArtifact::preserve`] is the only production path that keeps the
    /// directory.
    pub(crate) run_dir: Option<tempfile::TempDir>,
    pub(crate) preserved_path: Option<PathBuf>,
}

impl RunArtifact {
    pub fn preserve(&mut self) -> Option<PathBuf> {
        if self.preserved_path.is_none()
            && let Some(directory) = self.run_dir.take()
        {
            self.preserved_path = Some(directory.keep());
        }
        self.preserved_path.clone()
    }

    pub fn preserved_path(&self) -> Option<&Path> {
        self.preserved_path.as_deref()
    }
}

/// The result of one check against one run.
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub label: String,
    pub passed: bool,
    pub detail: String,
    /// Normalized relative file target/pattern, when this check addresses the
    /// workspace filesystem.
    pub normalized_target: Option<String>,
    /// Number of matching fixture files before the agent ran.
    pub baseline_matches: Option<usize>,
    /// Number of matching workspace files after the agent ran.
    pub workspace_matches: Option<usize>,
    /// Whether path/glob containment was validated for this outcome.
    pub validation: ValidationStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationStatus {
    Validated,
    Invalid,
    NotApplicable,
}

impl ValidationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validated => "validated",
            Self::Invalid => "invalid",
            Self::NotApplicable => "not_applicable",
        }
    }
}

/// Evaluate every check in the scenario against a run. Order is preserved.
pub async fn evaluate(
    scn: &Scenario,
    art: &RunArtifact,
    command_runner: &CommandRunner,
) -> Vec<CheckOutcome> {
    let mut outcomes = Vec::with_capacity(scn.checks.len());
    for check in &scn.checks {
        outcomes.push(eval_one(check, art, command_runner).await);
    }
    outcomes
}

async fn eval_one(
    check: &Check,
    art: &RunArtifact,
    command_runner: &CommandRunner,
) -> CheckOutcome {
    let mut outcome = CheckOutcome {
        label: check.label(),
        passed: false,
        detail: String::new(),
        normalized_target: None,
        baseline_matches: None,
        workspace_matches: None,
        validation: ValidationStatus::NotApplicable,
    };
    match check {
        Check::Command {
            run,
            expect_exit,
            contains,
        } => {
            (outcome.passed, outcome.detail) = run_command(
                command_runner,
                &art.workspace,
                run,
                *expect_exit,
                contains.as_deref(),
            )
            .await;
        }
        Check::Unchanged {
            pattern,
            allow_zero_matches,
        } => {
            evaluate_glob_check(
                &mut outcome,
                &art.pristine,
                &art.workspace,
                pattern,
                *allow_zero_matches,
                false,
            );
        }
        Check::Changed {
            pattern,
            allow_zero_matches,
        } => {
            evaluate_glob_check(
                &mut outcome,
                &art.pristine,
                &art.workspace,
                pattern,
                *allow_zero_matches,
                true,
            );
        }
        Check::Exists(p) => {
            evaluate_path_check(&mut outcome, &art.workspace, p, true);
        }
        Check::Absent(p) => {
            evaluate_path_check(&mut outcome, &art.workspace, p, false);
        }
        Check::ToolUsed(tool) => {
            let n = count_tool_intents(&art.events, tool);
            outcome.passed = n > 0;
            outcome.detail = format!("{n} call(s)");
        }
        Check::ToolNotUsed(tool) => {
            let n = count_tool_intents(&art.events, tool);
            outcome.passed = n == 0;
            outcome.detail = format!("{n} call(s)");
        }
        Check::EventAbsent { kind, contains } => {
            let n = count_events(&art.events, kind, contains);
            outcome.passed = n == 0;
            outcome.detail = format!("{n} match(es)");
        }
        Check::EventPresent { kind, contains } => {
            let n = count_events(&art.events, kind, contains);
            outcome.passed = n > 0;
            outcome.detail = format!("{n} match(es)");
        }
    }
    outcome
}

fn evaluate_glob_check(
    outcome: &mut CheckOutcome,
    pristine: &Path,
    workspace: &Path,
    pattern: &str,
    allow_zero_matches: bool,
    expect_change: bool,
) {
    let normalized = match validate_glob_pattern(pattern) {
        Ok(_) => pattern.to_string(),
        Err(error) => {
            outcome.validation = ValidationStatus::Invalid;
            outcome.detail = format!("validation failed for pattern {pattern:?}: {error}");
            return;
        }
    };
    outcome.normalized_target = Some(normalized.clone());
    match diff_glob(pristine, workspace, &normalized) {
        Err(error) => {
            outcome.validation = ValidationStatus::Invalid;
            outcome.detail =
                format!("validation failed for relative pattern {normalized:?}: {error}");
        }
        Ok(diff) => {
            outcome.validation = ValidationStatus::Validated;
            outcome.baseline_matches = Some(diff.baseline_matches);
            outcome.workspace_matches = Some(diff.workspace_matches);
            let prefix = format!(
                "validated relative pattern {normalized:?}; baseline matches: {}; \
                 workspace matches: {}",
                diff.baseline_matches, diff.workspace_matches
            );
            if diff.baseline_matches == 0 && !allow_zero_matches {
                outcome.detail =
                    format!("{prefix}; zero baseline matches are forbidden by this check");
                return;
            }
            outcome.passed = if expect_change {
                !diff.changed.is_empty()
            } else {
                diff.changed.is_empty()
            };
            outcome.detail = if diff.changed.is_empty() {
                format!("{prefix}; no changes")
            } else {
                format!("{prefix}; changed: {}", diff.changed.join(", "))
            };
        }
    }
}

fn evaluate_path_check(
    outcome: &mut CheckOutcome,
    workspace: &Path,
    target: &str,
    expect_exists: bool,
) {
    let normalized = match validate_relative_target(target) {
        Ok(normalized) => normalized,
        Err(error) => {
            outcome.validation = ValidationStatus::Invalid;
            outcome.detail = format!("validation failed for target {target:?}: {error}");
            return;
        }
    };
    outcome.normalized_target = Some(normalized.clone());
    match inspect_workspace_target(workspace, &normalized) {
        Ok(exists) => {
            outcome.validation = ValidationStatus::Validated;
            outcome.baseline_matches = None;
            outcome.workspace_matches = Some(usize::from(exists));
            outcome.passed = exists == expect_exists;
            outcome.detail = format!(
                "validated relative target {normalized:?}; match count: {}; {}",
                usize::from(exists),
                match (expect_exists, exists) {
                    (true, true) => "found",
                    (true, false) => "not found",
                    (false, true) => "present",
                    (false, false) => "absent",
                }
            );
        }
        Err(error) => {
            outcome.validation = ValidationStatus::Invalid;
            outcome.detail =
                format!("validation failed for relative target {normalized:?}: {error}");
        }
    }
}

/// Inspect a normalized relative target without following an agent-created
/// symbolic link. An absent ordinary component is a safely-contained miss.
fn inspect_workspace_target(workspace: &Path, normalized: &str) -> Result<bool, String> {
    let metadata = std::fs::symlink_metadata(workspace).map_err(|error| {
        format!(
            "could not inspect workspace {}: {error}",
            workspace.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "workspace root is not an ordinary directory: {}",
            workspace.display()
        ));
    }
    let root = workspace.canonicalize().map_err(|error| {
        format!(
            "could not resolve workspace {}: {error}",
            workspace.display()
        )
    })?;
    let components = normalized.split('/').collect::<Vec<_>>();
    let mut current = root;
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "target crosses symbolic link at {}",
                    components[..=index].join("/")
                ));
            }
            Ok(metadata) if index + 1 < components.len() && !metadata.is_dir() => {
                return Ok(false);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!("could not inspect {}: {error}", current.display()));
            }
        }
    }
    Ok(true)
}

/// A disposable HOME/TMPDIR inside the mounted workspace keeps the command from
/// discovering operator configuration. It is removed after every check so its
/// bookkeeping cannot influence later filesystem checks.
struct CheckEnvironment(PathBuf);

impl CheckEnvironment {
    fn create(workspace: &Path) -> Result<Self, String> {
        let root = workspace.join(format!(".medha-gate-check-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(root.join("home"))
            .and_then(|_| std::fs::create_dir_all(root.join("tmp")))
            .map_err(|error| format!("could not create isolated check environment: {error}"))?;
        Ok(Self(root))
    }

    fn environment(&self, backend: &str) -> Vec<(String, String)> {
        let (home, tmp) = if backend == "container" {
            let name = self
                .0
                .file_name()
                .expect("check environment has a name")
                .to_string_lossy();
            (
                format!("/workspace/{name}/home"),
                format!("/workspace/{name}/tmp"),
            )
        } else {
            (
                self.0.join("home").display().to_string(),
                self.0.join("tmp").display().to_string(),
            )
        };
        let mut env = vec![
            ("HOME".into(), home),
            ("TMPDIR".into(), tmp),
            ("LANG".into(), "C".into()),
            ("LC_ALL".into(), "C".into()),
        ];
        if backend == "container" {
            // Fixed image-internal search path: supports ordinary compiler/test
            // names without copying the operator's potentially repository-
            // writable PATH into the check.
            env.push((
                "PATH".into(),
                "/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                    .into(),
            ));
        } else {
            #[cfg(unix)]
            env.push((
                "PATH".into(),
                "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into(),
            ));
            #[cfg(windows)]
            {
                for key in ["SystemRoot", "WINDIR", "COMSPEC", "PATHEXT"] {
                    if let Ok(value) = std::env::var(key) {
                        env.push((key.into(), value));
                    }
                }
                let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
                env.push((
                    "PATH".into(),
                    format!(r"{root}\System32;{root};{root}\System32\Wbem"),
                ));
            }
        }
        env
    }
}

impl Drop for CheckEnvironment {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Execute one repository-authored command with the configured environment,
/// time/output limits, and process-tree ownership.
async fn execute_command(
    runner: &CommandRunner,
    workspace: &Path,
    cmd: &str,
) -> Result<ShellOutcome, String> {
    let check_env = CheckEnvironment::create(workspace)?;
    let backend = runner.backend(workspace)?;
    match backend {
        #[cfg(test)]
        PreparedCommandBackend::Direct(backend) => {
            let request = check_request(&check_env, backend.label(), workspace, cmd);
            let command = backend
                .build_command(&request)
                .map_err(|error| error.to_string())?;
            run_command_bounded(command, runner.timeout, runner.max_output, None)
                .await
                .map_err(|error| error.to_string())
        }
        PreparedCommandBackend::Container { backend, lease } => {
            let request = check_request(&check_env, "container", workspace, cmd);
            let cancellation = CancellationToken::new();
            let _cancel_on_drop = CancelOnDrop(cancellation.clone());
            let timeout = runner.timeout;
            let max_output = runner.max_output;
            // The task owns the temp environment, lease, and both runtime
            // phases. Dropping this outer future only detaches the JoinHandle;
            // CancelOnDrop signals the task, which still force-removes the
            // daemon-owned container before it finishes.
            let lifecycle = tokio::spawn(async move {
                let _check_env = check_env;
                execute_container_lifecycle(
                    backend,
                    lease,
                    request,
                    timeout,
                    max_output,
                    cancellation,
                )
                .await
            });
            lifecycle
                .await
                .map_err(|error| format!("container lifecycle task failed: {error}"))?
        }
    }
}

fn check_request(
    check_env: &CheckEnvironment,
    backend_label: &str,
    workspace: &Path,
    cmd: &str,
) -> ExecRequest {
    let (mut program, mut args) = sandbox::exec::shell_argv(backend_label, cmd);
    let env = check_env.environment(backend_label);

    // A container image may declare arbitrary ENV entries. Put `env -i` inside
    // the container so the actual scenario command sees exactly our allowlist,
    // not image defaults (and never the operator's provider credentials).
    if backend_label == "container" {
        let mut wrapped = vec!["-i".into()];
        wrapped.extend(env.iter().map(|(key, value)| format!("{key}={value}")));
        wrapped.push(program);
        wrapped.append(&mut args);
        program = "env".into();
        args = wrapped;
    }

    ExecRequest {
        program,
        args,
        cwd: workspace.to_path_buf(),
        env,
        clear_env: true,
    }
}

/// Run a hermetic check as two daemon lifecycle phases.
///
/// The registration phase intentionally ignores cancellation until it settles:
/// `create` cannot run repository code, and waiting prevents a late daemon
/// registration from appearing after cleanup. Once the name is registered,
/// cancellation is checked before `start` and also supervises the attached
/// client. Async cleanup runs after every result; the lease's bounded Drop path
/// is the final backstop for task panic/runtime shutdown.
async fn execute_container_lifecycle(
    backend: sandbox::exec::ContainerBackend,
    mut lease: ContainerLease,
    request: ExecRequest,
    timeout: Duration,
    max_output: usize,
    cancellation: CancellationToken,
) -> Result<ShellOutcome, String> {
    let operation: Result<ShellOutcome, String> = async {
        if cancellation.is_cancelled() {
            return Ok(cancelled_before_start());
        }

        let create_command = backend
            .build_create_command(&request)
            .map_err(|error| error.to_string())?;
        let created = run_command_bounded(create_command, CONTAINER_CREATE_TIMEOUT, 8 * 1024, None)
            .await
            .map_err(|error| format!("container create failed: {error}"))?;
        if created.timed_out {
            return Err(format!(
                "container create timed out after {}s; workload was not started",
                CONTAINER_CREATE_TIMEOUT.as_secs()
            ));
        }
        if created.cancelled {
            return Err(
                "container create was unexpectedly cancelled; workload was not started".into(),
            );
        }
        if created.status != Some(0) {
            return Err(format!(
                "container create exited {}: {}",
                created.status.unwrap_or(-1),
                created.output
            ));
        }
        lease.confirm_registration();

        if cancellation.is_cancelled() {
            return Ok(cancelled_before_start());
        }

        let start_command = backend
            .build_start_command()
            .map_err(|error| error.to_string())?;
        run_command_bounded(start_command, timeout, max_output, Some(&cancellation))
            .await
            .map_err(|error| format!("container start failed: {error}"))
    }
    .await;

    let cleanup = lease.cleanup().await;
    match (operation, cleanup) {
        (Ok(output), Ok(())) => Ok(output),
        (Err(operation_error), Ok(())) => Err(operation_error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(operation_error), Err(cleanup_error)) => {
            Err(format!("{operation_error}; additionally, {cleanup_error}"))
        }
    }
}

fn cancelled_before_start() -> ShellOutcome {
    ShellOutcome {
        status: None,
        output: "[cancelled before container workload start]".into(),
        timed_out: false,
        cancelled: true,
    }
}

/// Assert exit code and optional output substring. Launch errors, disabled
/// isolation, timeouts, and signals all fail explicitly.
async fn run_command(
    runner: &CommandRunner,
    workspace: &Path,
    cmd: &str,
    expect_exit: i32,
    contains: Option<&str>,
) -> (bool, String) {
    let out = match execute_command(runner, workspace, cmd).await {
        Ok(out) => out,
        Err(error) => return (false, format!("could not run safely: {error}")),
    };
    if out.timed_out {
        return (
            false,
            format!(
                "timed out after {}s; process tree stopped",
                runner.timeout.as_secs()
            ),
        );
    }
    if out.cancelled {
        return (false, "cancelled; process tree stopped".into());
    }

    let code = out.status.unwrap_or(-1);
    let mut passed = out.status == Some(expect_exit);
    let mut why = format!("exit {code}, retained {} output byte(s)", out.output.len());
    if let Some(sub) = contains {
        let has = out.output.contains(sub);
        passed = passed && has;
        why = format!(
            "{why}, output {}contains \"{sub}\"",
            if has { "" } else { "does not " }
        );
    }
    (passed, why)
}

/// Relative paths (under either root) matching `pattern` whose bytes differ
/// between the pristine fixture and the post-run workspace. A file present in
/// one tree but not the other counts as a difference.
struct GlobDiff {
    baseline_matches: usize,
    workspace_matches: usize,
    changed: Vec<String>,
}

fn diff_glob(pristine: &Path, workspace: &Path, pattern: &str) -> Result<GlobDiff, String> {
    let baseline = matching_files(pristine, pattern)?;
    let after = matching_files(workspace, pattern)?;
    let mut rels: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    rels.extend(baseline.iter().cloned());
    rels.extend(after.iter().cloned());
    let changed = rels
        .into_iter()
        .filter(|rel| {
            let a = std::fs::read(pristine.join(rel)).ok();
            let b = std::fs::read(workspace.join(rel)).ok();
            a != b
        })
        .collect();
    Ok(GlobDiff {
        baseline_matches: baseline.len(),
        workspace_matches: after.len(),
        changed,
    })
}

/// How many times the agent issued a tool intent for `tool`.
fn count_tool_intents(events: &[Event], tool: &str) -> usize {
    events
        .iter()
        .filter(|e| e.kind == EventKind::ModelIntent)
        .filter(|e| e.payload.get("tool").and_then(|v| v.as_str()) == Some(tool))
        .count()
}

/// Events whose kind matches `kind` (prefix match, so "policy" catches
/// "policy.decision") and whose serialized payload contains `needle`.
fn count_events(events: &[Event], kind: &str, needle: &str) -> usize {
    events
        .iter()
        .filter(|e| kind_matches(e.kind.as_str(), kind))
        .filter(|e| {
            serde_json::to_string(&e.payload)
                .unwrap_or_default()
                .contains(needle)
        })
        .count()
}

fn kind_matches(actual: &str, wanted: &str) -> bool {
    actual == wanted || actual.starts_with(&format!("{wanted}."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::{Provenance, TrustLabel};
    use sandbox::HostBackend;
    use serde_json::json;

    /// Behaves like the host backend but plants a would-be inherited credential
    /// before applying `clear_env`, making secret stripping deterministic without
    /// mutating the test process's global environment.
    struct SecretBearingHost;

    impl ExecBackend for SecretBearingHost {
        fn build_command(
            &self,
            req: &ExecRequest,
        ) -> Result<tokio::process::Command, sandbox::ExecError> {
            let mut command = tokio::process::Command::new(&req.program);
            command
                .args(&req.args)
                .current_dir(&req.cwd)
                .env("MEDHA_API_KEY", "must-not-reach-check");
            if req.clear_env {
                command.env_clear();
            }
            command.envs(req.env.iter().map(|(key, value)| (key, value)));
            Ok(command)
        }

        fn label(&self) -> &str {
            "host"
        }

        fn containment(&self) -> Containment {
            Containment::OsFsJailNoNet
        }
    }

    fn test_runner(timeout: Duration, max_output: usize) -> CommandRunner {
        CommandRunner::for_test(Arc::new(HostBackend), timeout, max_output)
    }

    fn temp_workspace(prefix: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn art(workspace: PathBuf, pristine: PathBuf, events: Vec<Event>) -> RunArtifact {
        RunArtifact {
            workspace,
            pristine,
            events,
            status: RunStatus::Succeeded,
            wall_ms: 0,
            run_dir: None,
            preserved_path: None,
        }
    }

    fn intent_event(tool: &str) -> Event {
        Event {
            id: ulid::Ulid::new(),
            session_id: ulid::Ulid::new(),
            parent_id: None,
            kind: EventKind::ModelIntent,
            payload: json!({ "id": "1", "tool": tool, "args": {} }),
            trust: TrustLabel::System,
            provenance: Provenance {
                source: "test".into(),
            },
            prev_hash: [0u8; 32],
            hash_version: kernel::events::EVENT_HASH_VERSION,
            ts: 0.0,
        }
    }

    fn policy_deny(reason: &str) -> Event {
        Event {
            id: ulid::Ulid::new(),
            session_id: ulid::Ulid::new(),
            parent_id: None,
            kind: EventKind::PolicyDecision,
            payload: json!({ "tool": "shell.exec", "decision": "deny", "reason": reason }),
            trust: TrustLabel::System,
            provenance: Provenance {
                source: "test".into(),
            },
            prev_hash: [0u8; 32],
            hash_version: kernel::events::EVENT_HASH_VERSION,
            ts: 0.0,
        }
    }

    /// Probe that prints `absent` when `MEDHA_API_KEY` is unset, in the syntax
    /// of the interpreter that will actually run it.
    ///
    /// Branching on `cfg(windows)` is not enough: `shell_argv` picks bash when
    /// it exists and only falls back to PowerShell or cmd, so a hardcoded cmd
    /// probe is a syntax error under the shell really in use — which is what a
    /// Windows runner (git bash present) does.
    fn secret_probe_command() -> &'static str {
        const POSIX: &str =
            r#"if [ -z "${MEDHA_API_KEY+x}" ]; then printf absent; else printf leaked; fi"#;
        let (program, _) = sandbox::exec::shell_argv("host", "");
        let program = program.to_ascii_lowercase();
        let name = program.rsplit(['/', '\\']).next().unwrap_or_default();
        match name.strip_suffix(".exe").unwrap_or(name) {
            "cmd" => "if defined MEDHA_API_KEY (echo leaked) else (echo absent)",
            "powershell" | "pwsh" => {
                "if ($env:MEDHA_API_KEY) { Write-Output leaked } else { Write-Output absent }"
            }
            _ => POSIX,
        }
    }

    // Each check must PASS on a good state and FAIL on a bad one — a check that
    // can't fail is worthless (Vol 5 §2).

    #[tokio::test]
    async fn command_check_reads_exit_code() {
        let ws = temp_workspace("gate-command");
        let runner = test_runner(Duration::from_secs(5), 8 * 1024);
        // `exit N` is the one spelling every interpreter this runs under
        // agrees on — sh, bash, PowerShell and cmd alike. The previous
        // Windows arm assumed cmd (`exit /B`), but `shell_argv` prefers bash
        // wherever it exists, which is every Windows image that ships git.
        let (success, failure) = ("exit 0", "exit 1");
        let (ok, _) = run_command(&runner, &ws, success, 0, None).await;
        assert!(ok);
        let (bad, _) = run_command(&runner, &ws, failure, 0, None).await;
        assert!(!bad);
        // stdout substring gate
        let (has, _) = run_command(&runner, &ws, "echo hello", 0, Some("hello")).await;
        assert!(has);
        let (miss, _) = run_command(&runner, &ws, "echo hello", 0, Some("goodbye")).await;
        assert!(!miss);
        std::fs::remove_dir_all(ws).ok();
    }

    #[tokio::test]
    async fn tool_used_and_not_used_scan_intents() {
        let runner = test_runner(Duration::from_secs(1), 1024);
        let events = vec![intent_event("fs.edit"), intent_event("shell.exec")];
        let used = eval_one(
            &Check::ToolUsed("fs.edit".into()),
            &art(".".into(), ".".into(), events.clone()),
            &runner,
        )
        .await;
        assert!(used.passed);
        let unused = eval_one(
            &Check::ToolNotUsed("web.fetch".into()),
            &art(".".into(), ".".into(), events.clone()),
            &runner,
        )
        .await;
        assert!(unused.passed);
        // and it fails when the tool WAS used
        let should_fail = eval_one(
            &Check::ToolNotUsed("shell.exec".into()),
            &art(".".into(), ".".into(), events),
            &runner,
        )
        .await;
        assert!(!should_fail.passed);
    }

    #[tokio::test]
    async fn event_absent_catches_dangerous_pattern() {
        let runner = test_runner(Duration::from_secs(1), 1024);
        let clean = vec![intent_event("fs.read")];
        let ok = eval_one(
            &Check::EventAbsent {
                kind: "policy".into(),
                contains: "dangerous_pattern".into(),
            },
            &art(".".into(), ".".into(), clean),
            &runner,
        )
        .await;
        assert!(ok.passed);

        let dirty = vec![policy_deny("blocked: dangerous_pattern rm -rf")];
        let caught = eval_one(
            &Check::EventAbsent {
                kind: "policy".into(),
                contains: "dangerous_pattern".into(),
            },
            &art(".".into(), ".".into(), dirty),
            &runner,
        )
        .await;
        assert!(
            !caught.passed,
            "a dangerous_pattern policy deny must fail event_absent"
        );
    }

    #[tokio::test]
    async fn unchanged_and_changed_diff_against_pristine() {
        let runner = test_runner(Duration::from_secs(1), 1024);
        let root = std::env::temp_dir().join(format!("gate-diff-{}", ulid::Ulid::new()));
        let pristine = root.join("p");
        let workspace = root.join("w");
        std::fs::create_dir_all(pristine.join("tests")).unwrap();
        std::fs::create_dir_all(workspace.join("tests")).unwrap();
        // identical test file, but a changed source file
        std::fs::write(pristine.join("tests/t.sh"), "assert 4").unwrap();
        std::fs::write(workspace.join("tests/t.sh"), "assert 4").unwrap();
        std::fs::write(pristine.join("src.sh"), "return 5").unwrap();
        std::fs::write(workspace.join("src.sh"), "return 4").unwrap();

        let a = art(workspace.clone(), pristine.clone(), vec![]);
        assert!(
            eval_one(
                &Check::Unchanged {
                    pattern: "tests/**".into(),
                    allow_zero_matches: false,
                },
                &a,
                &runner,
            )
            .await
            .passed,
            "tests/ untouched"
        );
        assert!(
            !eval_one(
                &Check::Unchanged {
                    pattern: "*.sh".into(),
                    allow_zero_matches: false,
                },
                &a,
                &runner,
            )
            .await
            .passed,
            "src.sh changed"
        );
        assert!(
            eval_one(
                &Check::Changed {
                    pattern: "src.sh".into(),
                    allow_zero_matches: false,
                },
                &a,
                &runner,
            )
            .await
            .passed,
            "src.sh did change"
        );
        assert!(
            !eval_one(
                &Check::Changed {
                    pattern: "tests/**".into(),
                    allow_zero_matches: false,
                },
                &a,
                &runner,
            )
            .await
            .passed,
            "tests/ did not change"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn zero_match_globs_fail_closed_unless_explicitly_allowed() {
        let runner = test_runner(Duration::from_secs(1), 1024);
        let root = temp_workspace("gate-zero-match-runtime");
        let pristine = root.join("p");
        let workspace = root.join("w");
        std::fs::create_dir_all(&pristine).unwrap();
        std::fs::create_dir_all(workspace.join("generated")).unwrap();
        std::fs::write(workspace.join("generated/result.txt"), "new").unwrap();
        let artifact = art(workspace, pristine, vec![]);

        let forbidden = eval_one(
            &Check::Unchanged {
                pattern: "missing/**".into(),
                allow_zero_matches: false,
            },
            &artifact,
            &runner,
        )
        .await;
        assert!(!forbidden.passed);
        assert_eq!(forbidden.baseline_matches, Some(0));
        assert_eq!(forbidden.validation, ValidationStatus::Validated);
        assert!(forbidden.detail.contains("zero baseline"));

        let intentional = eval_one(
            &Check::Changed {
                pattern: "generated/**".into(),
                allow_zero_matches: true,
            },
            &artifact,
            &runner,
        )
        .await;
        assert!(intentional.passed);
        assert_eq!(intentional.baseline_matches, Some(0));
        assert_eq!(intentional.workspace_matches, Some(1));
        assert_eq!(
            intentional.normalized_target.as_deref(),
            Some("generated/**")
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn mutated_invalid_checks_fail_instead_of_inspecting_host_paths() {
        let runner = test_runner(Duration::from_secs(1), 1024);
        let root = temp_workspace("gate-invalid-runtime-check");
        let pristine = root.join("p");
        let workspace = root.join("w");
        std::fs::create_dir_all(&pristine).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let artifact = art(workspace, pristine, vec![]);

        for check in [
            Check::Exists("/etc/passwd".into()),
            Check::Absent("../outside".into()),
            Check::Exists("C:/Windows/system.ini".into()),
            Check::Absent(r"\\server\share\secret".into()),
            Check::Unchanged {
                pattern: "../**".into(),
                allow_zero_matches: true,
            },
        ] {
            let result = eval_one(&check, &artifact, &runner).await;
            assert!(!result.passed, "{} unexpectedly passed", check.label());
            assert_eq!(result.validation, ValidationStatus::Invalid);
            assert!(result.detail.contains("validation failed"));
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_created_symlinks_cannot_escape_literal_or_glob_checks() {
        use std::os::unix::fs::symlink;

        let runner = test_runner(Duration::from_secs(1), 1024);
        let root = temp_workspace("gate-check-symlink");
        let pristine = root.join("p");
        let workspace = root.join("w");
        let outside = root.join("outside.txt");
        std::fs::create_dir_all(&pristine).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(&outside, "host-secret").unwrap();
        symlink(&outside, workspace.join("link.txt")).unwrap();
        let artifact = art(workspace, pristine, vec![]);

        for check in [
            Check::Exists("link.txt".into()),
            Check::Absent("link.txt".into()),
            Check::Changed {
                pattern: "**".into(),
                allow_zero_matches: true,
            },
        ] {
            let result = eval_one(&check, &artifact, &runner).await;
            assert!(!result.passed, "{} unexpectedly passed", check.label());
            assert_eq!(result.validation, ValidationStatus::Invalid);
            assert!(result.detail.contains("symbolic link"), "{result:?}");
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn command_environment_strips_provider_secrets() {
        let ws = temp_workspace("gate-env");
        let runner =
            CommandRunner::for_test(Arc::new(SecretBearingHost), Duration::from_secs(5), 1024);
        let command = secret_probe_command();
        let out = execute_command(&runner, &ws, command).await.unwrap();
        assert_eq!(out.status, Some(0));
        assert!(out.output.contains("absent"));
        assert!(!out.output.contains("leaked"));
        std::fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn container_environment_has_only_fixed_safe_search_paths() {
        let ws = temp_workspace("gate-container-env");
        let check_env = CheckEnvironment::create(&ws).unwrap();
        let environment = check_env.environment("container");
        let keys: Vec<_> = environment.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["HOME", "TMPDIR", "LANG", "LC_ALL", "PATH"]);
        let path = environment
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.as_str())
            .unwrap();
        assert_eq!(
            path,
            "/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        );
        std::fs::remove_dir_all(ws).ok();
    }

    #[tokio::test]
    async fn unconfined_backend_fails_closed_without_executing() {
        let root = temp_workspace("gate-contained");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let outside_marker = root.join("outside-marker");
        let runner = CommandRunner::from_sandbox(&SandboxConfig {
            backend: BackendKind::Host,
            ..SandboxConfig::default()
        });
        let command = format!("printf escaped > \"{}\"", outside_marker.display());
        let (passed, detail) = run_command(&runner, &workspace, &command, 0, None).await;
        assert!(!passed);
        assert!(detail.contains("host backend has no filesystem/network isolation"));
        assert!(
            !outside_marker.exists(),
            "a disabled command check still reached the host"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn repository_cannot_turn_image_name_into_runtime_flags() {
        for image in ["--privileged", "image --volume /:/host", "bad\nimage"] {
            assert!(validate_container_image(image).is_err(), "{image:?}");
        }
        for image in [
            "rust:1",
            "ghcr.io/example/check@sha256:abc123",
            "local/check_image",
        ] {
            assert!(validate_container_image(image).is_ok(), "{image:?}");
        }
    }

    #[test]
    fn container_cleanup_argv_is_forced_and_name_scoped() {
        assert_eq!(
            container_cleanup_args("medha-gate-01test"),
            ["rm", "-f", "-v", "medha-gate-01test"].map(String::from)
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_container_lease_invokes_forced_removal() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_workspace("gate-container-cleanup");
        let runtime = root.join("fake-runtime");
        let log = root.join("cleanup-argv");
        std::fs::write(
            &runtime,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        {
            let _lease = ContainerLease::new(runtime, "medha-gate-drop-test".into());
        }
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "rm\n-f\n-v\nmedha-gate-drop-test\n"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uncertain_create_absence_keeps_retrying_cleanup() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_workspace("gate-container-late-register");
        let runtime = root.join("fake-runtime");
        let attempts = root.join("attempts");
        std::fs::write(
            &runtime,
            format!(
                r#"#!/bin/sh
printf 'rm\n' >> "{attempts}"
count=$(wc -l < "{attempts}")
if [ "$count" -eq 1 ]; then
  printf 'Error: No such container\n' >&2
  exit 1
fi
exit 0
"#,
                attempts = attempts.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut lease = ContainerLease::new(runtime, "medha-gate-late-test".into());
        lease.cleanup().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&attempts).unwrap(),
            "rm\nrm\n",
            "an unconfirmed create must not trust the first absent response"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_container_check_finishes_owned_cleanup_without_a_survivor() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_workspace("gate-container-cancel");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let runtime = root.join("fake-runtime");
        let log = root.join("lifecycle");
        let registered = root.join("registered");
        let running = root.join("running");
        let removed = root.join("removed");
        let survivor = root.join("survivor");
        std::fs::write(
            &runtime,
            format!(
                r#"#!/bin/sh
case "$1" in
  create)
    previous=
    name=
    for argument in "$@"; do
      if [ "$previous" = "--name" ]; then name=$argument; break; fi
      previous=$argument
    done
    printf 'create:%s\n' "$name" >> "{log}"
    : > "{registered}"
    ;;
  start)
    printf 'start:%s\n' "$3" >> "{log}"
    : > "{running}"
    sleep 30
    ;;
  rm)
    printf 'rm:%s\n' "$4" >> "{log}"
    rm -f "{registered}" "{running}"
    : > "{removed}"
    ;;
  *)
    exit 64
    ;;
esac
"#,
                log = log.display(),
                registered = registered.display(),
                running = running.display(),
                removed = removed.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();

        let runner = CommandRunner::for_container_test(runtime, Duration::from_secs(10), 8 * 1024);
        let mut execution = Box::pin(execute_command(&runner, &workspace, "printf checked"));
        // These deadlines wait for a spawned `/bin/sh` to reach a state; they are
        // liveness bounds, not the assertion. A shared CI runner executing the
        // rest of the suite in parallel takes far longer than a quiet machine to
        // get a process scheduled, and a deadline tuned to the quiet case fails
        // for load rather than for a defect. Budget for the slow machine — the
        // marker assertions below are what catch a real regression.
        let observe_start = async {
            tokio::time::timeout(Duration::from_secs(30), async {
                while !running.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
        };
        tokio::pin!(observe_start);
        tokio::select! {
            result = &mut execution => {
                panic!("container check finished before cancellation: {result:?}");
            }
            observed = &mut observe_start => {
                observed.expect("fake runtime never reached start");
            }
        }

        // This watcher models daemon-owned work: if the running marker remains
        // after cancellation, it leaves an externally visible survivor.
        let watcher_running = running.clone();
        let watcher_survivor = survivor.clone();
        let watcher = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if watcher_running.exists() {
                std::fs::write(watcher_survivor, b"survived").unwrap();
            }
        });
        drop(execution);

        tokio::time::timeout(Duration::from_secs(30), async {
            while !removed.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("detached lifecycle never removed the container");
        watcher.await.unwrap();

        assert!(!registered.exists(), "registered container survived");
        assert!(!running.exists(), "running workload marker survived");
        assert!(!survivor.exists(), "cancelled workload left a survivor");
        let entries: Vec<_> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(entries.len(), 3, "unexpected lifecycle: {entries:?}");
        let name = entries[0]
            .strip_prefix("create:")
            .expect("first phase was not create");
        assert_eq!(entries[1], format!("start:{name}"));
        assert_eq!(entries[2], format!("rm:{name}"));
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_during_create_never_starts_the_workload() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_workspace("gate-container-create-cancel");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let runtime = root.join("fake-runtime");
        let log = root.join("lifecycle");
        let creating = root.join("creating");
        let registered = root.join("registered");
        let removed = root.join("removed");
        std::fs::write(
            &runtime,
            format!(
                r#"#!/bin/sh
case "$1" in
  create)
    printf 'create\n' >> "{log}"
    : > "{creating}"
    sleep 1
    : > "{registered}"
    ;;
  start)
    printf 'start\n' >> "{log}"
    ;;
  rm)
    printf 'rm\n' >> "{log}"
    rm -f "{registered}"
    : > "{removed}"
    ;;
  *)
    exit 64
    ;;
esac
"#,
                log = log.display(),
                creating = creating.display(),
                registered = registered.display(),
                removed = removed.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();

        let runner = CommandRunner::for_container_test(runtime, Duration::from_secs(10), 8 * 1024);
        let mut execution = Box::pin(execute_command(&runner, &workspace, "printf forbidden"));
        let observe_create = async {
            tokio::time::timeout(Duration::from_secs(30), async {
                while !creating.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
        };
        tokio::pin!(observe_create);
        tokio::select! {
            result = &mut execution => {
                panic!("container check finished before cancellation: {result:?}");
            }
            observed = &mut observe_create => {
                observed.expect("fake runtime never reached create");
            }
        }
        drop(execution);

        tokio::time::timeout(Duration::from_secs(30), async {
            while !removed.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("create did not settle and clean up after cancellation");
        assert!(!registered.exists(), "registered container survived");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "create\nrm\n",
            "cancellation during inert create must never reach start"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_output_is_capped_while_process_is_drained() {
        let ws = temp_workspace("gate-output-cap");
        let runner = test_runner(Duration::from_secs(5), 1024);
        let out = execute_command(
            &runner,
            &ws,
            "i=0; while [ \"$i\" -lt 5000 ]; do printf 0123456789; i=$((i+1)); done",
        )
        .await
        .unwrap();
        assert_eq!(out.status, Some(0));
        assert!(
            out.output.len() <= 1024 + 64,
            "retained output exceeded its cap: {} bytes",
            out.output.len()
        );
        assert!(out.output.contains("earlier output dropped"));
        std::fs::remove_dir_all(ws).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_timeout_reaps_descendant_processes() {
        let ws = temp_workspace("gate-timeout-tree");
        let marker = ws.join("survived");
        let release = ws.join("release");
        let runner = test_runner(Duration::from_millis(100), 1024);
        let command = format!(
            "(while [ ! -e \"{}\" ]; do sleep 0.05; done; printf survived > \"{}\") & wait",
            release.display(),
            marker.display()
        );
        let out = execute_command(&runner, &ws, &command).await.unwrap();
        assert!(out.timed_out);
        // The descendant cannot mutate before this point, even if the test
        // runtime was CPU-starved past the nominal 100 ms deadline. If teardown
        // missed it, releasing the barrier now makes that survivor observable.
        std::fs::write(&release, b"go").unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!marker.exists(), "a timed-out descendant survived");
        std::fs::remove_dir_all(ws).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_command_check_reaps_descendant_processes() {
        let ws = temp_workspace("gate-cancel-tree");
        let marker = ws.join("survived");
        let release = ws.join("release");
        let runner = test_runner(Duration::from_secs(5), 1024);
        let command = format!(
            "(while [ ! -e \"{}\" ]; do sleep 0.05; done; printf survived > \"{}\") & wait",
            release.display(),
            marker.display()
        );
        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            execute_command(&runner, &ws, &command),
        )
        .await;
        assert!(cancelled.is_err(), "outer cancellation should win");
        std::fs::write(&release, b"go").unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!marker.exists(), "a cancelled descendant survived");
        std::fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn kind_prefix_matching() {
        assert!(kind_matches("policy.decision", "policy"));
        assert!(kind_matches("tool.observation", "tool"));
        assert!(kind_matches("policy.decision", "policy.decision"));
        assert!(!kind_matches("model.text", "policy"));
    }
}
