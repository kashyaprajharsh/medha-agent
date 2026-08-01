//! Hermetic scenario runner (Vol 5 §5 isolation).
//!
//! Each run gets a throwaway workspace (the fixture, copied in), a throwaway
//! `MEDHA_HOME` (so its event log is isolated and never touches the operator's
//! real `~/.medha`), and the scenario's contract as budget env. We spawn the
//! *real* `medha` binary — black-box, end-to-end, exactly as a user runs it —
//! then read back the run's event log for scoring. This deliberately does not
//! re-assemble the kernel in-process: the gate tests the shipped artifact.

use kernel::{Event, EventLog};
use sandbox::run_command_bounded;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use store::SqliteLog;
use tokio_util::sync::CancellationToken;
#[cfg(test)]
use ulid::Ulid;

use crate::GateError;
use crate::checks::RunArtifact;
use crate::scenario::{Scenario, validate_gate_wall_seconds};
use crate::verdict::RunStatus;

/// Fixtures are expected to be compact source trees. These ceilings prevent a
/// malformed scenario or concurrently growing file from consuming arbitrary
/// disk while Gate makes its two snapshots.
const MAX_FIXTURE_ENTRIES: u64 = 100_000;
const MAX_FIXTURE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_FIXTURE_DEPTH: usize = 128;
/// Agent output is useful for draining its process tree, but Gate scores the
/// durable log and deterministic checks rather than retaining unbounded text.
const MAX_AGENT_OUTPUT: usize = 1024 * 1024;
const GATE_WALL_GRACE_SECS: u64 = 30;

/// Dropping a Gate run cancels, but does not abort, the independently owned
/// supervisor. The task retains the child handle until it has killed and reaped
/// the process tree, so caller cancellation cannot detach descendants.
struct CancelAgentOnDrop(CancellationToken);

impl Drop for CancelAgentOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PreserveRuns {
    /// Remove every run tree after scoring.
    #[default]
    Never,
    /// Keep setup errors, abnormal runs, and deterministic check failures.
    Failures,
    /// Keep every run tree for interactive debugging.
    Always,
}

impl PreserveRuns {
    pub(crate) fn should_preserve(self, failed: bool) -> bool {
        matches!(self, Self::Always) || (failed && matches!(self, Self::Failures))
    }
}

/// How to launch a run. Built by the caller (which owns provider resolution) so
/// the gate crate stays free of config/keychain concerns.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// The `medha` binary to run (normally the running executable itself).
    pub binary: PathBuf,
    /// Provider env injected into the child (`MEDHA_BASE_URL` / `MEDHA_MODEL` /
    /// `MEDHA_API_KEY`), so the child resolves a model without needing the
    /// operator's `config.toml` — which the isolated `MEDHA_HOME` hides.
    pub provider_env: Vec<(String, String)>,
    /// Backstop wall-clock ceiling (seconds) when the scenario contract sets none.
    pub default_wall_s: u64,
    /// Execution isolation used for repository-authored command checks. Gate
    /// tightens this to no-network and fails closed unless the backend provides
    /// a hermetic filesystem boundary.
    pub check_sandbox: sandbox::SandboxConfig,
    /// Explicit artifact retention policy. The safe/default path removes all
    /// run state as soon as scoring finishes.
    pub preserve: PreserveRuns,
}

/// Run a scenario once, hermetically, and return its artifact for scoring.
pub async fn run_once(scn: &Scenario, cfg: &RunConfig) -> Result<RunArtifact, GateError> {
    run_once_at(scn, cfg, std::env::temp_dir(), None).await
}

/// Give the complete run—including its temporary directory—to an owned task.
///
/// If the caller drops `run_once`, the guard cancels the child but the task
/// retains both the process supervisor and `TempDir` until kill/reap finishes.
/// This ordering is essential on Windows, where deleting a live process's
/// current directory otherwise fails and leaks the tree.
async fn run_once_at(
    scn: &Scenario,
    cfg: &RunConfig,
    temp_parent: PathBuf,
    hard_wall_override: Option<Duration>,
) -> Result<RunArtifact, GateError> {
    let hard_wall = match hard_wall_override {
        Some(wall) => wall,
        None => gate_wall_timeout(scn, cfg)?,
    };
    let cancellation = CancellationToken::new();
    let cancel_on_drop = CancelAgentOnDrop(cancellation.clone());
    let scenario = scn.clone();
    let config = cfg.clone();
    let owner = tokio::spawn(async move {
        run_once_owned(&scenario, &config, &temp_parent, hard_wall, cancellation).await
    });
    let result = owner
        .await
        .map_err(|error| GateError::Run(format!("Gate run owner task failed: {error}")))?;
    drop(cancel_on_drop);
    result
}

