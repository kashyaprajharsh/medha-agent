//! Scenario definition — a task + fixture + deterministic checks (Vol 5 §2).
//!
//! A scenario is a self-contained, content-addressed unit of agent evaluation:
//! a workspace fixture, the task to perform, a resource contract, and the checks
//! that decide pass/fail. Scenarios are authored as YAML and live in the repo
//! (part of the harness artifact), so they diff and travel like any other code.

use serde::Deserialize;
use std::collections::BTreeSet;
use std::fmt;
use std::path::Component;
use std::path::{Path, PathBuf};

use crate::GateError;

/// One day is a deliberately generous practical ceiling for a single
/// evaluation seed. It keeps Tokio/OS deadline arithmetic representable while
/// still allowing long cold builds; larger jobs should be split into scenarios.
pub const MAX_GATE_WALL_SECS: u64 = 24 * 60 * 60;

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
    /// Per-seed wall limit in seconds, validated in
    /// `1..=MAX_GATE_WALL_SECS`.
    pub max_wall_s: Option<u64>,
}

/// A single deterministic check. Authored as a one-key YAML map, e.g.
/// `- command: { run: "sh test.sh", expect_exit: 0 }` or
/// `- unchanged: { pattern: "tests/**", allow_zero_matches: false }`.
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
    Unchanged {
        pattern: String,
        allow_zero_matches: bool,
    },
    /// At least one file matching the glob differs from the pristine fixture.
    Changed {
        pattern: String,
        allow_zero_matches: bool,
    },
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    pattern: String,
    /// Explicit because accepting a vacuous file set is a consequential scoring
    /// choice, not a harmless default.
    allow_zero_matches: bool,
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
                f.write_str(
                    "a check as a single-key map, e.g. \
                     `unchanged: { pattern: \"tests/**\", allow_zero_matches: false }` or \
                     `command: { run: … }`",
                )
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
                    "unchanged" => {
                        let args: GlobArgs = map.next_value()?;
                        Check::Unchanged {
                            pattern: args.pattern,
                            allow_zero_matches: args.allow_zero_matches,
                        }
                    }
                    "changed" => {
                        let args: GlobArgs = map.next_value()?;
                        Check::Changed {
                            pattern: args.pattern,
                            allow_zero_matches: args.allow_zero_matches,
                        }
                    }
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
            Check::Unchanged { pattern, .. } => format!("unchanged: {pattern}"),
            Check::Changed { pattern, .. } => format!("changed: {pattern}"),
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
        scn.base_dir = base_dir.canonicalize().map_err(|e| {
            GateError::Scenario(format!(
                "resolving scenario directory {}: {e}",
                base_dir.display()
            ))
        })?;
        scn.validate(&file)?;
        Ok(scn)
    }

    /// Absolute path to the fixture directory.
    pub fn fixture_dir(&self) -> PathBuf {
        self.base_dir.join(&self.fixture)
    }

    /// Revalidate the fixture at the point of use. `Scenario` fields are public
    /// for reporting/integration, so `run_once` must not assume the value came
    /// from [`Scenario::load`] or remained unchanged after loading.
    pub(crate) fn validated_fixture_dir(&self) -> Result<PathBuf, String> {
        let configured = Path::new(&self.fixture);
        if configured.as_os_str().is_empty() {
            return Err("`fixture` must not be empty".into());
        }
        if configured.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(format!(
                "fixture must be a relative path without traversal: {}",
                configured.display()
            ));
        }

        let base = self.base_dir.canonicalize().map_err(|error| {
            format!(
                "could not resolve scenario directory {}: {error}",
                self.base_dir.display()
            )
        })?;
        let fixture = base.join(configured);
        let metadata = std::fs::symlink_metadata(&fixture).map_err(|error| {
            format!(
                "fixture directory not found: {}: {error}",
                fixture.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "fixture directory must not be a symlink: {}",
                fixture.display()
            ));
        }
        if !metadata.is_dir() {
            return Err(format!("fixture is not a directory: {}", fixture.display()));
        }
        let canonical_fixture = fixture
            .canonicalize()
            .map_err(|error| format!("could not resolve fixture {}: {error}", fixture.display()))?;
        if !canonical_fixture.starts_with(&base) {
            return Err(format!(
                "fixture resolves outside the scenario directory: {}",
                fixture.display()
            ));
        }
        Ok(canonical_fixture)
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
        if let Some(wall_s) = self.contract.max_wall_s {
            validate_gate_wall_seconds(wall_s).map_err(|error| {
                GateError::Scenario(format!("{}: `contract.max_wall_s` {error}", file.display()))
            })?;
        }

        let fixture = self
            .validated_fixture_dir()
            .map_err(|error| GateError::Scenario(format!("{}: {error}", file.display())))?;
        for (index, check) in self.checks.iter().enumerate() {
            match check {
                Check::Unchanged {
                    pattern,
                    allow_zero_matches,
                }
                | Check::Changed {
                    pattern,
                    allow_zero_matches,
                } => {
                    validate_glob_pattern(pattern).map_err(|error| {
                        GateError::Scenario(format!(
                            "{}: check {} pattern {:?}: {error}",
                            file.display(),
                            index + 1,
                            pattern
                        ))
                    })?;
                    let matches = matching_files(&fixture, pattern).map_err(|error| {
                        GateError::Scenario(format!(
                            "{}: check {} pattern {:?}: {error}",
                            file.display(),
                            index + 1,
                            pattern
                        ))
                    })?;
                    if matches.is_empty() && !allow_zero_matches {
                        return bad(format!(
                            "check {} pattern {:?} matched zero baseline fixture files; \
                             set `allow_zero_matches: true` only when that is intentional",
                            index + 1,
                            pattern
                        ));
                    }
                }
                Check::Exists(path) | Check::Absent(path) => {
                    validate_relative_target(path).map_err(|error| {
                        GateError::Scenario(format!(
                            "{}: check {} path {:?}: {error}",
                            file.display(),
                            index + 1,
                            path
                        ))
                    })?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_gate_wall_seconds(wall_s: u64) -> Result<(), String> {
    if wall_s == 0 {
        return Err("must be at least 1 second; requested 0".into());
    }
    if wall_s > MAX_GATE_WALL_SECS {
        return Err(format!(
            "must not exceed {MAX_GATE_WALL_SECS} seconds (24 hours); requested {wall_s}"
        ));
    }
    Ok(())
}

/// Validate a check target with the same portable rules on every host.
///
/// Rust's `Path` parser recognizes Windows prefixes only on Windows, so relying
/// on `Component::Prefix` would let `C:\...` through when a scenario is linted
/// on Unix and later executed on Windows. Gate scenarios use `/` separators and
/// normalized relative components everywhere.
pub(crate) fn validate_relative_target(target: &str) -> Result<String, String> {
    if target.is_empty() {
        return Err("target must not be empty".into());
    }
    if target.contains('\0') {
        return Err("target contains a NUL byte".into());
    }
    if target.contains('\\') {
        return Err(
            "target must use portable `/` separators and must not contain a Windows prefix".into(),
        );
    }
    let bytes = target.as_bytes();
    if target.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    {
        return Err("target must be relative and must not contain a platform prefix".into());
    }
    let components = target.split('/').collect::<Vec<_>>();
    if components
        .iter()
        .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        return Err(
            "target must be normalized without empty, current, or parent components".into(),
        );
    }
    Ok(components.join("/"))
}

pub(crate) fn validate_glob_pattern(pattern: &str) -> Result<glob::Pattern, String> {
    validate_relative_target(pattern)?;
    glob::Pattern::new(pattern).map_err(|error| format!("invalid glob: {error}"))
}

const MAX_CHECK_WALK_ENTRIES: usize = 100_000;
const MAX_CHECK_WALK_DEPTH: usize = 128;

/// Enumerate ordinary files beneath `root` without following symlinks.
///
/// This is shared by load-time baseline validation and post-run scoring. A
/// malicious agent-created link therefore cannot turn a relative glob into a
/// read of a host path after the scenario was validated.
pub(crate) fn matching_files(root: &Path, pattern: &str) -> Result<Vec<String>, String> {
    let pattern = validate_glob_pattern(pattern)?;
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|error| format!("could not inspect check root {}: {error}", root.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "check root is not an ordinary directory: {}",
            root.display()
        ));
    }
    let root = root
        .canonicalize()
        .map_err(|error| format!("could not resolve check root {}: {error}", root.display()))?;
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let mut matches = BTreeSet::new();
    let mut entries = 0usize;
    walk_matching_files(
        &root,
        &root,
        &pattern,
        options,
        &mut entries,
        0,
        &mut matches,
    )?;
    Ok(matches.into_iter().collect())
}

