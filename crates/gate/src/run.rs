//! Hermetic scenario runner (Vol 5 §5 isolation).
//!
//! Each run gets a throwaway workspace (the fixture, copied in), a throwaway
//! `MEDHA_HOME` (so its event log is isolated and never touches the operator's
//! real `~/.medha`), and the scenario's contract as budget env. We spawn the
//! *real* `medha` binary — black-box, end-to-end, exactly as a user runs it —
//! then read back the run's event log for scoring. This deliberately does not
//! re-assemble the kernel in-process: the gate tests the shipped artifact.

use kernel::{Event, EventLog};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use store::SqliteLog;
use ulid::Ulid;

use crate::GateError;
use crate::checks::RunArtifact;
use crate::scenario::Scenario;

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
}

/// Run a scenario once, hermetically, and return its artifact for scoring.
pub async fn run_once(scn: &Scenario, cfg: &RunConfig) -> Result<RunArtifact, GateError> {
    let root = std::env::temp_dir().join(format!("medha-gate-{}", Ulid::new()));
    let workspace = root.join("workspace");
    let pristine = root.join("pristine");
    let home = root.join("home");
    copy_dir(&scn.fixture_dir(), &workspace)?;
    copy_dir(&scn.fixture_dir(), &pristine)?;
    std::fs::create_dir_all(&home).map_err(|e| GateError::Run(format!("mkdir home: {e}")))?;

    let mut cmd = tokio::process::Command::new(&cfg.binary);
    cmd.arg(&scn.task)
        .current_dir(&workspace)
        .env("MEDHA_HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &cfg.provider_env {
        cmd.env(k, v);
    }
    // Eval runs are unattended — there is no human to answer an approval prompt,
    // so the agent must run autonomously or every consequential edit is denied
    // and no task can complete. Safety comes from the throwaway workspace + the
    // OS sandbox + the deny-first scanner (which still blocks truly dangerous
    // commands), not from a human gate. `MEDHA_APPROVE=none` = no approval gate.
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

    // The child enforces its own MEDHA_MAX_WALL; this is a hard backstop with a
    // grace margin so a hung/ignored budget can never wedge the gate forever.
    let wall = scn.contract.max_wall_s.unwrap_or(cfg.default_wall_s);
    let start = Instant::now();
    let child = cmd
        .spawn()
        .map_err(|e| GateError::Run(format!("spawning {}: {e}", cfg.binary.display())))?;
    let completed = match tokio::time::timeout(
        Duration::from_secs(wall + 30),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(_)) => false, // process error
        Err(_) => false,     // timed out; kill_on_drop reaps it
    };
    let wall_ms = start.elapsed().as_millis();

    let events = load_events(&home).await;
    Ok(RunArtifact {
        workspace,
        pristine,
        events,
        completed,
        wall_ms,
    })
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

/// Recursively copy a directory tree. Small and dependency-free — fixtures are
/// tiny by design.
fn copy_dir(from: &Path, to: &Path) -> Result<(), GateError> {
    std::fs::create_dir_all(to)
        .map_err(|e| GateError::Run(format!("mkdir {}: {e}", to.display())))?;
    let entries = std::fs::read_dir(from)
        .map_err(|e| GateError::Run(format!("read {}: {e}", from.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| GateError::Run(e.to_string()))?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)
                .map_err(|e| GateError::Run(format!("copy {}: {e}", src.display())))?;
        }
    }
    Ok(())
}
