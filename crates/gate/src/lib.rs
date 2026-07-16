//! The Eval Gate — "CI for cognition" (spec §4.11–4.12, Vol 5).
//!
//! `medha gate <scenario>` runs the real agent against a fixture task in
//! isolation, then scores the run with **deterministic checks over the event
//! log and the filesystem** — no LLM-as-judge (that is a later, calibrated
//! addition; Vol 5 §4). It emits a **promote / hold / reject** verdict with
//! evidence, and its exit code gates CI.
//!
//! Layering, so scoring stays testable without a model:
//! - [`scenario`] — the YAML schema + validation.
//! - [`run`] — spawns the binary hermetically → [`checks::RunArtifact`].
//! - [`checks`] — *pure* evaluation of an artifact → outcomes (unit-tested).
//! - [`verdict`] — seed aggregation + Wilson interval.
//! - [`report`] — human table + JSON.

pub mod checks;
pub mod report;
pub mod run;
pub mod scenario;
pub mod verdict;

pub use run::RunConfig;
pub use scenario::Scenario;
pub use verdict::{ScenarioResult, Verdict};

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("scenario error: {0}")]
    Scenario(String),
    #[error("run error: {0}")]
    Run(String),
    #[error("no scenarios found under {0}")]
    NoScenarios(String),
}

/// Options for a gate invocation.
pub struct GateOptions {
    /// A scenario file, a scenario directory, or a directory of scenarios.
    pub path: PathBuf,
    /// Repeats per scenario (`[gate] seeds`, overridable by `--seeds`).
    pub seeds: u32,
    /// Promote threshold (`[gate] pass_threshold`).
    pub threshold: f64,
    /// How to launch runs (binary + provider env + backstop wall).
    pub run: RunConfig,
}

/// Run the gate over one or more scenarios and return their aggregated results.
pub async fn run_gate(opts: GateOptions) -> Result<Vec<ScenarioResult>, GateError> {
    let paths = discover(&opts.path)?;
    let mut results = Vec::with_capacity(paths.len());
    for p in paths {
        let scn = Scenario::load(&p)?;
        results.push(run_scenario(&scn, &opts).await);
    }
    Ok(results)
}

/// Run one scenario `seeds` times and aggregate. A run that errors becomes a
/// non-completing seed rather than aborting the whole gate (P10).
async fn run_scenario(scn: &Scenario, opts: &GateOptions) -> ScenarioResult {
    let mut seeds = Vec::with_capacity(opts.seeds.max(1) as usize);
    for _ in 0..opts.seeds.max(1) {
        let seed = match run::run_once(scn, &opts.run).await {
            Ok(art) => verdict::SeedResult {
                checks: checks::evaluate(scn, &art),
                completed: art.completed,
                wall_ms: art.wall_ms,
            },
            Err(e) => verdict::SeedResult {
                checks: vec![checks::CheckOutcome {
                    label: "run".into(),
                    passed: false,
                    detail: e.to_string(),
                }],
                completed: false,
                wall_ms: 0,
            },
        };
        seeds.push(seed);
    }
    verdict::aggregate(scn.id.clone(), seeds, opts.threshold)
}

/// Resolve a path into a list of scenario paths:
/// - a `.yaml` file → itself;
/// - a directory containing `scenario.yaml` → that one scenario;
/// - otherwise a directory → each immediate subdirectory that has a
///   `scenario.yaml` (a scenario suite).
pub fn discover(path: &Path) -> Result<Vec<PathBuf>, GateError> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if path.join("scenario.yaml").is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if path.is_dir() {
        let mut found: Vec<PathBuf> = std::fs::read_dir(path)
            .map_err(|e| GateError::Run(format!("read {}: {e}", path.display())))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.join("scenario.yaml").is_file())
            .collect();
        found.sort();
        if found.is_empty() {
            return Err(GateError::NoScenarios(path.display().to_string()));
        }
        return Ok(found);
    }
    Err(GateError::NoScenarios(path.display().to_string()))
}