fn walk_matching_files(
    root: &Path,
    directory: &Path,
    pattern: &glob::Pattern,
    options: glob::MatchOptions,
    entries: &mut usize,
    depth: usize,
    matches: &mut BTreeSet<String>,
) -> Result<(), String> {
    if depth > MAX_CHECK_WALK_DEPTH {
        return Err(format!(
            "check tree exceeds maximum depth {MAX_CHECK_WALK_DEPTH}: {}",
            directory.display()
        ));
    }
    let read = std::fs::read_dir(directory).map_err(|error| {
        format!(
            "could not read check directory {}: {error}",
            directory.display()
        )
    })?;
    for entry in read {
        let entry = entry.map_err(|error| {
            format!(
                "could not enumerate check directory {}: {error}",
                directory.display()
            )
        })?;
        *entries = entries.saturating_add(1);
        if *entries > MAX_CHECK_WALK_ENTRIES {
            return Err(format!(
                "check tree exceeds {MAX_CHECK_WALK_ENTRIES} entries under {}",
                root.display()
            ));
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| format!("check entry escaped root: {}", path.display()))?;
        let portable = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            format!("could not inspect check entry {}: {error}", path.display())
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "check tree contains a symbolic link; containment is not validated: {portable}"
            ));
        }
        if metadata.is_dir() {
            walk_matching_files(root, &path, pattern, options, entries, depth + 1, matches)?;
        } else if metadata.is_file() {
            if pattern.matches_with(&portable, options) {
                matches.insert(portable);
            }
        } else {
            return Err(format!(
                "check tree contains a non-regular entry; containment is not validated: {portable}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_yaml_and_the_externally_tagged_checks() {
        let dir = std::env::temp_dir().join(format!("gate-scn-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(dir.join("fixture")).unwrap();
        std::fs::write(dir.join("fixture").join("test.sh"), "fixture test").unwrap();
        std::fs::write(
            dir.join("scenario.yaml"),
            r#"
id: demo
task: fix it
contract: { max_turns: 10 }
checks:
  - command: { run: "sh test.sh", expect_exit: 0 }
  - unchanged: { pattern: "test.sh", allow_zero_matches: false }
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
        assert!(matches!(
            &scn.checks[1],
            Check::Unchanged {
                pattern,
                allow_zero_matches: false
            } if pattern == "test.sh"
        ));
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

    #[test]
    fn gate_wall_limit_rejects_impractical_boundaries_without_overflow() {
        for wall_s in [0, MAX_GATE_WALL_SECS + 1, u64::MAX] {
            let dir = std::env::temp_dir().join(format!("gate-wall-invalid-{}", ulid::Ulid::new()));
            std::fs::create_dir_all(dir.join("fixture")).unwrap();
            std::fs::write(dir.join("fixture").join("present.txt"), "fixture").unwrap();
            std::fs::write(
                dir.join("scenario.yaml"),
                format!(
                    "id: wall\ntask: validate\ncontract:\n  max_wall_s: {wall_s}\n\
                     checks:\n  - exists: present.txt\n"
                ),
            )
            .unwrap();
            let error = Scenario::load(&dir).unwrap_err().to_string();
            assert!(error.contains("scenario.yaml"), "{wall_s}: {error}");
            assert!(error.contains("max_wall_s"), "{wall_s}: {error}");
            assert!(error.contains(&wall_s.to_string()), "{wall_s}: {error}");
            std::fs::remove_dir_all(dir).ok();
        }

        let dir = std::env::temp_dir().join(format!("gate-wall-max-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(dir.join("fixture")).unwrap();
        std::fs::write(dir.join("fixture").join("present.txt"), "fixture").unwrap();
        std::fs::write(
            dir.join("scenario.yaml"),
            format!(
                "id: wall\ntask: validate\ncontract:\n  max_wall_s: {MAX_GATE_WALL_SECS}\n\
                 checks:\n  - exists: present.txt\n"
            ),
        )
        .unwrap();
        Scenario::load(&dir).expect("the documented maximum must remain valid");
        std::fs::remove_dir_all(dir).ok();
    }

    fn write_check_scenario(dir: &Path, check_yaml: &str) {
        std::fs::create_dir_all(dir.join("fixture")).unwrap();
        std::fs::write(dir.join("fixture").join("present.txt"), "fixture").unwrap();
        std::fs::write(
            dir.join("scenario.yaml"),
            format!("id: check\ntask: validate\nchecks:\n  - {check_yaml}\n"),
        )
        .unwrap();
    }

    #[test]
    fn invalid_glob_reports_scenario_path_and_pattern() {
        let dir = std::env::temp_dir().join(format!("gate-invalid-glob-{}", ulid::Ulid::new()));
        write_check_scenario(
            &dir,
            r#"unchanged: { pattern: "[unterminated", allow_zero_matches: false }"#,
        );

        let error = Scenario::load(&dir).unwrap_err().to_string();
        assert!(error.contains("scenario.yaml"), "{error}");
        assert!(error.contains("[unterminated"), "{error}");
        assert!(error.contains("invalid glob"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn accidental_zero_match_requires_an_explicit_opt_in() {
        let forbidden = std::env::temp_dir().join(format!("gate-zero-glob-{}", ulid::Ulid::new()));
        write_check_scenario(
            &forbidden,
            r#"unchanged: { pattern: "missing/**", allow_zero_matches: false }"#,
        );
        let error = Scenario::load(&forbidden).unwrap_err().to_string();
        assert!(error.contains("scenario.yaml"), "{error}");
        assert!(error.contains("missing/**"), "{error}");
        assert!(error.contains("zero baseline"), "{error}");

        let allowed =
            std::env::temp_dir().join(format!("gate-allowed-zero-glob-{}", ulid::Ulid::new()));
        write_check_scenario(
            &allowed,
            r#"changed: { pattern: "generated/**", allow_zero_matches: true }"#,
        );
        Scenario::load(&allowed).expect("intentional zero baseline should validate");

        std::fs::remove_dir_all(forbidden).ok();
        std::fs::remove_dir_all(allowed).ok();
    }

    #[test]
    fn zero_match_policy_must_be_declared_in_yaml() {
        let dir =
            std::env::temp_dir().join(format!("gate-zero-policy-missing-{}", ulid::Ulid::new()));
        write_check_scenario(&dir, r#"unchanged: { pattern: "present.txt" }"#);
        let error = Scenario::load(&dir).unwrap_err().to_string();
        assert!(error.contains("allow_zero_matches"), "{error}");
        assert!(error.contains("scenario.yaml"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn literal_check_paths_reject_unix_windows_prefixes_and_traversal_portably() {
        for (index, target) in [
            "/etc/passwd",
            "../outside",
            "safe/../../outside",
            r"C:\Windows\system.ini",
            "C:/Windows/system.ini",
            r"\\server\share\secret",
            r"\rooted",
            "./present.txt",
            "safe//file",
        ]
        .into_iter()
        .enumerate()
        {
            let dir = std::env::temp_dir().join(format!(
                "gate-invalid-check-path-{index}-{}",
                ulid::Ulid::new()
            ));
            write_check_scenario(
                &dir,
                &format!(
                    "exists: {}",
                    serde_json::to_string(target).expect("serialize target")
                ),
            );
            let error = Scenario::load(&dir).unwrap_err().to_string();
            assert!(error.contains("scenario.yaml"), "{target:?}: {error}");
            assert!(
                error.contains(&format!("{target:?}")),
                "{target:?}: {error}"
            );
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn glob_patterns_cannot_traverse_or_use_platform_prefixes() {
        for (index, pattern) in ["../*.key", "/etc/*", "C:/Windows/*", r"..\*.key"]
            .into_iter()
            .enumerate()
        {
            let dir = std::env::temp_dir().join(format!(
                "gate-invalid-glob-path-{index}-{}",
                ulid::Ulid::new()
            ));
            write_check_scenario(
                &dir,
                &format!(
                    "unchanged: {{ pattern: {}, allow_zero_matches: true }}",
                    serde_json::to_string(pattern).expect("serialize pattern")
                ),
            );
            let error = Scenario::load(&dir).unwrap_err().to_string();
            assert!(error.contains("scenario.yaml"), "{pattern:?}: {error}");
            assert!(
                error.contains(&format!("{pattern:?}")),
                "{pattern:?}: {error}"
            );
            std::fs::remove_dir_all(dir).ok();
        }
    }

    fn write_scenario(dir: &Path, fixture: &Path) {
        std::fs::write(
            dir.join("scenario.yaml"),
            format!(
                "id: x\ntask: y\nfixture: {}\nchecks:\n  - exists: x\n",
                serde_json::to_string(&fixture.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();
    }

    #[test]
    fn fixture_parent_traversal_is_rejected_even_when_it_points_to_a_directory() {
        let root = std::env::temp_dir().join(format!("gate-scn-traversal-{}", ulid::Ulid::new()));
        let scenario = root.join("scenario");
        std::fs::create_dir_all(&scenario).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        write_scenario(&scenario, Path::new("../outside"));

        let error = Scenario::load(&scenario).unwrap_err().to_string();
        assert!(error.contains("without traversal"), "{error}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn absolute_fixture_path_is_rejected_even_when_it_points_to_a_directory() {
        let root = std::env::temp_dir().join(format!("gate-scn-absolute-{}", ulid::Ulid::new()));
        let scenario = root.join("scenario");
        let outside = root.join("outside");
        std::fs::create_dir_all(&scenario).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        write_scenario(&scenario, &outside);

        let error = Scenario::load(&scenario).unwrap_err().to_string();
        assert!(error.contains("relative path"), "{error}");
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn fixture_root_symlink_is_rejected_even_when_it_stays_beneath_scenario() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("gate-scn-root-link-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(root.join("real-fixture")).unwrap();
        symlink("real-fixture", root.join("fixture-link")).unwrap();
        write_scenario(&root, Path::new("fixture-link"));

        let error = Scenario::load(&root).unwrap_err().to_string();
        assert!(error.contains("must not be a symlink"), "{error}");
        std::fs::remove_dir_all(root).ok();
    }
}
