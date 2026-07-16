//! Scenario definition — a task + fixture + deterministic checks (Vol 5 §2).
//!
//! A scenario is a self-contained, content-addressed unit of agent evaluation:
//! a workspace fixture, the task to perform, a resource contract, and the checks
//! that decide pass/fail. Scenarios are authored as YAML and live in the repo
//! (part of the harness artifact), so they diff and travel like any other code.

use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::GateError;

/// One evaluation scenario, loaded from `<dir>/scenario.yaml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    /// Stable id (used in reports and, later, as a regression key).
    pub id: String,
    /// The instruction handed to the agent, verbatim.
    pub task: String,
    /// Directory (relative to the scenario file) copied into a fresh workspace
    /// for each run. Defaults to `fixture/`.
    #[serde(default = "default_fixture")]
    pub fixture: String,
    /// Hard resource ceilings for the run (mapped to the kernel budget).
    #[serde(default)]
    pub contract: Contract,
    /// The checks, evaluated in order. All must pass for the run to pass.
    pub checks: Vec<Check>,
    /// Free-form tags for slicing (`coding`, `golden`, `adversarial`, …).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Directory the scenario file was loaded from — resolves `fixture` and
    /// relative check paths. Not part of the on-disk format.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

fn default_fixture() -> String {
    "fixture".to_string()
}

/// Resource ceilings. `None` fields leave the harness default in place.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Contract {
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_wall_s: Option<u64>,
}

/// A single deterministic check. Authored as a one-key YAML map, e.g.
/// `- command: { run: "sh test.sh", expect_exit: 0 }` or `- unchanged: "tests/**"`.
/// A hand-written [`Deserialize`] (below) reads that shape directly — serde_yaml
/// won't map a bare externally-tagged enum onto it, and a `!tag` form would be
/// worse to author.
///
/// Every kind is exact and free of any model call (Vol 5 §2: "deterministic
/// checks first, judges last"). LLM-as-judge rubrics are a later addition.
#[derive(Debug, Clone)]
pub enum Check {
    /// Run a shell command in the post-run workspace; assert its exit code
    /// (and, optionally, that stdout contains a substring).
    Command {
        run: String,
        expect_exit: i32,
        contains: Option<String>,
    },
    /// Every file matching the glob is byte-identical to the pristine fixture
    /// (the anti-cheating guard: "fixed the bug without editing the tests").
    Unchanged(String),
    /// At least one file matching the glob differs from the pristine fixture.
    Changed(String),
    /// A path exists in the post-run workspace.
    Exists(String),
    /// A path does NOT exist in the post-run workspace.
    Absent(String),
    /// The agent invoked this tool at least once (scans `model.tool_intent`).
    ToolUsed(String),
    /// The agent never invoked this tool (a trajectory guard, e.g. "no web.fetch
    /// on a purely local bug").
    ToolNotUsed(String),
    /// No event of `kind` carries `contains` in its payload, e.g. no
    /// `policy.decision` mentioning `dangerous_pattern`.
    EventAbsent { kind: String, contains: String },
    /// At least one event of `kind` carries `contains` in its payload.
    EventPresent { kind: String, contains: String },
}

// Payload shapes for the map-valued check kinds.
#[derive(Deserialize)]
struct CommandArgs {
    run: String,
    #[serde(default)]
    expect_exit: i32,
    #[serde(default)]
    contains: Option<String>,
}
#[derive(Deserialize)]
struct EventArgs {
    kind: String,
    contains: String,
}

impl<'de> Deserialize<'de> for Check {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CheckVisitor;
        impl<'de> serde::de::Visitor<'de> for CheckVisitor {
            type Value = Check;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a check as a single-key map, e.g. `unchanged: \"tests/**\"` or `command: { run: … }`")
            }
            fn visit_map<A>(self, mut map: A) -> Result<Check, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                use serde::de::Error;
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| A::Error::custom("empty check"))?;
                let check = match key.as_str() {
                    "command" => {
                        let a: CommandArgs = map.next_value()?;
                        Check::Command {
                            run: a.run,
                            expect_exit: a.expect_exit,
                            contains: a.contains,
                        }
                    }
                    "unchanged" => Check::Unchanged(map.next_value()?),
                    "changed" => Check::Changed(map.next_value()?),
                    "exists" => Check::Exists(map.next_value()?),
                    "absent" => Check::Absent(map.next_value()?),
                    "tool_used" => Check::ToolUsed(map.next_value()?),
                    "tool_not_used" => Check::ToolNotUsed(map.next_value()?),
                    "event_absent" => {
                        let a: EventArgs = map.next_value()?;
                        Check::EventAbsent {
                            kind: a.kind,
                            contains: a.contains,
                        }
                    }
                    "event_present" => {
                        let a: EventArgs = map.next_value()?;
                        Check::EventPresent {
                            kind: a.kind,
                            contains: a.contains,
                        }
                    }
                    other => return Err(A::Error::custom(format!("unknown check kind `{other}`"))),
                };
                if map.next_key::<String>()?.is_some() {
                    return Err(A::Error::custom("a check must have exactly one kind key"));
                }
                Ok(check)
            }
        }
        d.deserialize_map(CheckVisitor)
    }
}

