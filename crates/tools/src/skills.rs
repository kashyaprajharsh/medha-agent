//! Skills — Phase A: the *consumption* side of spec §4.11 plus user/agent
//! authoring with a human approval gate. A skill is a folder with a `SKILL.md`
//! (agentskills.io-compatible: TOML frontmatter + a markdown procedure body).
//! No auto-distillation, evals, canary, or win-rates yet (Phase D) — here skills
//! are config the harness discovers and the model loads on demand.
//!
//! Progressive disclosure matches the existing `read_artifact` pattern: the
//! system prompt carries one compact line per skill (the manifest, K2), and the
//! model pulls a full procedure with `skill.load` only when it decides one is
//! relevant. `skill.save` lets the user (or the agent, on offer) persist a new
//! skill — always behind the approval card, since it rides the normal
//! blast-radius/policy path like any other write.

use crate::{Tool, ToolError};
use async_trait::async_trait;
use kernel::{BlastRadius, ToolCategory};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where a skill was found. Project (workspace-committed) shadows user (personal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    Project,
    User,
}

impl SkillScope {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillScope::Project => "project",
            SkillScope::User => "user",
        }
    }
}

/// The TOML frontmatter of a `SKILL.md`. `name` + `description` are the only
/// required fields (agentskills.io minimum); the rest default.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default)]
    triggers: Vec<String>,
    #[serde(default)]
    domains: Vec<String>,
    #[serde(default)]
    required_tools: Vec<String>,
    #[serde(default = "default_version")]
    version: u32,
}

fn default_version() -> u32 {
    1
}

/// A parsed `SKILL.md`: its frontmatter and the markdown body after it.
type ParsedMd = (Frontmatter, String);

/// A parsed skill: its frontmatter, the procedure body, and where it lives.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub domains: Vec<String>,
    pub required_tools: Vec<String>,
    pub version: u32,
    pub body: String,
    pub scope: SkillScope,
    pub path: PathBuf,
}

/// A skill as surfaced to `/skills` and the manifest: the skill plus the derived
/// facts (is it shadowed by a higher scope? are its required tools present?).
#[derive(Debug, Clone)]
pub struct SkillListing {
    pub skill: Skill,
    pub shadowed: bool,
    pub missing_tools: Vec<String>,
}

impl SkillListing {
    pub fn available(&self) -> bool {
        self.missing_tools.is_empty()
    }
}

/// Result of a discovery scan: every listing (including shadowed ones, for
/// `/skills`) plus per-file parse errors so one broken skill never breaks a
/// session.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub listings: Vec<SkillListing>,
    pub errors: Vec<(PathBuf, String)>,
}

impl Discovery {
    /// The skills that actually apply this session: not shadowed, sorted by name.
    pub fn effective(&self) -> impl Iterator<Item = &SkillListing> {
        self.listings.iter().filter(|l| !l.shadowed)
    }
}

/// Discovers, parses, and validates skills across the project and user scopes.
/// Pure filesystem access (reads the harness's own `.medha/skills` config dirs
/// directly, not through the sandbox) so scanning never prompts for permission
/// and the store is unit-testable with plain temp dirs.
pub struct SkillStore {
    project_dir: PathBuf,
    /// `None` when the platform has no home directory (user scope unavailable).
    user_dir: Option<PathBuf>,
}

impl SkillStore {
    pub fn new(project_dir: PathBuf, user_dir: Option<PathBuf>) -> Self {
        Self { project_dir, user_dir }
    }

    /// Scan both scopes. Project is scanned first; a same-named user skill is
    /// marked `shadowed`. `known_tools` (the registry's registered tool names)
    /// drives the `missing_tools` availability check.
    pub fn discover(&self, known_tools: &HashSet<String>) -> Discovery {
        let mut out = Discovery::default();
        let mut seen: HashSet<String> = HashSet::new();

        let dirs: [(Option<&PathBuf>, SkillScope); 2] =
            [(Some(&self.project_dir), SkillScope::Project), (self.user_dir.as_ref(), SkillScope::User)];
        for (dir, scope) in dirs {
            let Some(dir) = dir else { continue };
            for (path, parsed) in scan_dir(dir) {
                match parsed {
                    Ok(fm_body) => {
                        let skill = build_skill(fm_body, scope, path);
                        let shadowed = !seen.insert(skill.name.clone());
                        let missing_tools = skill
                            .required_tools
                            .iter()
                            .filter(|t| !known_tools.contains(*t))
                            .cloned()
                            .collect();
                        out.listings.push(SkillListing { skill, shadowed, missing_tools });
                    }
                    Err(reason) => out.errors.push((path, reason)),
                }
            }
        }
        out
    }

