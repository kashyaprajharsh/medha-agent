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
            }
            if !first.completed {
                out.push_str("    ⚠ run did not complete (timeout or launch failure)\n");
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
                    "completed": s.completed,
                    "wall_ms": s.wall_ms,
                    "checks": s.checks.iter().map(|c| json!({
                        "label": c.label, "passed": c.passed, "detail": c.detail
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
