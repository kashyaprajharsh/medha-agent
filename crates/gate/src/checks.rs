//! Deterministic check evaluation over a finished run (Vol 5 §2).
//!
//! Every check here is a pure function of the run artifact — no model call, no
//! randomness. A given `RunArtifact` always yields the same verdict, which is
//! what makes the gate's *scoring* deterministic even though the agent *run*
//! that produced the artifact is not (that stochasticity is handled by seeds,
//! see `verdict.rs`).

use kernel::{Event, EventKind};
use std::path::{Path, PathBuf};

use crate::scenario::{Check, Scenario};

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
    /// True if the agent process finished on its own (not killed by the wall
    /// timeout / didn't fail to launch).
    pub completed: bool,
    /// Wall-clock time the agent run took.
    pub wall_ms: u128,
}

/// The result of one check against one run.
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub label: String,
    pub passed: bool,
    pub detail: String,
}

/// Evaluate every check in the scenario against a run. Order is preserved.
pub fn evaluate(scn: &Scenario, art: &RunArtifact) -> Vec<CheckOutcome> {
    scn.checks.iter().map(|c| eval_one(c, art)).collect()
}

fn eval_one(check: &Check, art: &RunArtifact) -> CheckOutcome {
    let label = check.label();
    let (passed, detail) = match check {
        Check::Command {
            run,
            expect_exit,
            contains,
        } => run_command(&art.workspace, run, *expect_exit, contains.as_deref()),
        Check::Unchanged(glob) => {
            let diffs = diff_glob(&art.pristine, &art.workspace, glob);
            (
                diffs.is_empty(),
                if diffs.is_empty() {
                    "no changes".into()
                } else {
                    format!("changed: {}", diffs.join(", "))
                },
            )
        }
        Check::Changed(glob) => {
            let diffs = diff_glob(&art.pristine, &art.workspace, glob);
            (
                !diffs.is_empty(),
                if diffs.is_empty() {
                    "no changes".into()
                } else {
                    format!("changed: {}", diffs.join(", "))
                },
            )
        }
        Check::Exists(p) => {
            let exists = art.workspace.join(p).exists();
            (
                exists,
                if exists {
                    "found".into()
                } else {
                    "not found".into()
                },
            )
        }
        Check::Absent(p) => {
            let exists = art.workspace.join(p).exists();
            (
                !exists,
                if exists {
                    "present".into()
                } else {
                    "absent".into()
                },
            )
        }
        Check::ToolUsed(tool) => {
            let n = count_tool_intents(&art.events, tool);
            (n > 0, format!("{n} call(s)"))
        }
        Check::ToolNotUsed(tool) => {
            let n = count_tool_intents(&art.events, tool);
            (n == 0, format!("{n} call(s)"))
        }
        Check::EventAbsent { kind, contains } => {
            let n = count_events(&art.events, kind, contains);
            (n == 0, format!("{n} match(es)"))
        }
        Check::EventPresent { kind, contains } => {
            let n = count_events(&art.events, kind, contains);
            (n > 0, format!("{n} match(es)"))
        }
    };
    CheckOutcome {
        label,
        passed,
        detail,
    }
}

/// Run `cmd` under `sh -c` in `workspace`; assert exit code (and optional stdout
/// substring). A command that fails to spawn counts as a failed check, never a
/// panic (P10 discipline carried into the gate).
fn run_command(
    workspace: &Path,
    cmd: &str,
    expect_exit: i32,
    contains: Option<&str>,
) -> (bool, String) {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(workspace)
        .output();
    match out {
        Ok(o) => {
            let code = o.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&o.stdout);
            let mut passed = code == expect_exit;
            let mut why = format!("exit {code}");
            if let Some(sub) = contains {
                let has = stdout.contains(sub) || String::from_utf8_lossy(&o.stderr).contains(sub);
                passed = passed && has;
                why = format!(
                    "{why}, stdout {}contains \"{sub}\"",
                    if has { "" } else { "does not " }
                );
            }
            (passed, why)
        }
        Err(e) => (false, format!("could not run: {e}")),
    }
}