    /// Load one skill's full body by name (project shadows user). Re-reads from
    /// disk so a mid-session edit is picked up on the next call. Returns a
    /// structured error the model can act on: not found (lists what exists) or
    /// unavailable (names the missing tools).
    pub fn load(&self, name: &str, known_tools: &HashSet<String>) -> Result<Value, String> {
        let disc = self.discover(known_tools);
        let Some(listing) = disc.effective().find(|l| l.skill.name == name) else {
            let available: Vec<&str> = disc.effective().map(|l| l.skill.name.as_str()).collect();
            return Err(if available.is_empty() {
                format!("no skill named '{name}'; no skills are installed")
            } else {
                format!("no skill named '{name}'; available skills: {}", available.join(", "))
            });
        };
        if !listing.available() {
            return Err(format!(
                "skill '{name}' needs tools not available in this session: {}. \
                 It is listed but cannot be followed here.",
                listing.missing_tools.join(", ")
            ));
        }
        let s = &listing.skill;
        Ok(json!({
            "name": s.name,
            "description": s.description,
            "scope": s.scope.as_str(),
            "required_tools": s.required_tools,
            "procedure": s.body,
        }))
    }

    /// Build the compact K2 manifest section injected into the system prompt.
    /// Empty string when no skills exist (regression guard: zero skills → no
    /// section, no behaviour change). When more than `TRIM_ABOVE` skills exist,
    /// trims to those whose triggers/domains match `prompt` plus a count of the
    /// rest — keeping the sheath cheap.
    pub fn manifest(&self, known_tools: &HashSet<String>, prompt: Option<&str>) -> String {
        const TRIM_ABOVE: usize = 30;
        let disc = self.discover(known_tools);
        let all: Vec<&SkillListing> = disc.effective().collect();
        if all.is_empty() {
            return String::new();
        }

        let (shown, hidden): (Vec<&SkillListing>, usize) = if all.len() > TRIM_ABOVE {
            let p = prompt.unwrap_or("").to_lowercase();
            let matched: Vec<&SkillListing> = all
                .iter()
                .copied()
                .filter(|l| {
                    l.skill
                        .triggers
                        .iter()
                        .chain(l.skill.domains.iter())
                        .any(|kw| !kw.is_empty() && p.contains(&kw.to_lowercase()))
                })
                .collect();
            let hidden = all.len().saturating_sub(matched.len());
            (matched, hidden)
        } else {
            (all, 0)
        };

        let mut lines = String::from(
            "## Skills available — CHECK THIS BEFORE STARTING A TASK. If any description below \
             matches the request, skill.load it and follow it before doing the task your own way.\n",
        );
        for l in &shown {
            let s = &l.skill;
            let mut line = format!("- {} — {}", s.name, s.description);
            if !s.triggers.is_empty() {
                line.push_str(&format!("  [triggers: {}]", s.triggers.join(", ")));
            }
            if !l.available() {
                line.push_str(&format!("  (unavailable: needs {})", l.missing_tools.join(", ")));
            }
            lines.push_str(&line);
            lines.push('\n');
        }
        if hidden > 0 {
            lines.push_str(&format!("- … and {hidden} more — ask to list skills\n"));
        }
        lines
    }

