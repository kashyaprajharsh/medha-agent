//! Turning stochastic runs into a verdict (Vol 5 §5).
//!
//! A single run's pass/fail is noise — agents are stochastic. Running a scenario
//! `n` times gives a pass-rate; with `n > 1` we attach a Wilson score interval so
//! the report shows *confidence*, not a coin flip dressed as a fact.

use crate::checks::CheckOutcome;
#[cfg(test)]
use crate::checks::ValidationStatus;
use std::path::PathBuf;

/// The gate's decision for one scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Pass-rate met the threshold — safe to promote.
    Promote,
    /// Neither clearly good nor clearly broken — flaky or a run error.
    Hold,
    /// Nothing passed — a real regression.
    Reject,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Promote => "PROMOTE",
            Verdict::Hold => "HOLD",
            Verdict::Reject => "REJECT",
        }
    }
}

/// How the evaluated Medha process terminated.
///
/// Keep this separate from deterministic check outcomes: a workspace can look
/// correct after a crash, but only an ordinary zero exit is eligible to pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    /// The process exited normally with status zero.
    Succeeded,
    /// The process exited normally with a non-zero status.
    ExitCode(i32),
    /// The process was terminated by a signal or an equivalent platform event
    /// that does not expose a numeric exit code.
    Signaled,
    /// Gate's hard wall-clock deadline expired and the whole process tree was
    /// killed and reaped.
    TimedOut,
    /// The run was explicitly cancelled and its process tree was stopped.
    Cancelled,
    /// The configured Medha executable could not be launched or supervised.
    LaunchError(String),
    /// Gate failed before an agent process could produce an artifact.
    HarnessError(String),
}

impl RunStatus {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded)
    }

    pub fn completed(&self) -> bool {
        matches!(self, Self::Succeeded | Self::ExitCode(_))
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::ExitCode(_) => "exit_code",
            Self::Signaled => "signaled",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::LaunchError(_) => "launch_error",
            Self::HarnessError(_) => "harness_error",
        }
    }

    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Succeeded => Some(0),
            Self::ExitCode(code) => Some(*code),
            _ => None,
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::LaunchError(detail) | Self::HarnessError(detail) => Some(detail),
            _ => None,
        }
    }

    pub fn description(&self) -> String {
        match self {
            Self::Succeeded => "succeeded (exit 0)".into(),
            Self::ExitCode(code) => format!("failed with exit code {code}"),
            Self::Signaled => "terminated by signal".into(),
            Self::TimedOut => "timed out; process tree stopped".into(),
            Self::Cancelled => "cancelled; process tree stopped".into(),
            Self::LaunchError(error) => format!("launch error: {error}"),
            Self::HarnessError(error) => format!("harness error: {error}"),
        }
    }
}

/// One run of one scenario.
#[derive(Debug, Clone)]
pub struct SeedResult {
    pub checks: Vec<CheckOutcome>,
    pub status: RunStatus,
    pub wall_ms: u128,
    /// Present only when the operator explicitly requested artifact retention.
    pub artifact_path: Option<PathBuf>,
}

impl SeedResult {
    /// A seed passes only if the run exited zero AND every check passed.
    pub fn passed(&self) -> bool {
        self.status.is_success() && self.checks.iter().all(|c| c.passed)
    }
}

/// Aggregate of all seeds for one scenario.
#[derive(Debug, Clone)]
pub struct ScenarioResult {
    pub id: String,
    pub seeds: Vec<SeedResult>,
    pub pass_rate: f64,
    /// Wilson 95% interval on the pass-rate; `None` for a single seed.
    pub ci: Option<(f64, f64)>,
    pub verdict: Verdict,
}

/// Combine seeds into a verdict against `threshold` (from `[gate] pass_threshold`).
pub fn aggregate(id: String, seeds: Vec<SeedResult>, threshold: f64) -> ScenarioResult {
    let n = seeds.len().max(1);
    let passed = seeds.iter().filter(|s| s.passed()).count();
    let pass_rate = passed as f64 / n as f64;
    let ci = if seeds.len() > 1 {
        Some(wilson(passed, seeds.len()))
    } else {
        None
    };
    // promote when the bar is cleared; reject when nothing passed at all;
    // otherwise hold (flaky, or a run that errored/timed out).
    let verdict = if pass_rate + 1e-9 >= threshold {
        Verdict::Promote
    } else if passed == 0 {
        Verdict::Reject
    } else {
        Verdict::Hold
    };
    ScenarioResult {
        id,
        seeds,
        pass_rate,
        ci,
        verdict,
    }
}

/// Wilson score interval at 95% (z = 1.96) — well-behaved for small n and near
/// 0/1, unlike the naive normal interval.
fn wilson(passes: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 0.0);
    }
    let z = 1.96_f64;
    let n = n as f64;
    let p = passes as f64 / n;
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let margin = z * ((p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt()) / denom;
    ((center - margin).max(0.0), (center + margin).min(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(pass: bool) -> SeedResult {
        SeedResult {
            checks: vec![CheckOutcome {
                label: "x".into(),
                passed: pass,
                detail: String::new(),
                normalized_target: None,
                baseline_matches: None,
                workspace_matches: None,
                validation: ValidationStatus::NotApplicable,
            }],
            status: RunStatus::Succeeded,
            wall_ms: 0,
            artifact_path: None,
        }
    }

    #[test]
    fn single_pass_promotes_single_fail_rejects() {
        assert_eq!(
            aggregate("s".into(), vec![seed(true)], 1.0).verdict,
            Verdict::Promote
        );
        assert_eq!(
            aggregate("s".into(), vec![seed(false)], 1.0).verdict,
            Verdict::Reject
        );
    }

    #[test]
    fn flaky_multi_seed_holds() {
        let seeds = vec![seed(true), seed(true), seed(false)];
        let r = aggregate("s".into(), seeds, 1.0);
        assert_eq!(r.verdict, Verdict::Hold);
        assert!((r.pass_rate - 2.0 / 3.0).abs() < 1e-9);
        assert!(r.ci.is_some());
    }

    #[test]
    fn threshold_below_one_can_promote_a_flaky_scenario() {
        let seeds = vec![seed(true), seed(true), seed(true), seed(false)];
        assert_eq!(aggregate("s".into(), seeds, 0.7).verdict, Verdict::Promote);
    }

    #[test]
    fn every_abnormal_run_status_fails_even_when_checks_pass() {
        let abnormal = [
            RunStatus::ExitCode(7),
            RunStatus::Signaled,
            RunStatus::TimedOut,
            RunStatus::Cancelled,
            RunStatus::LaunchError("missing".into()),
            RunStatus::HarnessError("copy failed".into()),
        ];
        for status in abnormal {
            let description = status.description();
            let s = SeedResult {
                checks: vec![CheckOutcome {
                    label: "x".into(),
                    passed: true,
                    detail: String::new(),
                    normalized_target: None,
                    baseline_matches: None,
                    workspace_matches: None,
                    validation: ValidationStatus::NotApplicable,
                }],
                status,
                wall_ms: 0,
                artifact_path: None,
            };
            assert!(!s.passed(), "{description} counted as a passing run");
        }
    }

    #[test]
    fn wilson_brackets_the_point_estimate() {
        let (lo, hi) = wilson(8, 10);
        assert!(lo < 0.8 && 0.8 < hi);
        assert!(lo >= 0.0 && hi <= 1.0);
    }
}
