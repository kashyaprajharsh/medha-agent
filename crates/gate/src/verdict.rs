//! Turning stochastic runs into a verdict (Vol 5 §5).
//!
//! A single run's pass/fail is noise — agents are stochastic. Running a scenario
//! `n` times gives a pass-rate; with `n > 1` we attach a Wilson score interval so
//! the report shows *confidence*, not a coin flip dressed as a fact.

use crate::checks::CheckOutcome;

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

/// One run of one scenario.
#[derive(Debug, Clone)]
pub struct SeedResult {
    pub checks: Vec<CheckOutcome>,
    /// The agent process finished on its own (not timed out / failed to launch).
    pub completed: bool,
    pub wall_ms: u128,
}

impl SeedResult {
    /// A seed passes only if the run completed AND every check passed.
    pub fn passed(&self) -> bool {
        self.completed && self.checks.iter().all(|c| c.passed)
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
            }],
            completed: true,
            wall_ms: 0,
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
    fn a_timed_out_run_never_counts_as_passed() {
        let s = SeedResult {
            checks: vec![CheckOutcome {
                label: "x".into(),
                passed: true,
                detail: String::new(),
            }],
            completed: false,
            wall_ms: 0,
        };
        assert!(
            !s.passed(),
            "checks passing is moot if the run didn't finish"
        );
    }

    #[test]
    fn wilson_brackets_the_point_estimate() {
        let (lo, hi) = wilson(8, 10);
        assert!(lo < 0.8 && 0.8 < hi);
        assert!(lo >= 0.0 && hi <= 1.0);
    }
}