    /// Validate and write a new skill. Creates only (a duplicate name in-scope is
    /// rejected — editing is plain `fs.edit` on the existing file). Writes
    /// directly: the approval card (skill.save is on the policy approve list)
    /// already showed the full content, so a second permission prompt would be
    /// redundant. Returns the written path.
    pub fn save(&self, spec: &SaveSpec, known_tools: &HashSet<String>) -> Result<PathBuf, String> {
        validate_name(&spec.name)?;
        let desc = spec.description.trim();
        if desc.is_empty() {
            return Err("description must not be empty".into());
        }
        if desc.chars().count() > 120 {
            return Err(format!("description is {} chars; keep it ≤120 (one line)", desc.chars().count()));
        }
        if spec.procedure.trim().is_empty() {
            return Err("procedure body must not be empty".into());
        }
        let unknown: Vec<&String> = spec.required_tools.iter().filter(|t| !known_tools.contains(*t)).collect();
        if !unknown.is_empty() {
            return Err(format!(
                "required_tools not registered in this session: {}",
                unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        let dir = match spec.scope {
            SkillScope::Project => &self.project_dir,
            SkillScope::User => self
                .user_dir
                .as_ref()
                .ok_or("no user home directory available; save to project scope instead")?,
        };
        let target = dir.join(&spec.name).join("SKILL.md");
        if target.exists() {
            return Err(format!(
                "a {} skill named '{}' already exists at {}; edit it with fs.edit instead",
                spec.scope.as_str(),
                spec.name,
                target.display()
            ));
        }
        let content = spec.render();
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&target, content).map_err(|e| e.to_string())?;
        Ok(target)
    }
}

/// The fields `skill.save` collects and renders into a `SKILL.md`.
#[derive(Debug, Clone)]
pub struct SaveSpec {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub domains: Vec<String>,
    pub required_tools: Vec<String>,
    pub procedure: String,
    pub scope: SkillScope,
}

impl SaveSpec {
    /// Render the full `SKILL.md` (TOML frontmatter between `---` fences + body).
    /// This is exactly what the approval card previews.
    pub fn render(&self) -> String {
        let fm = Frontmatter {
            name: self.name.clone(),
            description: self.description.trim().to_string(),
            triggers: self.triggers.clone(),
            domains: self.domains.clone(),
            required_tools: self.required_tools.clone(),
            version: 1,
        };
        // toml::to_string on a plain struct is stable and correctly escapes
        // strings — safer than hand-formatting the frontmatter.
        let frontmatter = toml::to_string(&fm).unwrap_or_default();
        format!("---\n{frontmatter}---\n\n{}\n", self.procedure.trim())
    }
}

/// kebab-case: lowercase alphanumerics separated by single hyphens.
fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if ok {
        Ok(())
    } else {
        Err(format!("name '{name}' must be kebab-case (lowercase, digits, single hyphens)"))
    }
}

/// Scan a `<dir>/*/SKILL.md` layout, returning each file's parse result. A
/// missing directory yields nothing (the common case — most workspaces have no
/// skills). Sorted by directory name for deterministic ordering.
fn scan_dir(dir: &Path) -> Vec<(PathBuf, Result<ParsedMd, String>)> {
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort();
    let mut out = Vec::new();
    for sub in entries {
        let md = sub.join("SKILL.md");
        if md.is_file() {
            let parsed = std::fs::read_to_string(&md)
                .map_err(|e| e.to_string())
                .and_then(|text| parse_skill_md(&text));
            out.push((md, parsed));
        }
    }
    out
}

/// Split a `SKILL.md` into its TOML frontmatter and markdown body. The file must
/// open with a `---` fence, contain a closing `---` fence, and the frontmatter
/// must parse as TOML with a non-empty `name` and `description`.
fn parse_skill_md(text: &str) -> Result<ParsedMd, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text); // tolerate a BOM
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or("missing opening '---' frontmatter fence")?;
    // Find the closing fence at the start of a line.
    let (fm_src, body) = split_at_closing_fence(rest).ok_or("missing closing '---' frontmatter fence")?;
    let fm: Frontmatter = toml::from_str(fm_src).map_err(|e| format!("invalid frontmatter TOML: {e}"))?;
    if fm.name.trim().is_empty() {
        return Err("frontmatter 'name' is required".into());
    }
    if fm.description.trim().is_empty() {
        return Err("frontmatter 'description' is required".into());
    }
    Ok((fm, body.trim_start_matches(['\n', '\r']).to_string()))
}