async fn run_once_owned(
    scn: &Scenario,
    cfg: &RunConfig,
    temp_parent: &Path,
    hard_wall: Duration,
    cancellation: CancellationToken,
) -> Result<RunArtifact, GateError> {
    let mut run_dir = Some(
        tempfile::Builder::new()
            .prefix("medha-gate-")
            .tempdir_in(temp_parent)
            .map_err(|error| {
                GateError::Run(format!(
                    "create Gate run directory under {}: {error}",
                    temp_parent.display()
                ))
            })?,
    );
    let root = run_dir
        .as_ref()
        .expect("run directory exists")
        .path()
        .to_path_buf();
    let workspace = root.join("workspace");
    let pristine = root.join("pristine");
    let home = root.join("home");
    let result: Result<(Vec<Event>, RunStatus, u128), GateError> = async {
        let fixture = scn.validated_fixture_dir().map_err(GateError::Scenario)?;
        // Snapshot the repository-authored fixture exactly once, then derive the
        // mutable workspace from that validated snapshot. A concurrent source
        // change can no longer make the "before" and "after" baselines disagree.
        copy_dir(&fixture, &pristine)?;
        copy_dir(&pristine, &workspace)?;
        std::fs::create_dir_all(&home).map_err(|e| GateError::Run(format!("mkdir home: {e}")))?;

        let mut cmd = tokio::process::Command::new(&cfg.binary);
        cmd.arg(&scn.task)
            .current_dir(&workspace)
            .env("MEDHA_HOME", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &cfg.provider_env {
            cmd.env(k, v);
        }
        // Eval runs are unattended — there is no human to answer an approval
        // prompt, so the agent must run autonomously.
        cmd.env("MEDHA_APPROVE", "none");
        if let Some(t) = scn.contract.max_turns {
            cmd.env("MEDHA_MAX_TURNS", t.to_string());
        }
        if let Some(t) = scn.contract.max_tokens {
            cmd.env("MEDHA_MAX_TOKENS", t.to_string());
        }
        if let Some(c) = scn.contract.max_cost_usd {
            cmd.env("MEDHA_MAX_COST", c.to_string());
        }
        if let Some(w) = scn.contract.max_wall_s {
            cmd.env("MEDHA_MAX_WALL", w.to_string());
        }

        // The child enforces MEDHA_MAX_WALL; this validated duration is the
        // hard process-tree backstop with a fixed grace period.
        let start = Instant::now();
        let status = supervise_agent_command_owned(cmd, hard_wall, &cancellation).await;
        let wall_ms = start.elapsed().as_millis();
        let events = load_events(&home).await;
        Ok((events, status, wall_ms))
    }
    .await;

    match result {
        Ok((events, status, wall_ms)) => {
            let failed = !status.is_success();
            let mut artifact = RunArtifact {
                workspace,
                pristine,
                events,
                status,
                wall_ms,
                run_dir,
                preserved_path: None,
            };
            if cfg.preserve.should_preserve(failed) {
                artifact.preserve();
            }
            Ok(artifact)
        }
        Err(error) => {
            if cfg.preserve.should_preserve(true)
                && let Some(directory) = run_dir.take()
            {
                let path = directory.keep();
                return Err(GateError::Run(format!(
                    "{error}; Gate run artifacts kept at {}",
                    path.display()
                )));
            }
            Err(error)
        }
    }
}

fn gate_wall_timeout(scn: &Scenario, cfg: &RunConfig) -> Result<Duration, GateError> {
    validate_gate_wall_seconds(cfg.default_wall_s).map_err(|error| {
        GateError::Options(format!(
            "invalid Gate default wall limit {}: {error}",
            cfg.default_wall_s
        ))
    })?;
    let requested = scn.contract.max_wall_s.unwrap_or(cfg.default_wall_s);
    validate_gate_wall_seconds(requested).map_err(|error| {
        GateError::Scenario(format!(
            "scenario {:?} `contract.max_wall_s` {error}",
            scn.id
        ))
    })?;
    Duration::from_secs(requested)
        .checked_add(Duration::from_secs(GATE_WALL_GRACE_SECS))
        .ok_or_else(|| {
            GateError::Options(format!(
                "Gate wall limit {requested} seconds plus {GATE_WALL_GRACE_SECS} seconds grace \
                 is not representable"
            ))
        })
}

/// Run the agent in an owned process group/tree and settle it before returning.
///
/// `run_command_bounded` keeps the direct child unreaped while killing its
/// group, which prevents PID reuse from redirecting a late kill. On Windows it
/// uses a new process group plus `taskkill /T`; on Unix it uses `killpg`.
#[cfg(test)]
async fn supervise_agent_command(command: tokio::process::Command, wall: Duration) -> RunStatus {
    let cancellation = CancellationToken::new();
    let cancel_on_drop = CancelAgentOnDrop(cancellation.clone());
    let supervisor =
        tokio::spawn(
            async move { supervise_agent_command_owned(command, wall, &cancellation).await },
        );
    let status = match supervisor.await {
        Ok(status) => status,
        Err(error) => {
            return RunStatus::LaunchError(format!("agent supervisor task failed: {error}"));
        }
    };
    drop(cancel_on_drop);
    status
}

async fn supervise_agent_command_owned(
    command: tokio::process::Command,
    wall: Duration,
    cancellation: &CancellationToken,
) -> RunStatus {
    let outcome = run_command_bounded(command, wall, MAX_AGENT_OUTPUT, Some(cancellation)).await;
    match outcome {
        Ok(output) if output.timed_out => RunStatus::TimedOut,
        Ok(output) if output.cancelled => RunStatus::Cancelled,
        Ok(output) => match output.status {
            Some(0) => RunStatus::Succeeded,
            Some(code) => RunStatus::ExitCode(code),
            None => RunStatus::Signaled,
        },
        Err(error) => RunStatus::LaunchError(error.to_string()),
    }
}

/// Read the run's event log back from its isolated home. The unique temp home
/// contains exactly one project → one `events.db` → one session, so there is no
/// ambiguity about which run we're scoring. A missing/empty log yields no events
/// (checks then fail honestly rather than the gate erroring).
async fn load_events(home: &Path) -> Vec<Event> {
    let pattern = format!("{}/projects/*/events.db", home.display());
    let Some(db) = glob::glob(&pattern)
        .ok()
        .and_then(|mut it| it.find_map(|p| p.ok()))
    else {
        return Vec::new();
    };
    let Ok(log) = SqliteLog::open(&db) else {
        return Vec::new();
    };
    let sessions = log.sessions().await;
    let Some(latest) = sessions.iter().max_by(|a, b| {
        a.last_ts
            .partial_cmp(&b.last_ts)
            .unwrap_or(std::cmp::Ordering::Equal)
    }) else {
        return Vec::new();
    };
    log.events(latest.id).await
}

#[derive(Default)]
struct CopyBudget {
    entries: u64,
    bytes: u64,
}

impl CopyBudget {
    fn add_entry(&mut self, source: &Path) -> Result<(), GateError> {
        self.entries = self.entries.saturating_add(1);
        if self.entries > MAX_FIXTURE_ENTRIES {
            return Err(GateError::Run(format!(
                "fixture exceeds {MAX_FIXTURE_ENTRIES} entries while copying {}",
                source.display()
            )));
        }
        Ok(())
    }

    fn remaining_bytes(&self) -> u64 {
        MAX_FIXTURE_BYTES.saturating_sub(self.bytes)
    }

    fn add_bytes(&mut self, copied: u64, source: &Path) -> Result<(), GateError> {
        self.bytes = self.bytes.saturating_add(copied);
        if self.bytes > MAX_FIXTURE_BYTES {
            return Err(GateError::Run(format!(
                "fixture exceeds {} bytes while copying {}",
                MAX_FIXTURE_BYTES,
                source.display()
            )));
        }
        Ok(())
    }
}

/// Recursively copy a fixture without following links or accepting filesystem
/// objects that do not have ordinary file/directory semantics.
fn copy_dir(from: &Path, to: &Path) -> Result<(), GateError> {
    let metadata = std::fs::symlink_metadata(from)
        .map_err(|e| GateError::Run(format!("inspect {}: {e}", from.display())))?;
    if metadata.file_type().is_symlink() {
        return Err(unsupported_fixture_entry(from, "symbolic link"));
    }
    if !metadata.is_dir() {
        return Err(unsupported_fixture_entry(from, file_kind(&metadata)));
    }
    let source_root = from
        .canonicalize()
        .map_err(|e| GateError::Run(format!("resolve fixture {}: {e}", from.display())))?;
    let mut budget = CopyBudget::default();
    copy_dir_inner(&source_root, from, to, &mut budget, 0)
}

fn copy_dir_inner(
    source_root: &Path,
    from: &Path,
    to: &Path,
    budget: &mut CopyBudget,
    depth: usize,
) -> Result<(), GateError> {
    if depth > MAX_FIXTURE_DEPTH {
        return Err(GateError::Run(format!(
            "fixture exceeds maximum directory depth {MAX_FIXTURE_DEPTH}: {}",
            from.display()
        )));
    }
    verify_source_is_beneath(source_root, from)?;
    if to.exists() {
        return Err(GateError::Run(format!(
            "fixture destination already exists: {}",
            to.display()
        )));
    }
    std::fs::create_dir_all(to)
        .map_err(|e| GateError::Run(format!("mkdir {}: {e}", to.display())))?;
    let entries = std::fs::read_dir(from)
        .map_err(|e| GateError::Run(format!("read {}: {e}", from.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| GateError::Run(e.to_string()))?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        budget.add_entry(&src)?;
        let metadata = std::fs::symlink_metadata(&src)
            .map_err(|e| GateError::Run(format!("inspect {}: {e}", src.display())))?;
        let kind = metadata.file_type();
        if kind.is_symlink() {
            return Err(unsupported_fixture_entry(&src, "symbolic link"));
        }
        if kind.is_dir() {
            copy_dir_inner(source_root, &src, &dst, budget, depth + 1)?;
            std::fs::set_permissions(&dst, metadata.permissions()).map_err(|e| {
                GateError::Run(format!("set permissions on {}: {e}", dst.display()))
            })?;
        } else if kind.is_file() {
            verify_source_is_beneath(source_root, &src)?;
            copy_regular_file(&src, &dst, &metadata, budget)?;
        } else {
            return Err(unsupported_fixture_entry(&src, file_kind(&metadata)));
        }
    }
    Ok(())
}

fn verify_source_is_beneath(source_root: &Path, source: &Path) -> Result<(), GateError> {
    let resolved = source
        .canonicalize()
        .map_err(|e| GateError::Run(format!("resolve fixture entry {}: {e}", source.display())))?;
    if resolved.starts_with(source_root) {
        Ok(())
    } else {
        Err(GateError::Run(format!(
            "fixture entry resolves outside {}: {}",
            source_root.display(),
            source.display()
        )))
    }
}

fn copy_regular_file(
    source: &Path,
    destination: &Path,
    metadata: &std::fs::Metadata,
    budget: &mut CopyBudget,
) -> Result<(), GateError> {
    let remaining = budget.remaining_bytes();
    if metadata.len() > remaining {
        return Err(GateError::Run(format!(
            "fixture exceeds {MAX_FIXTURE_BYTES} bytes at {}",
            source.display()
        )));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself. Its handle metadata then fails the
        // ordinary-file check below instead of following it to a host file.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let source_file = options
        .open(source)
        .map_err(|e| GateError::Run(format!("open fixture file {}: {e}", source.display())))?;
    if !source_file
        .metadata()
        .map_err(|e| GateError::Run(format!("inspect open file {}: {e}", source.display())))?
        .is_file()
    {
        return Err(unsupported_fixture_entry(source, "non-regular file"));
    }

    let mut destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|e| {
            GateError::Run(format!(
                "create fixture copy {}: {e}",
                destination.display()
            ))
        })?;
    let copied = std::io::copy(
        &mut source_file.take(remaining.saturating_add(1)),
        &mut destination_file,
    )
    .map_err(|e| GateError::Run(format!("copy {}: {e}", source.display())))?;
    if copied > remaining {
        drop(destination_file);
        let _ = std::fs::remove_file(destination);
        return Err(GateError::Run(format!(
            "fixture exceeds {MAX_FIXTURE_BYTES} bytes while copying {}",
            source.display()
        )));
    }
    destination_file
        .flush()
        .map_err(|e| GateError::Run(format!("flush {}: {e}", destination.display())))?;
    std::fs::set_permissions(destination, metadata.permissions()).map_err(|e| {
        GateError::Run(format!("set permissions on {}: {e}", destination.display()))
    })?;
    budget.add_bytes(copied, source)
}

fn unsupported_fixture_entry(path: &Path, kind: &str) -> GateError {
    GateError::Run(format!(
        "unsupported fixture entry ({kind}); only directories and regular files are allowed: {}",
        path.display()
    ))
}

fn file_kind(metadata: &std::fs::Metadata) -> &'static str {
    let kind = metadata.file_type();
    if kind.is_symlink() {
        "symbolic link"
    } else if kind.is_dir() {
        "directory"
    } else if kind.is_file() {
        "regular file"
    } else {
        "special file"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::{Check, Contract, MAX_GATE_WALL_SECS};

    fn temp_dir(prefix: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", Ulid::new()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_scenario(base_dir: PathBuf) -> Scenario {
        std::fs::create_dir_all(base_dir.join("fixture")).unwrap();
        std::fs::write(base_dir.join("fixture").join("input.txt"), "fixture").unwrap();
        Scenario {
            id: "cleanup".into(),
            task: "run::tests::process_tree_helper".into(),
            fixture: "fixture".into(),
            contract: Contract::default(),
            checks: vec![Check::ToolNotUsed("unused".into())],
            labels: Vec::new(),
            base_dir,
        }
    }

    fn test_run_config(mode: &str, preserve: PreserveRuns) -> RunConfig {
        RunConfig {
            binary: std::env::current_exe().unwrap(),
            provider_env: vec![("MEDHA_GATE_TEST_HELPER".into(), mode.into())],
            default_wall_s: 30,
            check_sandbox: sandbox::SandboxConfig::default(),
            preserve,
        }
    }

    fn run_directories(parent: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("medha-gate-"))
            })
            .collect()
    }

    async fn wait_until_no_run_directories(parent: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !run_directories(parent).is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Gate run directory was not cleaned");
    }

    #[test]
    fn wall_deadline_maximum_adds_grace_without_panicking_or_shortening() {
        let root = tempfile::tempdir().unwrap();
        let mut scenario = test_scenario(root.path().join("scenario"));
        scenario.contract.max_wall_s = Some(MAX_GATE_WALL_SECS);
        let config = test_run_config("exit-0", PreserveRuns::Never);
        let deadline = gate_wall_timeout(&scenario, &config).unwrap();
        assert_eq!(
            deadline,
            Duration::from_secs(MAX_GATE_WALL_SECS + GATE_WALL_GRACE_SECS)
        );
        assert!(deadline > Duration::from_secs(MAX_GATE_WALL_SECS));

        for invalid in [0, MAX_GATE_WALL_SECS + 1, u64::MAX] {
            scenario.contract.max_wall_s = Some(invalid);
            assert!(
                gate_wall_timeout(&scenario, &config).is_err(),
                "invalid wall limit {invalid} reached deadline arithmetic"
            );
        }

        scenario.contract.max_wall_s = None;
        let mut invalid_default = config;
        invalid_default.default_wall_s = u64::MAX;
        assert!(gate_wall_timeout(&scenario, &invalid_default).is_err());
    }

    #[tokio::test]
    async fn invalid_wall_limit_fails_before_tempdir_or_process_creation() {
        let root = tempfile::tempdir().unwrap();
        let runs = root.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        let mut scenario = test_scenario(root.path().join("scenario"));
        scenario.contract.max_wall_s = Some(u64::MAX);
        let mut config = test_run_config("exit-0", PreserveRuns::Never);
        config.binary = PathBuf::from("must-not-launch");
        let error = run_once_at(&scenario, &config, runs.clone(), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("max_wall_s"), "{error}");
        assert!(run_directories(&runs).is_empty());
    }

    #[tokio::test]
    async fn completed_failed_timed_out_and_panicked_children_clean_run_directories() {
        for (mode, timeout, expected) in [
            ("exit-0", None, RunStatus::Succeeded),
            ("exit-7", None, RunStatus::ExitCode(7)),
            ("sleep", Some(Duration::ZERO), RunStatus::TimedOut),
        ] {
            let root = tempfile::tempdir().unwrap();
            let scenario = test_scenario(root.path().join("scenario"));
            let runs = root.path().join("runs");
            std::fs::create_dir_all(&runs).unwrap();
            let artifact = run_once_at(
                &scenario,
                &test_run_config(mode, PreserveRuns::Never),
                runs.clone(),
                timeout,
            )
            .await
            .unwrap();
            assert_eq!(artifact.status, expected);
            assert_eq!(run_directories(&runs).len(), 1);
            drop(artifact);
            wait_until_no_run_directories(&runs).await;
        }

        // A panic in the evaluated process is a failed seed, not a leaked
        // harness directory.
        let root = tempfile::tempdir().unwrap();
        let scenario = test_scenario(root.path().join("scenario"));
        let runs = root.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        let artifact = run_once_at(
            &scenario,
            &test_run_config("panic", PreserveRuns::Never),
            runs.clone(),
            None,
        )
        .await
        .unwrap();
        assert!(!artifact.status.is_success());
        drop(artifact);
        wait_until_no_run_directories(&runs).await;
    }

    #[tokio::test]
    async fn setup_errors_and_unwinding_clean_owned_tempdirs() {
        let root = tempfile::tempdir().unwrap();
        let runs = root.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        let mut scenario = test_scenario(root.path().join("scenario"));
        scenario.fixture = "missing".into();
        assert!(
            run_once_at(
                &scenario,
                &test_run_config("exit-0", PreserveRuns::Never),
                runs.clone(),
                None,
            )
            .await
            .is_err()
        );
        wait_until_no_run_directories(&runs).await;

        let panic_result = std::panic::catch_unwind({
            let runs = runs.clone();
            move || {
                let _directory = tempfile::Builder::new()
                    .prefix("medha-gate-")
                    .tempdir_in(runs)
                    .unwrap();
                panic!("injected Gate panic");
            }
        });
        assert!(panic_result.is_err());
        wait_until_no_run_directories(&runs).await;
    }

    #[tokio::test]
    async fn cancelling_run_once_keeps_ownership_until_cleanup_finishes() {
        let root = tempfile::tempdir().unwrap();
        let scenario = test_scenario(root.path().join("scenario"));
        let runs = root.path().join("runs");
        let started = root.path().join("child-started");
        std::fs::create_dir_all(&runs).unwrap();
        let mut config = test_run_config("sleep-marker", PreserveRuns::Never);
        config
            .provider_env
            .push(("MEDHA_GATE_STARTED".into(), started.display().to_string()));
        let mut execution = Box::pin(run_once_at(&scenario, &config, runs.clone(), None));
        tokio::select! {
            result = &mut execution => panic!("run ended before cancellation: {result:?}"),
            observed = tokio::time::timeout(Duration::from_secs(5), async {
                while !started.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }) => observed.expect("child did not start"),
        }
        drop(execution);
        wait_until_no_run_directories(&runs).await;
    }

    #[tokio::test]
    async fn preservation_policy_keeps_only_explicitly_requested_runs() {
        for (mode, policy, timeout) in [
            ("exit-0", PreserveRuns::Always, None),
            ("exit-7", PreserveRuns::Failures, None),
            ("panic", PreserveRuns::Failures, None),
            (
                "sleep",
                PreserveRuns::Failures,
                Some(Duration::from_millis(1)),
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let scenario = test_scenario(root.path().join("scenario"));
            let runs = root.path().join("runs");
            std::fs::create_dir_all(&runs).unwrap();
            let artifact = run_once_at(&scenario, &test_run_config(mode, policy), runs, timeout)
                .await
                .unwrap();
            let kept = artifact
                .preserved_path()
                .expect("requested run was not preserved")
                .to_path_buf();
            drop(artifact);
            assert!(kept.exists(), "preserved run was removed");
            std::fs::remove_dir_all(kept).unwrap();
        }

        let root = tempfile::tempdir().unwrap();
        let runs = root.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        let mut scenario = test_scenario(root.path().join("scenario"));
        scenario.fixture = "missing".into();
        let error = run_once_at(
            &scenario,
            &test_run_config("exit-0", PreserveRuns::Failures),
            runs.clone(),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        let kept = run_directories(&runs);
        assert_eq!(kept.len(), 1, "setup failure was not preserved: {error}");
        assert!(
            error.contains(&kept[0].display().to_string()),
            "retained path was not reported: {error}"
        );
        std::fs::remove_dir_all(&kept[0]).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fixture_copy_rejects_file_and_directory_symlinks_and_cycles() {
        use std::os::unix::fs::symlink;

        for (name, make_link) in [
            ("file-link", "../outside.txt"),
            ("directory-link", "../outside-dir"),
            ("cycle", "."),
        ] {
            let root = temp_dir("gate-copy-link");
            let fixture = root.join("fixture");
            std::fs::create_dir_all(&fixture).unwrap();
            std::fs::write(root.join("outside.txt"), "secret").unwrap();
            std::fs::create_dir_all(root.join("outside-dir")).unwrap();
            symlink(make_link, fixture.join(name)).unwrap();

            let error = copy_dir(&fixture, &root.join("copy"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("symbolic link"), "{name}: {error}");
            std::fs::remove_dir_all(root).ok();
        }
    }

    #[cfg(unix)]
    #[test]
    fn fixture_copy_rejects_fifo_and_device_nodes() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let root = temp_dir("gate-copy-special");
        let fixture = root.join("fixture");
        std::fs::create_dir_all(&fixture).unwrap();
        let fifo = fixture.join("pipe");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        let result = unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) };
        assert_eq!(
            result,
            0,
            "mkfifo failed: {}",
            std::io::Error::last_os_error()
        );
        let fifo_error = copy_dir(&fixture, &root.join("fifo-copy"))
            .unwrap_err()
            .to_string();
        assert!(fifo_error.contains("special file"), "{fifo_error}");

        let device = Path::new("/dev/null");
        let metadata = std::fs::symlink_metadata(device).unwrap();
        assert!(
            !metadata.is_file(),
            "the platform's /dev/null unexpectedly looks regular"
        );
        let device_error = copy_regular_file(
            device,
            &root.join("device-copy"),
            &metadata,
            &mut CopyBudget::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(device_error.contains("non-regular file"), "{device_error}");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn nonzero_exit_and_launch_error_have_distinct_statuses() {
        let executable = std::env::current_exe().unwrap();
        let mut nonzero = tokio::process::Command::new(executable);
        nonzero
            .args(["--exact", "run::tests::process_tree_helper", "--nocapture"])
            .env("MEDHA_GATE_TEST_HELPER", "exit-7");
        let nonzero_status = supervise_agent_command(nonzero, Duration::from_secs(10)).await;
        assert_eq!(nonzero_status, RunStatus::ExitCode(7));

        let mut missing = tokio::process::Command::new(
            std::env::temp_dir().join(format!("missing-medha-{}", Ulid::new())),
        );
        missing.stdin(Stdio::null());
        let missing_status = supervise_agent_command(missing, Duration::from_secs(10)).await;
        assert!(
            matches!(missing_status, RunStatus::LaunchError(_)),
            "{missing_status:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signal_is_distinct_from_exit_code_and_timeout() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "kill -TERM $$"]);
        let status = supervise_agent_command(command, Duration::from_secs(10)).await;
        assert_eq!(status, RunStatus::Signaled);
    }

    #[tokio::test]
    async fn timeout_reaps_nested_descendant_and_releases_its_file_lock() {
        let root = temp_dir("gate-agent-timeout");
        let ready = root.join("ready");
        let release = root.join("release");
        let survivor = root.join("survived");
        let lock_file = root.join("held.lock");
        let executable = std::env::current_exe().unwrap();
        let mut command = tokio::process::Command::new(executable);
        command
            .args(["--exact", "run::tests::process_tree_helper", "--nocapture"])
            .env("MEDHA_GATE_TEST_HELPER", "leader")
            .env("MEDHA_GATE_READY", &ready)
            .env("MEDHA_GATE_RELEASE", &release)
            .env("MEDHA_GATE_SURVIVOR", &survivor)
            .env("MEDHA_GATE_LOCK", &lock_file);

        let status = supervise_agent_command(command, Duration::from_secs(4)).await;
        assert_eq!(status, RunStatus::TimedOut);
        assert!(
            ready.exists(),
            "nested helper did not acquire its file lock before the timeout"
        );

        // If the tree survived, this releases its barrier and makes it leave a
        // deterministic marker. The cross-process lock must already be reusable
        // when scoring resumes, proving teardown was settled rather than merely
        // signalled.
        assert!(
            fixture_lock_available(&lock_file),
            "nested helper still holds its file lock"
        );
        std::fs::write(&release, b"go").unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!survivor.exists(), "nested helper survived Gate timeout");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn dropping_run_future_still_reaps_owned_process_tree() {
        let root = temp_dir("gate-agent-cancel");
        let ready = root.join("ready");
        let release = root.join("release");
        let survivor = root.join("survived");
        let lock_file = root.join("held.lock");
        let executable = std::env::current_exe().unwrap();
        let mut command = tokio::process::Command::new(executable);
        command
            .args(["--exact", "run::tests::process_tree_helper", "--nocapture"])
            .env("MEDHA_GATE_TEST_HELPER", "leader")
            .env("MEDHA_GATE_READY", &ready)
            .env("MEDHA_GATE_RELEASE", &release)
            .env("MEDHA_GATE_SURVIVOR", &survivor)
            .env("MEDHA_GATE_LOCK", &lock_file);

        let mut execution = Box::pin(supervise_agent_command(command, Duration::from_secs(30)));
        let observe_ready = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !ready.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
        };
        tokio::pin!(observe_ready);
        tokio::select! {
            status = &mut execution => panic!("helper ended before cancellation: {status:?}"),
            observed = &mut observe_ready => {
                observed.expect("nested helper did not start");
            }
        }
        drop(execution);

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if fixture_lock_available(&lock_file) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled run's nested helper kept its file lock");
        std::fs::write(&release, b"go").unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !survivor.exists(),
            "nested helper survived cancellation of the Gate future"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn process_tree_helper() {
        let Ok(mode) = std::env::var("MEDHA_GATE_TEST_HELPER") else {
            return;
        };
        match mode.as_str() {
            "exit-0" => {}
            "exit-7" => std::process::exit(7),
            "panic" => panic!("injected evaluated-process panic"),
            "sleep" => std::thread::sleep(Duration::from_secs(30)),
            "sleep-marker" => {
                let started = PathBuf::from(std::env::var_os("MEDHA_GATE_STARTED").unwrap());
                std::fs::write(started, b"started").unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            "leader" => {
                let executable = std::env::current_exe().unwrap();
                let status = std::process::Command::new(executable)
                    .args(["--exact", "run::tests::process_tree_helper", "--nocapture"])
                    .env("MEDHA_GATE_TEST_HELPER", "grandchild")
                    .status()
                    .unwrap();
                std::process::exit(status.code().unwrap_or(1));
            }
            "grandchild" => {
                let ready = PathBuf::from(std::env::var_os("MEDHA_GATE_READY").unwrap());
                let release = PathBuf::from(std::env::var_os("MEDHA_GATE_RELEASE").unwrap());
                let survivor = PathBuf::from(std::env::var_os("MEDHA_GATE_SURVIVOR").unwrap());
                let lock_file = PathBuf::from(std::env::var_os("MEDHA_GATE_LOCK").unwrap());
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(lock_file)
                    .unwrap();
                let mut lock = fd_lock::RwLock::new(file);
                let _guard = lock.write().unwrap();
                std::fs::write(&ready, b"locked").unwrap();
                while !release.exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                std::fs::write(survivor, b"survived").unwrap();
            }
            other => panic!("unknown Gate helper mode {other}"),
        }
    }

    fn fixture_lock_available(path: &Path) -> bool {
        let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
            return false;
        };
        let mut lock = fd_lock::RwLock::new(file);
        lock.try_write().is_ok()
    }
}