/// Relative paths (under either root) matching `pattern` whose bytes differ
/// between the pristine fixture and the post-run workspace. A file present in
/// one tree but not the other counts as a difference.
fn diff_glob(pristine: &Path, workspace: &Path, pattern: &str) -> Vec<String> {
    let mut rels: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for root in [pristine, workspace] {
        for rel in glob_rel(root, pattern) {
            rels.insert(rel);
        }
    }
    rels.into_iter()
        .filter(|rel| {
            let a = std::fs::read(pristine.join(rel)).ok();
            let b = std::fs::read(workspace.join(rel)).ok();
            a != b
        })
        .collect()
}

/// Files matching `pattern` under `root`, as paths relative to `root`.
fn glob_rel(root: &Path, pattern: &str) -> Vec<String> {
    let full = format!("{}/{}", root.display(), pattern);
    let mut out = Vec::new();
    if let Ok(paths) = glob::glob(&full) {
        for p in paths.flatten() {
            if p.is_file() {
                if let Ok(rel) = p.strip_prefix(root) {
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
    out
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
    use serde_json::json;

    fn art(workspace: PathBuf, pristine: PathBuf, events: Vec<Event>) -> RunArtifact {
        RunArtifact {
            workspace,
            pristine,
            events,
            completed: true,
            wall_ms: 0,
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
            ts: 0.0,
        }
    }

    // Each check must PASS on a good state and FAIL on a bad one — a check that
    // can't fail is worthless (Vol 5 §2).

    #[test]
    fn command_check_reads_exit_code() {
        let ws = std::env::temp_dir();
        let (ok, _) = run_command(&ws, "true", 0, None);
        assert!(ok);
        let (bad, _) = run_command(&ws, "false", 0, None);
        assert!(!bad);
        // stdout substring gate
        let (has, _) = run_command(&ws, "echo hello", 0, Some("hello"));
        assert!(has);
        let (miss, _) = run_command(&ws, "echo hello", 0, Some("goodbye"));
        assert!(!miss);
    }

    #[test]
    fn tool_used_and_not_used_scan_intents() {
        let events = vec![intent_event("fs.edit"), intent_event("shell.exec")];
        let used = eval_one(
            &Check::ToolUsed("fs.edit".into()),
            &art(".".into(), ".".into(), events.clone()),
        );
        assert!(used.passed);
        let unused = eval_one(
            &Check::ToolNotUsed("web.fetch".into()),
            &art(".".into(), ".".into(), events.clone()),
        );
        assert!(unused.passed);
        // and it fails when the tool WAS used
        let should_fail = eval_one(
            &Check::ToolNotUsed("shell.exec".into()),
            &art(".".into(), ".".into(), events),
        );
        assert!(!should_fail.passed);
    }

    #[test]
    fn event_absent_catches_dangerous_pattern() {
        let clean = vec![intent_event("fs.read")];
        let ok = eval_one(
            &Check::EventAbsent {
                kind: "policy".into(),
                contains: "dangerous_pattern".into(),
            },
            &art(".".into(), ".".into(), clean),
        );
        assert!(ok.passed);

        let dirty = vec![policy_deny("blocked: dangerous_pattern rm -rf")];
        let caught = eval_one(
            &Check::EventAbsent {
                kind: "policy".into(),
                contains: "dangerous_pattern".into(),
            },
            &art(".".into(), ".".into(), dirty),
        );
        assert!(
            !caught.passed,
            "a dangerous_pattern policy deny must fail event_absent"
        );
    }

    #[test]
    fn unchanged_and_changed_diff_against_pristine() {
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
            eval_one(&Check::Unchanged("tests/**".into()), &a).passed,
            "tests/ untouched"
        );
        assert!(
            !eval_one(&Check::Unchanged("*.sh".into()), &a).passed,
            "src.sh changed"
        );
        assert!(
            eval_one(&Check::Changed("src.sh".into()), &a).passed,
            "src.sh did change"
        );
        assert!(
            !eval_one(&Check::Changed("tests/**".into()), &a).passed,
            "tests/ did not change"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn kind_prefix_matching() {
        assert!(kind_matches("policy.decision", "policy"));
        assert!(kind_matches("tool.observation", "tool"));
        assert!(kind_matches("policy.decision", "policy.decision"));
        assert!(!kind_matches("model.text", "policy"));
    }
}