/// Return (frontmatter_src, body) by finding a line that is exactly `---`.
fn split_at_closing_fence(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

fn build_skill(fm_body: ParsedMd, scope: SkillScope, path: PathBuf) -> Skill {
    let (fm, body) = fm_body;
    Skill {
        name: fm.name,
        description: fm.description,
        triggers: fm.triggers,
        domains: fm.domains,
        required_tools: fm.required_tools,
        version: fm.version,
        body,
        scope,
        path,
    }
}

// ---- Tools ---------------------------------------------------------------

/// `skill.load` — read radius. The model pulls a full procedure by name.
pub struct SkillLoad {
    pub store: Arc<SkillStore>,
    pub known_tools: Arc<HashSet<String>>,
}

#[async_trait]
impl Tool for SkillLoad {
    fn name(&self) -> &str {
        "skill.load"
    }
    fn description(&self) -> &str {
        "Load the full procedure of an installed skill by name (see the 'Skills \
         available' list in your system prompt). Returns the skill's steps and \
         known failure modes. Call this before following a skill."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The skill's name (kebab-case)" }
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Args("expected string 'name'".into()))?;
        self.store.load(name, &self.known_tools).map_err(ToolError::Failed)
    }
}

/// `skill.save` — reversible-local write, always gated by the approval card
/// (it is on the policy approve list). The card previews the full SKILL.md.
pub struct SkillSave {
    pub store: Arc<SkillStore>,
    pub known_tools: Arc<HashSet<String>>,
}