impl Check {
    /// A short, stable human label for reports.
    pub fn label(&self) -> String {
        match self {
            Check::Command {
                run, expect_exit, ..
            } => format!("command `{run}` exits {expect_exit}"),
            Check::Unchanged(p) => format!("unchanged: {p}"),
            Check::Changed(p) => format!("changed: {p}"),
            Check::Exists(p) => format!("exists: {p}"),
            Check::Absent(p) => format!("absent: {p}"),
            Check::ToolUsed(t) => format!("tool used: {t}"),
            Check::ToolNotUsed(t) => format!("tool not used: {t}"),
            Check::EventAbsent { kind, contains } => format!("event absent: {kind} ~ {contains}"),
            Check::EventPresent { kind, contains } => format!("event present: {kind} ~ {contains}"),
        }
    }
}

impl Scenario {
    /// Load and validate a scenario from a directory (expects `scenario.yaml`)
    /// or a direct path to the YAML file.
    pub fn load(path: &Path) -> Result<Scenario, GateError> {
        let (file, base_dir) = if path.is_dir() {
            (path.join("scenario.yaml"), path.to_path_buf())
        } else {
            let base = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            (path.to_path_buf(), base)
        };
        let text = std::fs::read_to_string(&file)
            .map_err(|e| GateError::Scenario(format!("reading {}: {e}", file.display())))?;
        let mut scn: Scenario = serde_yaml::from_str(&text)
            .map_err(|e| GateError::Scenario(format!("parsing {}: {e}", file.display())))?;
        scn.base_dir = base_dir;
        scn.validate(&file)?;
        Ok(scn)
    }

    /// Absolute path to the fixture directory.
    pub fn fixture_dir(&self) -> PathBuf {
        self.base_dir.join(&self.fixture)
    }

    fn validate(&self, file: &Path) -> Result<(), GateError> {
        let bad = |m: String| Err(GateError::Scenario(format!("{}: {m}", file.display())));
        if self.id.trim().is_empty() {
            return bad("`id` must not be empty".into());
        }
        if self.task.trim().is_empty() {
            return bad("`task` must not be empty".into());
        }
        if self.checks.is_empty() {
            // A scenario with no checks can never fail — it would rubber-stamp
            // any run. Vol 5 §2: "a check that can't fail is deleted."
            return bad("a scenario must declare at least one check".into());
        }
        let fx = self.fixture_dir();
        if !fx.is_dir() {
            return bad(format!("fixture directory not found: {}", fx.display()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_yaml_and_the_externally_tagged_checks() {
        let dir = std::env::temp_dir().join(format!("gate-scn-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(dir.join("fixture")).unwrap();
        std::fs::write(
            dir.join("scenario.yaml"),
            r#"
id: demo
task: fix it
contract: { max_turns: 10 }
checks:
  - command: { run: "sh test.sh", expect_exit: 0 }
  - unchanged: "test.sh"
  - tool_not_used: "web.fetch"
  - event_absent: { kind: policy, contains: "dangerous_pattern" }
labels: [coding]
"#,
        )
        .unwrap();

        let scn = Scenario::load(&dir).expect("valid scenario loads");
        assert_eq!(scn.id, "demo");
        assert_eq!(scn.checks.len(), 4);
        assert!(matches!(
            scn.checks[0],
            Check::Command { expect_exit: 0, .. }
        ));
        assert!(matches!(&scn.checks[1], Check::Unchanged(p) if p == "test.sh"));
        assert!(matches!(&scn.checks[2], Check::ToolNotUsed(t) if t == "web.fetch"));
        assert!(matches!(&scn.checks[3], Check::EventAbsent { .. }));
        assert_eq!(scn.contract.max_turns, Some(10));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_scenario_with_no_checks_is_rejected() {
        // A check-less scenario would rubber-stamp any run (Vol 5 §2).
        let dir = std::env::temp_dir().join(format!("gate-scn-empty-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(dir.join("fixture")).unwrap();
        std::fs::write(dir.join("scenario.yaml"), "id: x\ntask: y\nchecks: []\n").unwrap();
        assert!(Scenario::load(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
