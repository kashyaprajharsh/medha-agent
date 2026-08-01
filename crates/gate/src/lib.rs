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

pub use run::{PreserveRuns, RunConfig};
pub use scenario::Scenario;
pub use verdict::{RunStatus, ScenarioResult, Verdict};

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("scenario error: {0}")]
    Scenario(String),
    #[error("run error: {0}")]
    Run(String),
    #[error("no scenarios found under {0}")]
    NoScenarios(String),
    #[error("invalid gate options: {0}")]
    Options(String),
}

/// Hard ceiling for stochastic repeats in one invocation. One Gate run can
/// spend the full scenario token/cost budget, so this is deliberately finite.
pub const MAX_SEEDS: u32 = 100;
/// Above this count an operator must acknowledge the paid work explicitly.
pub const COSTLY_SEEDS: u32 = 10;

/// Options for a gate invocation.
pub struct GateOptions {
    /// A scenario file, a scenario directory, or a directory of scenarios.
    pub path: PathBuf,
    /// Repeats per scenario (`[gate] seeds`, overridable by `--seeds`).
    pub seeds: u32,
    /// Promote threshold (`[gate] pass_threshold`).
    pub threshold: f64,
    /// Explicit acknowledgement for a seed count above [`COSTLY_SEEDS`].
    pub confirm_costly: bool,
    /// How to launch runs (binary + provider env + backstop wall).
    pub run: RunConfig,
}

pub fn validate_run_inputs(
    seeds: u32,
    threshold: f64,
    confirm_costly: bool,
) -> Result<(), GateError> {
    if seeds == 0 {
        return Err(GateError::Options("seeds must be at least 1".into()));
    }
    if seeds > MAX_SEEDS {
        return Err(GateError::Options(format!(
            "seeds must not exceed {MAX_SEEDS}; requested {seeds}"
        )));
    }
    if !threshold.is_finite() || threshold <= 0.0 || threshold > 1.0 {
        return Err(GateError::Options(format!(
            "pass threshold must be finite and in (0, 1]; requested {threshold}"
        )));
    }
    if seeds > COSTLY_SEEDS && !confirm_costly {
        return Err(GateError::Options(format!(
            "{seeds} seeds can trigger substantial model cost; pass --yes to confirm \
             counts above {COSTLY_SEEDS}"
        )));
    }
    Ok(())
}

/// Run the gate over one or more scenarios and return their aggregated results.
pub async fn run_gate(opts: GateOptions) -> Result<Vec<ScenarioResult>, GateError> {
    // Validate before discovery, vector allocation, or any provider-backed run.
    validate_run_inputs(opts.seeds, opts.threshold, opts.confirm_costly)?;
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
    // Do not size an allocation from a caller-controlled count. The validated
    // ceiling above bounds both memory and paid provider work.
    let mut seeds = Vec::new();
    let command_runner = checks::CommandRunner::from_sandbox(&opts.run.check_sandbox);
    for _ in 0..opts.seeds {
        let seed = match run::run_once(scn, &opts.run).await {
            Ok(mut art) => {
                let checks = checks::evaluate(scn, &art, &command_runner).await;
                let failed = !art.status.is_success() || checks.iter().any(|check| !check.passed);
                if opts.run.preserve.should_preserve(failed) {
                    art.preserve();
                }
                let artifact_path = art.preserved_path().map(Path::to_path_buf);
                verdict::SeedResult {
                    checks,
                    status: art.status,
                    wall_ms: art.wall_ms,
                    artifact_path,
                }
            }
            Err(e) => verdict::SeedResult {
                checks: vec![checks::CheckOutcome {
                    label: "run".into(),
                    passed: false,
                    detail: e.to_string(),
                    normalized_target: None,
                    baseline_matches: None,
                    workspace_matches: None,
                    validation: checks::ValidationStatus::NotApplicable,
                }],
                status: verdict::RunStatus::HarnessError(e.to_string()),
                wall_ms: 0,
                artifact_path: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_thresholds_and_seed_counts_fail_validation() {
        for threshold in [
            0.0,
            -0.1,
            -f64::INFINITY,
            f64::INFINITY,
            f64::NAN,
            1.000_001,
        ] {
            assert!(
                validate_run_inputs(1, threshold, false).is_err(),
                "threshold {threshold:?} was accepted"
            );
        }
        for seeds in [0, MAX_SEEDS + 1, u32::MAX] {
            assert!(
                validate_run_inputs(seeds, 1.0, true).is_err(),
                "seed count {seeds} was accepted"
            );
        }
        assert!(validate_run_inputs(1, f64::MIN_POSITIVE, false).is_ok());
        assert!(validate_run_inputs(MAX_SEEDS, 1.0, true).is_ok());
    }

    #[test]
    fn costly_seed_counts_require_explicit_confirmation() {
        assert!(validate_run_inputs(COSTLY_SEEDS, 1.0, false).is_ok());
        let error = validate_run_inputs(COSTLY_SEEDS + 1, 1.0, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--yes"), "{error}");
        assert!(validate_run_inputs(COSTLY_SEEDS + 1, 1.0, true).is_ok());
    }

    #[tokio::test]
    async fn invalid_inputs_fail_before_scenario_discovery_or_process_launch() {
        let missing_path =
            std::env::temp_dir().join(format!("gate-must-not-discover-{}", ulid::Ulid::new()));
        let options = GateOptions {
            path: missing_path,
            seeds: u32::MAX,
            threshold: 1.0,
            confirm_costly: true,
            run: RunConfig {
                binary: PathBuf::from("must-not-launch"),
                provider_env: Vec::new(),
                default_wall_s: 1,
                check_sandbox: sandbox::SandboxConfig::default(),
                preserve: PreserveRuns::Never,
            },
        };
        let error = run_gate(options).await.unwrap_err();
        assert!(
            matches!(error, GateError::Options(_)),
            "validation did not run first: {error}"
        );
    }

    #[tokio::test]
    async fn keep_failures_preserves_a_deterministic_check_failure() {
        let root = tempfile::tempdir().unwrap();
        let scenario_dir = root.path().join("scenario");
        std::fs::create_dir_all(scenario_dir.join("fixture")).unwrap();
        std::fs::write(scenario_dir.join("fixture").join("input.txt"), "fixture").unwrap();
        std::fs::write(
            scenario_dir.join("scenario.yaml"),
            "id: keep-failure\n\
             task: run::tests::process_tree_helper\n\
             checks:\n  - tool_used: never.called\n",
        )
        .unwrap();
        let scenario = Scenario::load(&scenario_dir).unwrap();
        let options = GateOptions {
            path: scenario_dir,
            seeds: 1,
            threshold: 1.0,
            confirm_costly: false,
            run: RunConfig {
                binary: std::env::current_exe().unwrap(),
                provider_env: vec![("MEDHA_GATE_TEST_HELPER".into(), "exit-0".into())],
                default_wall_s: 10,
                check_sandbox: sandbox::SandboxConfig::default(),
                preserve: PreserveRuns::Failures,
            },
        };

        let result = run_scenario(&scenario, &options).await;
        assert_eq!(result.verdict, Verdict::Reject);
        let kept = result.seeds[0]
            .artifact_path
            .as_ref()
            .expect("failed check did not preserve artifacts");
        assert!(kept.exists());
        std::fs::remove_dir_all(kept).unwrap();
    }
}