impl SkillSave {
    /// Parse tool args into a validated-shape SaveSpec (field presence only;
    /// SkillStore::save does the semantic validation).
    fn spec_from(args: &Value) -> Result<SaveSpec, ToolError> {
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        let list = |k: &str| {
            args.get(k)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        let name = s("name").ok_or_else(|| ToolError::Args("expected string 'name'".into()))?;
        let description =
            s("description").ok_or_else(|| ToolError::Args("expected string 'description'".into()))?;
        let procedure =
            s("procedure").ok_or_else(|| ToolError::Args("expected string 'procedure'".into()))?;
        let scope = match args.get("scope").and_then(Value::as_str).unwrap_or("user") {
            "project" => SkillScope::Project,
            "user" => SkillScope::User,
            other => return Err(ToolError::Args(format!("scope must be 'user' or 'project', got '{other}'"))),
        };
        Ok(SaveSpec {
            name,
            description,
            triggers: list("triggers"),
            domains: list("domains"),
            required_tools: list("required_tools"),
            procedure,
            scope,
        })
    }
}

#[async_trait]
impl Tool for SkillSave {
    fn name(&self) -> &str {
        "skill.save"
    }
    fn description(&self) -> &str {
        "Save a reusable procedure as a skill so it is available in future \
         sessions. Use when the user asks to remember a procedure, or OFFER \
         (ask first) when the user has repeated an instruction or when web \
         research produced a reusable procedure. Writes a SKILL.md; always \
         requires the user's approval."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::ReversibleLocal
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Write
    }
    fn icon(&self) -> &'static str {
        "★"
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "kebab-case skill name" },
                "description": { "type": "string", "description": "one line, ≤120 chars — shown in the skills list" },
                "procedure": { "type": "string", "description": "the skill body: steps, decision points, known failure modes (markdown)" },
                "triggers": { "type": "array", "items": { "type": "string" }, "description": "match hints (keywords)" },
                "domains": { "type": "array", "items": { "type": "string" } },
                "required_tools": { "type": "array", "items": { "type": "string" }, "description": "tool names the procedure needs; validated against the registry" },
                "scope": { "type": "string", "enum": ["user", "project"], "description": "user (personal, default) or project (committed with the repo)" }
            },
            "required": ["name", "description", "procedure"]
        })
    }
    async fn preview(&self, args: &Value) -> Option<String> {
        let spec = Self::spec_from(args).ok()?;
        let dir = match spec.scope {
            SkillScope::Project => "<workspace>/.medha/skills",
            SkillScope::User => "~/.medha/skills",
        };
        Some(format!(
            "Save skill '{}' ({} scope) → {dir}/{}/SKILL.md\n\n{}",
            spec.name,
            spec.scope.as_str(),
            spec.name,
            spec.render()
        ))
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let spec = Self::spec_from(args)?;
        let path = self.store.save(&spec, &self.known_tools).map_err(ToolError::Failed)?;
        Ok(json!({
            "saved": true,
            "name": spec.name,
            "scope": spec.scope.as_str(),
            "path": path.display().to_string(),
            "note": "Available to load with skill.load; appears in the skills list next session."
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), body).unwrap();
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("medha-skills-test-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const DEPLOY: &str = "---\nname = \"deploy-fly\"\ndescription = \"Deploy a FastAPI app to Fly.io\"\ntriggers = [\"deploy\", \"fly.io\"]\nrequired_tools = [\"shell.exec\"]\nversion = 1\n---\n\n## Steps\n1. flyctl launch\n";

    #[test]
    fn parses_valid_frontmatter_and_body() {
        let (fm, body) = parse_skill_md(DEPLOY).unwrap();
        assert_eq!(fm.name, "deploy-fly");
        assert_eq!(fm.triggers, vec!["deploy", "fly.io"]);
        assert!(body.starts_with("## Steps"));
    }

    #[test]
    fn rejects_missing_name_or_description_and_fences() {
        assert!(parse_skill_md("---\ndescription = \"x\"\n---\nbody").is_err());
        assert!(parse_skill_md("---\nname = \"x\"\n---\nbody").is_err());
        assert!(parse_skill_md("no fence here").is_err());
        assert!(parse_skill_md("---\nname = \"x\"\ndescription = \"y\"\nbody").is_err()); // no closing fence
    }

    #[test]
    fn project_shadows_user_and_reports_it() {
        let root = tmp();
        let (proj, user) = (root.join("proj"), root.join("user"));
        write_skill(&proj, "deploy-fly", DEPLOY);
        write_skill(&user, "deploy-fly", DEPLOY);
        write_skill(&user, "rust-review", "---\nname = \"rust-review\"\ndescription = \"Review Rust\"\n---\nbody");
        let store = SkillStore::new(proj, Some(user));
        let disc = store.discover(&tools(&["shell.exec"]));
        assert_eq!(disc.effective().count(), 2); // deploy-fly (project) + rust-review (user)
        let shadowed: Vec<_> = disc.listings.iter().filter(|l| l.shadowed).collect();
        assert_eq!(shadowed.len(), 1);
        assert_eq!(shadowed[0].skill.scope, SkillScope::User);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parse_failure_is_skipped_with_reason_not_fatal() {
        let root = tmp();
        let proj = root.join("proj");
        write_skill(&proj, "good", DEPLOY);
        write_skill(&proj, "bad", "no frontmatter at all");
        let store = SkillStore::new(proj, Some(root.join("user")));
        let disc = store.discover(&tools(&["shell.exec"]));
        assert_eq!(disc.effective().count(), 1);
        assert_eq!(disc.errors.len(), 1);
        assert!(disc.errors[0].0.ends_with("SKILL.md"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn required_tools_unavailable_marks_and_load_errors() {
        let root = tmp();
        let proj = root.join("proj");
        write_skill(&proj, "deploy-fly", DEPLOY);
        let store = SkillStore::new(proj, Some(root.join("user")));
        let known = tools(&["fs.read"]); // shell.exec missing
        let disc = store.discover(&known);
        let l = disc.effective().next().unwrap();
        assert!(!l.available());
        assert_eq!(l.missing_tools, vec!["shell.exec"]);
        // manifest marks it unavailable
        assert!(store.manifest(&known, None).contains("(unavailable: needs shell.exec)"));
        // load returns a structured error naming the tool
        let err = store.load("deploy-fly", &known).unwrap_err();
        assert!(err.contains("shell.exec"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn load_returns_body_and_unknown_name_lists_available() {
        let root = tmp();
        let proj = root.join("proj");
        write_skill(&proj, "deploy-fly", DEPLOY);
        let store = SkillStore::new(proj, Some(root.join("user")));
        let known = tools(&["shell.exec"]);
        let v = store.load("deploy-fly", &known).unwrap();
        assert!(v["procedure"].as_str().unwrap().contains("flyctl launch"));
        let err = store.load("nope", &known).unwrap_err();
        assert!(err.contains("deploy-fly"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn manifest_is_empty_with_no_skills() {
        let root = tmp();
        let store = SkillStore::new(root.join("proj"), Some(root.join("user")));
        assert_eq!(store.manifest(&tools(&["shell.exec"]), None), "");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn manifest_trims_above_threshold_by_prompt_match() {
        let root = tmp();
        let proj = root.join("proj");
        for i in 0..35 {
            let body = format!(
                "---\nname = \"skill-{i}\"\ndescription = \"d{i}\"\ntriggers = [\"kw{i}\"]\n---\nbody"
            );
            write_skill(&proj, &format!("skill-{i}"), &body);
        }
        let store = SkillStore::new(proj, Some(root.join("user")));
        let m = store.manifest(&tools(&[]), Some("please do kw7 now"));
        assert!(m.contains("skill-7"));
        assert!(m.contains("and 34 more"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn save_writes_valid_file_and_roundtrips() {
        let root = tmp();
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let store = SkillStore::new(proj.clone(), Some(root.join("user")));
        let spec = SaveSpec {
            name: "my-skill".into(),
            description: "Does a thing".into(),
            triggers: vec!["thing".into()],
            domains: vec![],
            required_tools: vec!["fs.read".into()],
            procedure: "## Steps\n1. do it".into(),
            scope: SkillScope::Project,
        };
        let known = tools(&["fs.read"]);
        let path = store.save(&spec, &known).unwrap();
        assert!(path.ends_with("my-skill/SKILL.md"));
        // The written file re-parses and loads.
        let v = store.load("my-skill", &known).unwrap();
        assert!(v["procedure"].as_str().unwrap().contains("do it"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skill_load_tool_parses_args_and_returns_body() {
        let root = tmp();
        let proj = root.join("proj");
        write_skill(&proj, "deploy-fly", DEPLOY);
        let store = Arc::new(SkillStore::new(proj, Some(root.join("user"))));
        let known = Arc::new(tools(&["shell.exec"]));
        let tool = SkillLoad { store, known_tools: known };
        // happy path
        let v = futures::executor::block_on(tool.execute(&json!({"name": "deploy-fly"}))).unwrap();
        assert!(v["procedure"].as_str().unwrap().contains("flyctl launch"));
        // missing arg → Args error
        assert!(matches!(
            futures::executor::block_on(tool.execute(&json!({}))),
            Err(ToolError::Args(_))
        ));
        // unknown skill → Failed error naming what exists
        assert!(matches!(
            futures::executor::block_on(tool.execute(&json!({"name": "nope"}))),
            Err(ToolError::Failed(m)) if m.contains("deploy-fly")
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skill_save_tool_writes_and_previews() {
        let root = tmp();
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let store = Arc::new(SkillStore::new(proj, Some(root.join("user"))));
        let known = Arc::new(tools(&["fs.read"]));
        let tool = SkillSave { store: store.clone(), known_tools: known.clone() };
        let args = json!({
            "name": "note-taker",
            "description": "Capture a decision as a note",
            "procedure": "## Steps\n1. write it down",
            "required_tools": ["fs.read"],
            "scope": "project"
        });
        // preview renders the full SKILL.md that would be written
        let preview = futures::executor::block_on(tool.preview(&args)).unwrap();
        assert!(preview.contains("name = \"note-taker\""));
        assert!(preview.contains("write it down"));
        // execute writes it, and it round-trips through load
        let out = futures::executor::block_on(tool.execute(&args)).unwrap();
        assert_eq!(out["saved"], json!(true));
        let loaded = store.load("note-taker", &known).unwrap();
        assert!(loaded["procedure"].as_str().unwrap().contains("write it down"));
        // missing required field → Args error
        assert!(matches!(
            futures::executor::block_on(tool.execute(&json!({"name": "x"}))),
            Err(ToolError::Args(_))
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn save_rejects_bad_name_duplicate_and_unknown_tools() {
        let root = tmp();
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let store = SkillStore::new(proj, Some(root.join("user")));
        let known = tools(&["fs.read"]);
        let base = SaveSpec {
            name: "ok-name".into(),
            description: "d".into(),
            triggers: vec![],
            domains: vec![],
            required_tools: vec![],
            procedure: "body".into(),
            scope: SkillScope::Project,
        };
        // bad name
        assert!(store.save(&SaveSpec { name: "Bad Name".into(), ..base.clone() }, &known).is_err());
        // unknown required tool
        assert!(store
            .save(&SaveSpec { required_tools: vec!["web.crawl".into()], ..base.clone() }, &known)
            .is_err());
        // empty procedure
        assert!(store.save(&SaveSpec { procedure: "   ".into(), ..base.clone() }, &known).is_err());
        // first save ok, duplicate rejected
        assert!(store.save(&base, &known).is_ok());
        assert!(store.save(&base, &known).is_err());
        std::fs::remove_dir_all(&root).ok();
    }
}
