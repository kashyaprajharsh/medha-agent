//! Rendering gate results — a human table for the terminal and JSON for CI.

use serde_json::json;

use crate::verdict::{ScenarioResult, Verdict};

/// The overall process exit code: `0` all promoted · `1` any reject · `2` any
/// hold (and no reject). CI keys the merge gate off this.
pub fn exit_code(results: &[ScenarioResult]) -> i32 {
    if results.iter().any(|r| r.verdict == Verdict::Reject) {
        1
    } else if results.iter().any(|r| r.verdict != Verdict::Promote) {
        2
    } else {
        0
    }
}

/// Human-readable report for the terminal.
pub fn human(results: &[ScenarioResult], seeds: u32) -> String {
    let mut out = String::new();
    if seeds == 1 {
        out.push_str("note: 1 seed — a single run is noisy; raise --seeds (or [gate] seeds) for a confidence interval.\n\n");
    }
    for r in results {
        out.push_str(&format!("● {}  {}\n", r.id, r.verdict.as_str()));
        // Show the first seed's per-check breakdown (representative); multi-seed
        // adds the aggregate pass-rate line below.
        if let Some(first) = r.seeds.first() {
            for c in &first.checks {
                let mark = if c.passed { "✔" } else { "✗" };
                out.push_str(&format!("    {mark} {}  ({})\n", c.label, c.detail));
                if c.normalized_target.is_some()
                    || c.baseline_matches.is_some()
                    || c.workspace_matches.is_some()
                    || c.validation.as_str() != "not_applicable"
                {
                    out.push_str(&format!(
                        "        validation={} target={} baseline_matches={} workspace_matches={}\n",
                        c.validation.as_str(),
                        c.normalized_target.as_deref().unwrap_or("n/a"),
                        c.baseline_matches
                            .map(|count| count.to_string())
                            .unwrap_or_else(|| "n/a".into()),
                        c.workspace_matches
                            .map(|count| count.to_string())
                            .unwrap_or_else(|| "n/a".into()),
                    ));
                }
            }
            if !first.status.is_success() {
                out.push_str(&format!("    ⚠ agent run {}\n", first.status.description()));
            }
        }
        for (index, seed) in r.seeds.iter().enumerate() {
            if let Some(path) = &seed.artifact_path {
                out.push_str(&format!(
                    "    kept seed {} artifacts: {}\n",
                    index + 1,
                    path.display()
                ));
            }
        }
        if r.seeds.len() > 1 {
            let pct = (r.pass_rate * 100.0).round();
            let ci =
                r.ci.map(|(lo, hi)| format!("  (95% CI {:.0}–{:.0}%)", lo * 100.0, hi * 100.0))
                    .unwrap_or_default();
            out.push_str(&format!(
                "    pass-rate: {}/{} = {pct:.0}%{ci}\n",
                r.seeds.iter().filter(|s| s.passed()).count(),
                r.seeds.len()
            ));
        }
        let wall: u128 =
            r.seeds.iter().map(|s| s.wall_ms).sum::<u128>() / r.seeds.len().max(1) as u128;
        out.push_str(&format!("    avg wall: {:.1}s\n\n", wall as f64 / 1000.0));
    }
    let promoted = results
        .iter()
        .filter(|r| r.verdict == Verdict::Promote)
        .count();
    out.push_str(&format!("{promoted}/{} scenarios PROMOTE\n", results.len()));
    out
}

/// Machine-readable report for CI (`--json`).
pub fn json(results: &[ScenarioResult]) -> String {
    let scenarios: Vec<_> = results
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "verdict": r.verdict.as_str(),
                "pass_rate": r.pass_rate,
                "ci": r.ci.map(|(lo, hi)| json!([lo, hi])),
                "seeds": r.seeds.iter().map(|s| json!({
                    "passed": s.passed(),
                    "completed": s.status.completed(),
                    "run_status": s.status.kind(),
                    "exit_code": s.status.exit_code(),
                    "run_error": s.status.detail(),
                    "wall_ms": s.wall_ms,
                    "artifact_path": s.artifact_path,
                    "checks": s.checks.iter().map(|c| json!({
                        "label": c.label,
                        "passed": c.passed,
                        "detail": c.detail,
                        "normalized_target": c.normalized_target,
                        "baseline_matches": c.baseline_matches,
                        "workspace_matches": c.workspace_matches,
                        "validation": c.validation.as_str(),
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let doc = json!({
        "promoted": results.iter().filter(|r| r.verdict == Verdict::Promote).count(),
        "total": results.len(),
        "exit_code": exit_code(results),
        "scenarios": scenarios,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::{CheckOutcome, ValidationStatus};
    use crate::verdict::{RunStatus, SeedResult};

    fn result_with_filesystem_metadata() -> ScenarioResult {
        ScenarioResult {
            id: "metadata".into(),
            seeds: vec![SeedResult {
                checks: vec![
                    CheckOutcome {
                        label: "unchanged: tests/**".into(),
                        passed: false,
                        detail: "zero baseline matches are forbidden".into(),
                        normalized_target: Some("tests/**".into()),
                        baseline_matches: Some(0),
                        workspace_matches: Some(0),
                        validation: ValidationStatus::Validated,
                    },
                    CheckOutcome {
                        label: "exists: ../outside".into(),
                        passed: false,
                        detail: "validation failed: parent traversal".into(),
                        normalized_target: None,
                        baseline_matches: None,
                        workspace_matches: None,
                        validation: ValidationStatus::Invalid,
                    },
                ],
                status: RunStatus::Succeeded,
                wall_ms: 12,
                artifact_path: None,
            }],
            pass_rate: 0.0,
            ci: None,
            verdict: Verdict::Reject,
        }
    }

    #[test]
    fn human_and_json_reports_expose_match_and_containment_metadata() {
        let result = result_with_filesystem_metadata();
        let human = human(std::slice::from_ref(&result), 1);
        assert!(human.contains("validation=validated"), "{human}");
        assert!(human.contains("target=tests/**"), "{human}");
        assert!(human.contains("baseline_matches=0"), "{human}");
        assert!(human.contains("workspace_matches=0"), "{human}");
        assert!(human.contains("validation=invalid"), "{human}");
        assert!(human.contains("exists: ../outside"), "{human}");

        let document: serde_json::Value =
            serde_json::from_str(&json(std::slice::from_ref(&result))).unwrap();
        let checks = document["scenarios"][0]["seeds"][0]["checks"]
            .as_array()
            .unwrap();
        assert_eq!(checks[0]["validation"], "validated");
        assert_eq!(checks[0]["normalized_target"], "tests/**");
        assert_eq!(checks[0]["baseline_matches"], 0);
        assert_eq!(checks[0]["workspace_matches"], 0);
        assert_eq!(checks[1]["validation"], "invalid");
        assert!(checks[1]["normalized_target"].is_null());
    }
}
