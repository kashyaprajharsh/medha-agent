//! Skill discovery, loading, and authoring. Writes use the human approval gate.
//! `SKILL.md` accepts YAML frontmatter and legacy TOML.

use crate::{Tool, ToolError};
use async_trait::async_trait;
use futures::StreamExt;
use kernel::{BlastRadius, ToolCategory};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_SKILL_MD_BYTES: usize = 128 * 1024;
const MAX_MANIFEST_LIST_ITEMS: usize = 64;
const MAX_MANIFEST_CHARS: usize = 32 * 1024;
const MAX_INSTALL_FILES: usize = 256;
const MAX_INSTALL_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_INSTALL_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_BUNDLED_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_BUNDLED_LINES_PER_READ: usize = 1_000;
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30);
const PROVENANCE_FILE: &str = ".medha-source.json";

/// Compatibility is limited to skill metadata/instructions. Do not install
/// executable aliases: dispatch and permission narrowing still use real tools.
fn legacy_tool(name: &str) -> Option<(&'static str, Value)> {
    let (tool, extra) = match name {
        "fs.read" | "image.view" | "read_artifact" => ("read", json!({})),
        "word_count" => ("read", json!({"count": true})),
        "fs.write" | "fs.edit" | "multi_edit" => ("edit", json!({})),
        "fs.list" => ("ls", json!({})),
        "code_outline" | "references" => ("code", json!({})),
        "skill.load" | "skill.list" => ("skill", json!({})),
        "agent.followup" => ("agent.spawn", json!({})),
        _ => {
            let (namespace, op) = name.split_once('.')?;
            match (namespace, op) {
                ("web", "search" | "fetch" | "crawl") => ("web", json!({"op": op})),
                ("memory", "write" | "update" | "forget" | "search") => {
                    ("memory", json!({"op": op}))
                }
                (
                    "lsp",
                    "status" | "diagnostics" | "definition" | "references" | "hover" | "symbols"
                    | "implementation" | "document_symbols" | "call_hierarchy",
                ) => ("lsp", json!({"op": op})),
                ("agent", "list" | "transcript" | "steer" | "message" | "cancel" | "wait") => {
                    ("agent", json!({"action": op}))
                }
                ("task", "kill" | "remove") => ("task.control", json!({"op": op})),
                _ => return None,
            }
        }
    };
    Some((tool, extra))
}

fn requirement_available(name: &str, known: &HashSet<String>) -> bool {
    known.contains(name) || legacy_tool(name).is_some_and(|(tool, _)| known.contains(tool))
}

/// Where a skill was found. Project (workspace-committed) shadows user (personal).
/// Plugin skills are namespaced by their plugin id, so they never shadow either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    Project,
    User,
    Plugin,
}

impl SkillScope {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillScope::Project => "project",
            SkillScope::User => "user",
            SkillScope::Plugin => "plugin",
        }
    }
}

/// Recorded source and security receipt for an installed skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillProvenance {
    pub source: String,
    pub kind: String,
    pub revision: Option<String>,
    pub installed_at: u64,
    /// Content hash of the package as installed (`sha256:<hex>`), covering
    /// every file including `SKILL.md`. Drives update drift-detection and the
    /// skills lockfile. `None` for packages installed before hashing existed.
    #[serde(default)]
    pub content_hash: Option<String>,
    /// Guard verdict recorded at install: `"safe"` or `"caution"` (dangerous
    /// never installs). Lets a surface show a trust receipt without re-scanning.
    /// `None` for packages installed before the guard existed.
    #[serde(default)]
    pub scan_verdict: Option<String>,
}

/// Result of installing a complete skill package.
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub name: String,
    pub path: PathBuf,
    pub source: String,
    pub revision: Option<String>,
    pub files: usize,
    pub bytes: usize,
    pub replaced: bool,
    /// Content hash of the installed package (`sha256:<hex>`) — recorded in the
    /// lockfile and compared on update to detect upstream changes.
    pub content_hash: String,
    /// Guard verdict for the installed package: `"safe"` or `"caution"`. A
    /// `"dangerous"` verdict aborts the install, so it never reaches a report.
    pub scan_verdict: &'static str,
    /// Human-readable guard findings (`"file:line — reason"`), empty when safe.
    /// Surfaced by the caller so a caution install is never silent.
    pub scan_findings: Vec<String>,
}

/// YAML frontmatter with a legacy TOML read path. Unknown keys are ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    triggers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required_tools: Vec<String>,
    #[serde(
        default = "default_version",
        skip_serializing_if = "is_default_version"
    )]
    version: u32,
}

fn default_version() -> u32 {
    1
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde skip_serializing_if signature
fn is_default_version(v: &u32) -> bool {
    *v == 1
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

/// Reads skill configuration directly so discovery never raises sandbox prompts.
pub struct SkillStore {
    project_dir: PathBuf,
    /// `None` when the platform has no home directory (user scope unavailable).
    user_dir: Option<PathBuf>,
    /// Optional LLM escalation for the guard's ambiguous (Caution) verdicts.
    /// Attached by the surface that owns a model; `None` = regex-only.
    judge: Option<Arc<dyn crate::judge::SkillJudge>>,
    /// Parsed once from a hash-verified plugin package and served from memory,
    /// so editing the package after approval cannot change what the model loads.
    /// Replaced wholesale when plugins are enabled or disabled mid-session.
    plugin_skills: std::sync::RwLock<Vec<Skill>>,
}

impl SkillStore {
    pub fn new(project_dir: PathBuf, user_dir: Option<PathBuf>) -> Self {
        Self {
            project_dir,
            user_dir,
            judge: None,
            plugin_skills: std::sync::RwLock::new(Vec::new()),
        }
    }

    /// Replaces every plugin skill; each `(plugin_id, dir)` is served as
    /// `<plugin_id>:<name>`. Returns one message per skill that could not load.
    pub fn set_plugin_skills(&self, sources: &[(String, PathBuf)]) -> Vec<String> {
        let mut skills: Vec<Skill> = Vec::new();
        let mut problems = Vec::new();
        for (plugin_id, dir) in sources {
            match plugin_skill(plugin_id, dir) {
                Ok(skill) if skills.iter().any(|s| s.name == skill.name) => {
                    problems.push(format!("plugin skill '{}' is declared twice", skill.name));
                }
                Ok(skill) => skills.push(skill),
                Err(problem) => {
                    problems.push(format!("{plugin_id}: skill was not loaded: {problem}"))
                }
            }
        }
        if let Ok(mut current) = self.plugin_skills.write() {
            *current = skills;
        }
        problems
    }

    fn plugin_skill_list(&self) -> Vec<Skill> {
        self.plugin_skills
            .read()
            .map(|skills| skills.clone())
            .unwrap_or_default()
    }

    /// Adds LLM review for ambiguous guard verdicts; otherwise review is regex-only.
    pub fn with_judge(mut self, judge: Arc<dyn crate::judge::SkillJudge>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Scan both scopes. Project is scanned first; a same-named user skill is
    /// marked `shadowed`. `known_tools` (the registry's registered tool names)
    /// drives the `missing_tools` availability check.
    pub fn discover(&self, known_tools: &HashSet<String>) -> Discovery {
        let mut out = Discovery::default();
        let mut seen: HashSet<String> = HashSet::new();

        let dirs: [(Option<&PathBuf>, SkillScope); 2] = [
            (Some(&self.project_dir), SkillScope::Project),
            (self.user_dir.as_ref(), SkillScope::User),
        ];
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
                            .filter(|t| !requirement_available(t, known_tools))
                            .cloned()
                            .collect();
                        out.listings.push(SkillListing {
                            skill,
                            shadowed,
                            missing_tools,
                        });
                    }
                    Err(reason) => out.errors.push((path, reason)),
                }
            }
        }
        for skill in &self.plugin_skill_list() {
            let missing_tools = skill
                .required_tools
                .iter()
                .filter(|t| !requirement_available(t, known_tools))
                .cloned()
                .collect();
            out.listings.push(SkillListing {
                skill: skill.clone(),
                shadowed: false,
                missing_tools,
            });
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
                format!(
                    "no skill named '{name}'; available skills: {}",
                    available.join(", ")
                )
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
        // Skills ship with bundled files (references, scripts, templates) that
        // the procedure mentions by relative path. Surface the skill's dir and
        // its files so those references are resolvable, not dead ends.
        let dir = s.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let files = bundled_files(&dir);
        let mut out = json!({
            "name": s.name,
            "description": s.description,
            "scope": s.scope.as_str(),
            "required_tools": s.required_tools,
            "dir": dir.display().to_string(),
            "procedure": s.body,
        });
        let migrations: Vec<Value> = s
            .required_tools
            .iter()
            .filter_map(|old| {
                if known_tools.contains(old) {
                    return None;
                }
                let (tool, add_arguments) = legacy_tool(old)?;
                Some(json!({"old_tool": old, "tool": tool, "add_arguments": add_arguments}))
            })
            .collect();
        if !migrations.is_empty() {
            out["tool_migrations"] = json!(migrations);
            out["tool_migration_note"] = json!(
                "This procedure predates tool consolidation. Use the mapped tool names, retaining \
                 the original arguments and adding the listed arguments. Follow the current tool \
                 schemas; do not call the retired names. The procedure on disk is unchanged."
            );
        }
        if !files.is_empty() {
            // Give each bundled file BOTH its relative name (pass to `skill`
            // as `file` to READ text) and its absolute `abs_path` (use with
            // shell.exec to RUN) — so the agent never has to reconstruct a path.
            let entries: Vec<Value> = files
                .iter()
                .map(|f| json!({ "file": f, "abs_path": dir.join(f).display().to_string() }))
                .collect();
            out["bundled_files"] = json!(entries);
            out["note"] = json!(format!(
                "These files live in `{dir}` (ABSOLUTE, OUTSIDE your workspace) — NOT your working \
                 directory, so never conclude one is missing by listing/globbing the workspace. \
                 Each entry gives `abs_path` (the full path). To READ a text reference, call \
                 skill with `name`+`file`; to RUN a script, pass its `abs_path` to shell.exec \
                 (a relative `scripts/foo.py` in the procedure is that entry's `abs_path`).",
                dir = dir.display()
            ));
        }
        Ok(out)
    }

    /// Read a page from a text file bundled with an effective skill. This is a
    /// dedicated path because user skills normally live outside the workspace
    /// sandbox. Relative traversal, hidden paths, symlinks, binary files, and
    /// context-flooding reads are rejected.
    pub fn load_file(
        &self,
        name: &str,
        file: &str,
        known_tools: &HashSet<String>,
        line_start: usize,
        line_limit: usize,
    ) -> Result<Value, String> {
        validate_reference(name)?;
        let relative = Path::new(file);
        if file.is_empty()
            || relative.components().any(|component| {
                !matches!(component, std::path::Component::Normal(_))
                    || matches!(component, std::path::Component::Normal(part) if part.to_string_lossy().starts_with('.'))
            })
            || relative == Path::new("SKILL.md")
        {
            return Err("file must be a visible bundled path relative to the skill directory".into());
        }
        let discovery = self.discover(known_tools);
        let listing = discovery
            .effective()
            .find(|listing| listing.skill.name == name)
            .ok_or_else(|| format!("no skill named '{name}' is installed"))?;
        if !listing.available() {
            return Err(format!(
                "skill '{name}' needs tools not available in this session: {}",
                listing.missing_tools.join(", ")
            ));
        }
        let dir = listing
            .skill
            .path
            .parent()
            .ok_or_else(|| format!("skill '{name}' has no package directory"))?;
        let mut target = dir.to_path_buf();
        for component in relative.components() {
            let std::path::Component::Normal(part) = component else {
                return Err("invalid bundled file path".into());
            };
            target.push(part);
            let metadata = std::fs::symlink_metadata(&target)
                .map_err(|e| format!("bundled file '{}': {e}", relative.display()))?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "bundled file '{}' crosses a symlink, which is not allowed",
                    relative.display()
                ));
            }
        }
        if !target.is_file() {
            return Err(format!(
                "bundled path '{}' is not a file",
                relative.display()
            ));
        }
        let content = read_text_limited(&target, MAX_BUNDLED_TEXT_BYTES)
            .map_err(|e| format!("reading bundled file: {e}"))?;
        let total_lines = content.lines().count();
        let start = line_start.max(1);
        let limit = line_limit.clamp(1, MAX_BUNDLED_LINES_PER_READ);
        let page = content
            .lines()
            .skip(start - 1)
            .take(limit)
            .collect::<Vec<_>>();
        let end = if page.is_empty() {
            start.saturating_sub(1)
        } else {
            start + page.len() - 1
        };
        Ok(json!({
            "name": name,
            "file": crate::portable_rel(relative),
            "content": page.join("\n"),
            "line_start": start,
            "line_end": end,
            "total_lines": total_lines,
            "has_more": end < total_lines,
        }))
    }

    /// Large catalogs retain prompt matches and an omitted count.
    pub fn manifest(&self, known_tools: &HashSet<String>, prompt: Option<&str>) -> String {
        if !known_tools.contains("skill") {
            return String::new();
        }
        const TRIM_ABOVE: usize = 30;
        let disc = self.discover(known_tools);
        let all: Vec<&SkillListing> = disc.effective().collect();
        if all.is_empty() {
            return String::new();
        }

        let (shown, mut hidden): (Vec<&SkillListing>, usize) = if all.len() > TRIM_ABOVE {
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
             matches the request, call skill with its name and follow it before doing the task your own way.\n",
        );
        for (i, l) in shown.iter().enumerate() {
            let s = &l.skill;
            let mut line = format!("- {} — {}", s.name, s.description);
            if !s.triggers.is_empty() {
                line.push_str(&format!("  [triggers: {}]", s.triggers.join(", ")));
            }
            if !l.available() {
                line.push_str(&format!(
                    "  (unavailable: needs {})",
                    l.missing_tools.join(", ")
                ));
            }
            line.push('\n');
            if lines.len().saturating_add(line.len()) > MAX_MANIFEST_CHARS {
                hidden = hidden.saturating_add(shown.len() - i);
                break;
            }
            lines.push_str(&line);
        }
        if hidden > 0 {
            lines.push_str(&format!(
                "- … and {hidden} more — call skill without a name to browse them\n"
            ));
        }
        lines
    }

    /// Return the compact skill index for `skill.list`. Procedures are omitted
    /// so listing a catalog cannot inject every workflow into context.
    pub fn list(&self, known_tools: &HashSet<String>) -> Value {
        let disc = self.discover(known_tools);
        let skills: Vec<Value> = disc
            .effective()
            .map(|l| {
                let s = &l.skill;
                json!({
                    "name": s.name,
                    "description": s.description,
                    "scope": s.scope.as_str(),
                    "available": l.available(),
                    "missing_tools": l.missing_tools,
                    "triggers": s.triggers,
                    "domains": s.domains,
                })
            })
            .collect();
        let errors: Vec<Value> = disc
            .errors
            .iter()
            .map(|(path, reason)| json!({ "path": path.display().to_string(), "error": reason }))
            .collect();
        json!({ "skills": skills, "errors": errors })
    }

    /// Writes the human-approved skill and increments an existing version.
    pub fn save(
        &self,
        spec: &SaveSpec,
        known_tools: &HashSet<String>,
    ) -> Result<(PathBuf, u32), String> {
        validate_name(&spec.name)?;
        validate_manifest_text("description", &spec.description, 1024, false)?;
        validate_manifest_list("triggers", &spec.triggers)?;
        validate_manifest_list("domains", &spec.domains)?;
        validate_manifest_list("required_tools", &spec.required_tools)?;
        if spec.procedure.trim().is_empty() {
            return Err("procedure body must not be empty".into());
        }
        if spec.procedure.len() > MAX_SKILL_MD_BYTES {
            return Err(format!(
                "procedure body is {} bytes; keep it at or below {MAX_SKILL_MD_BYTES}",
                spec.procedure.len()
            ));
        }
        // A skill installed before a tool merge names the verb it was written
        // against; resolve it rather than declaring the skill unusable.
        let unknown: Vec<&String> = spec
            .required_tools
            .iter()
            .filter(|t| !requirement_available(t, known_tools))
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "required_tools not registered in this session: {}",
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let dir = match spec.scope {
            SkillScope::Project => &self.project_dir,
            SkillScope::User => self
                .user_dir
                .as_ref()
                .ok_or("no user home directory available; save to project scope instead")?,
            SkillScope::Plugin => return Err("plugin skills are read-only".into()),
        };
        let target = dir.join(&spec.name).join("SKILL.md");
        let version = next_version(&target);
        let content = spec.render(version);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        atomic_write(&target, content.as_bytes())?;
        Ok((target, version))
    }

    /// The existing on-disk content a save would replace, if any — lets the
    /// approval card preview an update as a diff instead of a full re-dump.
    pub fn existing_content(&self, spec: &SaveSpec) -> Option<(PathBuf, String)> {
        let dir = match spec.scope {
            SkillScope::Project => &self.project_dir,
            SkillScope::User => self.user_dir.as_ref()?,
            SkillScope::Plugin => return None,
        };
        let target = dir.join(&spec.name).join("SKILL.md");
        let text = std::fs::read_to_string(&target).ok()?;
        Some((target, text))
    }

    /// Describe an effective skill without loading its procedure into model
    /// context. Unlike `load`, this also works for skills whose required tools
    /// are unavailable, making it suitable for `/skill info` diagnostics.
    pub fn inspect(&self, name: &str, known_tools: &HashSet<String>) -> Result<Value, String> {
        validate_reference(name)?;
        let disc = self.discover(known_tools);
        let Some(listing) = disc.effective().find(|l| l.skill.name == name) else {
            let names = disc
                .effective()
                .map(|l| l.skill.name.as_str())
                .collect::<Vec<_>>();
            return Err(if names.is_empty() {
                format!("no skill named '{name}'; no skills are installed")
            } else {
                format!(
                    "no skill named '{name}'; available skills: {}",
                    names.join(", ")
                )
            });
        };
        let skill = &listing.skill;
        let dir = skill
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let provenance = (skill.scope == SkillScope::User)
            .then(|| self.provenance(name))
            .flatten();
        Ok(json!({
            "name": skill.name,
            "description": skill.description,
            "scope": skill.scope.as_str(),
            "version": skill.version,
            "path": skill.path.display().to_string(),
            "dir": dir.display().to_string(),
            "available": listing.available(),
            "required_tools": skill.required_tools,
            "missing_tools": listing.missing_tools,
            "bundled_files": bundled_files(&dir),
            "source": provenance.as_ref().map(|p| p.source.as_str()),
            "source_kind": provenance.as_ref().map(|p| p.kind.as_str()),
            "revision": provenance.as_ref().and_then(|p| p.revision.as_deref()),
        }))
    }

    /// Hash of a user-installed package as it currently sits on disk. Compared
    /// against the recorded provenance hash to detect local edits (drift), and
    /// against a re-fetched upstream to detect available updates. `None` if the
    /// package or user scope is absent.
    pub fn installed_hash(&self, name: &str) -> Option<String> {
        validate_name(name).ok()?;
        let dir = self.user_dir.as_ref()?.join(name);
        dir.join("SKILL.md").is_file().then(|| hash_package(&dir))
    }

    /// Provenance recorded for a user-installed package, if it has one.
    pub fn provenance(&self, name: &str) -> Option<SkillProvenance> {
        validate_name(name).ok()?;
        let dir = self.user_dir.as_ref()?.join(name);
        let text = std::fs::read_to_string(dir.join(PROVENANCE_FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Remove a user-scoped skill package. Project skills are committed workspace
    /// configuration and are deliberately not deleted by this convenience path.
    pub fn remove_user(&self, name: &str) -> Result<PathBuf, String> {
        validate_name(name)?;
        let root = self
            .user_dir
            .as_ref()
            .ok_or("no user home directory available")?;
        let target = root.join(name);
        let md = target.join("SKILL.md");
        if !md.is_file() {
            return Err(format!("no user skill named '{name}' is installed"));
        }
        std::fs::remove_dir_all(&target)
            .map_err(|e| format!("removing {}: {e}", target.display()))?;
        Ok(target)
    }

    /// Install a skill package from a GitHub folder URL, a raw `SKILL.md` URL, a
    /// local directory, or a local `SKILL.md`. Validated before anything is
    /// written; the kebab-validated frontmatter name cannot escape the skills
    /// dir. Installing over an existing name is an upgrade.
    pub async fn install_from(&self, src: &str) -> Result<InstallReport, String> {
        let user_dir = self
            .user_dir
            .as_ref()
            .ok_or("no user home directory available")?
            .clone();
        std::fs::create_dir_all(&user_dir).map_err(|e| e.to_string())?;
        let stage = unique_sibling(&user_dir, ".installing-skill");
        std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;

        let staged = async {
            let mut budget = CopyBudget::default();
            let (kind, revision) = if let Some(tree) = parse_github_tree_url(src)? {
                let revision = download_github_tree(&tree, &stage, &mut budget).await?;
                ("github-folder".to_string(), Some(revision))
            } else if src.starts_with("http://") || src.starts_with("https://") {
                let body = fetch_limited(src, MAX_SKILL_MD_BYTES).await?;
                let text =
                    std::str::from_utf8(&body).map_err(|_| format!("{src} is not UTF-8 text"))?;
                let head = text
                    .trim_start()
                    .get(..15)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if head.starts_with("<!doctype") || head.starts_with("<html") {
                    return Err(format!(
                        "{src} returned a web page — use a GitHub /tree/ folder URL \
                         for a packaged skill, or a raw SKILL.md URL"
                    ));
                }
                budget.admit(body.len(), Path::new("SKILL.md"))?;
                std::fs::write(stage.join("SKILL.md"), body).map_err(|e| e.to_string())?;
                ("raw-url".to_string(), None)
            } else {
                let path = Path::new(src);
                if path.is_dir() {
                    copy_dir_recursive(path, &stage, &mut budget)?;
                    ("local-folder".to_string(), None)
                } else {
                    let text = read_text_limited(path, MAX_SKILL_MD_BYTES)?;
                    budget.admit(text.len(), path)?;
                    std::fs::write(stage.join("SKILL.md"), text).map_err(|e| e.to_string())?;
                    ("local-file".to_string(), None)
                }
            };

            let md_path = stage.join("SKILL.md");
            let text = read_text_limited(&md_path, MAX_SKILL_MD_BYTES)
                .map_err(|e| format!("reading staged {}: {e}", md_path.display()))?;
            let (fm, _) = parse_skill_md(&text)
                .map_err(|e| format!("{src} is not a valid skill package: {e}"))?;
            // Hash the package now — no provenance sidecar exists yet, so the
            // hash covers only real content and never itself. Provenance is
            // written after the scan (below) so it can record the verdict.
            let content_hash = hash_package(&stage);
            Ok::<_, String>((
                fm.name,
                fm.description,
                kind,
                revision,
                content_hash,
                budget,
            ))
        }
        .await;

        let (name, description, kind, revision, content_hash, budget) = match staged {
            Ok(result) => result,
            Err(e) => {
                std::fs::remove_dir_all(&stage).ok();
                return Err(e);
            }
        };
        // Screen the staged package before it is committed — an untrusted
        // SKILL.md becomes model context and its scripts may run, so it is
        // scanned at install exactly as a command is scanned at exec. A
        // dangerous verdict aborts (nothing is written); caution installs but
        // its findings ride back in the report so the surface can warn.
        let scan = match scan_staged(&stage) {
            Ok(scan) => scan,
            Err(error) => {
                std::fs::remove_dir_all(&stage).ok();
                return Err(error);
            }
        };
        let scan_findings = format_findings(&scan.findings);
        if scan.verdict == policy::guard::ScanVerdict::Dangerous {
            std::fs::remove_dir_all(&stage).ok();
            return Err(format!(
                "refusing to install '{name}': the package contains dangerous content:\n  {}",
                scan_findings.join("\n  ")
            ));
        }
        // Regex flagged Caution (ambiguous) → escalate to the LLM judge to
        // refine it. The judge can clear it (safe), keep it (caution), or block
        // it (dangerous). No judge / a judge error keeps the regex Caution — a
        // model hiccup never blocks a legitimate skill (fail-safe).
        let scan_verdict = if scan.verdict == policy::guard::ScanVerdict::Caution {
            match &self.judge {
                Some(judge) => {
                    let req = crate::judge::JudgeRequest {
                        name: name.clone(),
                        description: description.clone(),
                        findings: scan_findings.clone(),
                        content: gather_flagged_content(&stage, &scan),
                    };
                    match judge.judge(req).await {
                        Ok(o) => match o.verdict {
                            crate::judge::JudgeVerdict::Dangerous => {
                                std::fs::remove_dir_all(&stage).ok();
                                return Err(format!(
                                    "refusing to install '{name}': security review flagged it — {}",
                                    o.reason
                                ));
                            }
                            crate::judge::JudgeVerdict::Safe if scan.requires_explicit_review => {
                                // A model can interpret prose, but it cannot
                                // prove opaque bytes, archives, or executable
                                // modes safe. Structural review is never
                                // silently downgraded.
                                "caution"
                            }
                            crate::judge::JudgeVerdict::Safe => "safe",
                            crate::judge::JudgeVerdict::Caution => "caution",
                        },
                        Err(_) => "caution", // fail-safe: keep the regex verdict
                    }
                }
                None => "caution",
            }
        } else {
            "safe"
        };
        if let Err(error) = quarantine_executable_modes(&stage) {
            std::fs::remove_dir_all(&stage).ok();
            return Err(error);
        }
        // Write provenance now that the verdict is known (the sidecar is a
        // dotfile, so it is excluded from both the hash and the scan above).
        let provenance = SkillProvenance {
            source: src.to_string(),
            kind,
            revision: revision.clone(),
            installed_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            content_hash: Some(content_hash.clone()),
            scan_verdict: Some(scan_verdict.to_string()),
        };
        match serde_json::to_vec_pretty(&provenance) {
            Ok(json) => {
                if let Err(e) = std::fs::write(stage.join(PROVENANCE_FILE), json) {
                    std::fs::remove_dir_all(&stage).ok();
                    return Err(format!("writing skill provenance: {e}"));
                }
            }
            Err(e) => {
                std::fs::remove_dir_all(&stage).ok();
                return Err(format!("serializing skill provenance: {e}"));
            }
        }
        let dest = user_dir.join(&name);
        let replaced = dest.exists();
        if let Err(e) = replace_dir_atomically(&stage, &dest) {
            std::fs::remove_dir_all(&stage).ok();
            return Err(e);
        }
        Ok(InstallReport {
            name,
            path: dest.join("SKILL.md"),
            source: src.to_string(),
            revision,
            files: budget.files,
            bytes: budget.bytes,
            replaced,
            content_hash,
            scan_verdict,
            scan_findings,
        })
    }
}

fn next_version(target: &Path) -> u32 {
    std::fs::read_to_string(target)
        .ok()
        .and_then(|text| parse_skill_md(&text).ok())
        .map(|(fm, _)| fm.version.saturating_add(1))
        .unwrap_or(1)
}

fn bundled_files(dir: &Path) -> Vec<String> {
    const CAP: usize = 100;
    let mut out = Vec::new();
    collect_files(dir, dir, &mut out);
    out.sort();
    out.truncate(CAP);
    out
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(ty) = entry.file_type() else { continue };
        if ty.is_dir() {
            collect_files(root, &path, out);
        } else if ty.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            let rel = crate::portable_rel(rel);
            if rel != "SKILL.md" {
                out.push(rel);
            }
        }
    }
}

/// Screen every staged file with the Skills Guard before committing. `SKILL.md`
/// is added back explicitly since `collect_files` lists only bundled extras. A
/// read failure must never drop a file from the scan.
fn scan_staged(stage: &Path) -> Result<policy::guard::ScanReport, String> {
    let mut rels = Vec::new();
    collect_files(stage, stage, &mut rels);
    rels.push("SKILL.md".to_string());
    rels.sort();
    rels.dedup();
    let mut files = Vec::with_capacity(rels.len());
    for rel in rels {
        let path = stage.join(&rel);
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("cannot security-scan staged file '{rel}': {error}"))?;
        files.push((rel, bytes, is_executable_file(&path)?));
    }
    Ok(policy::guard::scan_package_with_modes(files.iter().map(
        |(path, bytes, executable)| (path.as_str(), bytes.as_slice(), *executable),
    )))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> Result<bool, String> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("reading staged file mode '{}': {error}", path.display()))?;
    Ok(metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(_path: &Path) -> Result<bool, String> {
    Ok(false)
}

/// Installed skill assets are data until a reviewed interpreter invocation
/// consumes them. Removing execute bits prevents a bundled launcher from being
/// run directly after installation; directories retain traversal permissions.
#[cfg(unix)]
fn quarantine_executable_modes(stage: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut rels = Vec::new();
    collect_files(stage, stage, &mut rels);
    rels.push("SKILL.md".to_string());
    rels.sort();
    rels.dedup();
    for rel in rels {
        let path = stage.join(&rel);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("reading staged file mode '{}': {error}", path.display()))?;
        let mode = metadata.permissions().mode();
        if mode & 0o111 != 0 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(mode & !0o111);
            std::fs::set_permissions(&path, permissions).map_err(|error| {
                format!(
                    "quarantining executable skill file '{}': {error}",
                    path.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn quarantine_executable_modes(_stage: &Path) -> Result<(), String> {
    Ok(())
}

/// Deterministic content hash of a package directory (`sha256:<hex>`). Each file
/// contributes its relative path and bytes, length-prefixed and sorted, so
/// enumeration order cannot change the result. Dotfiles are excluded.
fn hash_package(dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut rels = Vec::new();
    collect_files(dir, dir, &mut rels);
    rels.push("SKILL.md".to_string());
    rels.sort();
    rels.dedup();
    let mut h = Sha256::new();
    for rel in &rels {
        if let Ok(bytes) = std::fs::read(dir.join(rel)) {
            h.update((rel.len() as u64).to_le_bytes());
            h.update(rel.as_bytes());
            h.update((bytes.len() as u64).to_le_bytes());
            h.update(&bytes);
        }
    }
    format!("sha256:{:x}", h.finalize())
}

/// Collect the flagged files' contents for the judge, bounded so a large package
/// can't blow up the review prompt. `SKILL.md` (the main doc) is always included,
/// then each distinct flagged file — capped per file and in total.
fn gather_flagged_content(stage: &Path, scan: &policy::guard::ScanReport) -> String {
    const PER_FILE: usize = 6 * 1024;
    const TOTAL: usize = 24 * 1024;
    let mut files = vec!["SKILL.md".to_string()];
    for f in &scan.findings {
        if !files.contains(&f.file) {
            files.push(f.file.clone());
        }
    }
    let mut out = String::new();
    for rel in files {
        if out.len() >= TOTAL {
            break;
        }
        if let Ok(text) = std::fs::read_to_string(stage.join(&rel)) {
            let snippet: String = text.chars().take(PER_FILE).collect();
            out.push_str(&format!("### {rel}\n{snippet}\n\n"));
        }
    }
    out.chars().take(TOTAL).collect()
}

/// Render guard findings as `"file:line — reason"` (or `"file — reason"` for a
/// whole-file finding) for the install report and any surface that shows them.
fn format_findings(findings: &[policy::guard::Finding]) -> Vec<String> {
    findings
        .iter()
        .map(|f| match f.line {
            Some(n) => format!("{}:{n} — {}", f.file, f.reason),
            None => format!("{} — {}", f.file, f.reason),
        })
        .collect()
}

#[derive(Debug, Default)]
struct CopyBudget {
    files: usize,
    bytes: usize,
}

impl CopyBudget {
    fn admit(&mut self, size: usize, path: &Path) -> Result<(), String> {
        if size > MAX_INSTALL_FILE_BYTES {
            return Err(format!(
                "{} is {size} bytes; per-file install limit is {MAX_INSTALL_FILE_BYTES}",
                path.display()
            ));
        }
        self.files = self.files.saturating_add(1);
        self.bytes = self.bytes.saturating_add(size);
        if self.files > MAX_INSTALL_FILES {
            return Err(format!("skill package exceeds {MAX_INSTALL_FILES} files"));
        }
        if self.bytes > MAX_INSTALL_TOTAL_BYTES {
            return Err(format!(
                "skill package exceeds the {MAX_INSTALL_TOTAL_BYTES}-byte total limit"
            ));
        }
        Ok(())
    }
}

/// Bounded recursive copy for local skill packages. Symlinks are rejected, not
/// followed; hidden metadata and VCS directories are skipped.
fn copy_dir_recursive(from: &Path, to: &Path, budget: &mut CopyBudget) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    let entries = std::fs::read_dir(from).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(&name));
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        if ty.is_dir() {
            copy_dir_recursive(&src, &dst, budget)?;
        } else if ty.is_file() {
            let size = entry.metadata().map_err(|e| e.to_string())?.len() as usize;
            budget.admit(size, &src)?;
            std::fs::copy(&src, &dst).map_err(|e| e.to_string())?;
        } else if ty.is_symlink() {
            return Err(format!(
                "skill packages may not contain symlinks: {}",
                src.display()
            ));
        }
    }
    Ok(())
}

fn read_text_limited(path: &Path, cap: usize) -> Result<String, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len() as usize;
    if size > cap {
        return Err(format!(
            "{} is {size} bytes; maximum supported size is {cap}",
            path.display()
        ));
    }
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn unique_sibling(parent: &Path, prefix: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    parent.join(format!("{prefix}-{}-{n}", std::process::id()))
}

/// Replace a package as one same-filesystem transaction. A failed commit restores
/// the previous version; a successful update cannot retain stale bundled files.
fn replace_dir_atomically(stage: &Path, dest: &Path) -> Result<(), String> {
    let parent = dest.parent().ok_or("skill destination has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let backup = unique_sibling(parent, ".replaced-skill");
    let had_old = dest.exists();
    if had_old {
        std::fs::rename(dest, &backup).map_err(|e| format!("staging existing skill: {e}"))?;
    }
    if let Err(e) = std::fs::rename(stage, dest) {
        if had_old {
            std::fs::rename(&backup, dest).ok();
        }
        return Err(format!("committing skill installation: {e}"));
    }
    if had_old {
        std::fs::remove_dir_all(backup).map_err(|e| format!("removing old skill: {e}"))?;
    }
    Ok(())
}

fn atomic_write(target: &Path, content: &[u8]) -> Result<(), String> {
    let parent = target.parent().ok_or("skill file has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temp = unique_sibling(parent, ".writing-skill");
    let mut file = std::fs::File::create(&temp).map_err(|e| e.to_string())?;
    file.write_all(content).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    // Windows refuses to rename an open source file. Unix permits renaming an
    // open inode, which otherwise hides this lifetime bug from local tests.
    drop(file);
    let backup = unique_sibling(parent, ".previous-skill");
    let had_old = target.exists();
    if had_old {
        std::fs::rename(target, &backup).map_err(|e| e.to_string())?;
    }
    if let Err(e) = std::fs::rename(&temp, target) {
        if had_old {
            std::fs::rename(&backup, target).ok();
        }
        std::fs::remove_file(&temp).ok();
        return Err(e.to_string());
    }
    if had_old {
        std::fs::remove_file(backup).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitHubTree {
    owner: String,
    repo: String,
    git_ref: String,
    path: String,
}

/// Parse a normal browser URL such as
/// `https://github.com/anthropics/skills/tree/main/skills/pptx`.
fn parse_github_tree_url(src: &str) -> Result<Option<GitHubTree>, String> {
    let Ok(url) = reqwest::Url::parse(src) else {
        return Ok(None);
    };
    if url.host_str() != Some("github.com") {
        return Ok(None);
    }
    let parts: Vec<String> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .map(|p| {
            urlencoding::decode(p)
                .map(|s| s.into_owned())
                .unwrap_or_else(|_| p.to_string())
        })
        .collect();
    if parts.get(2).map(String::as_str) != Some("tree") {
        return Ok(None);
    }
    if parts.len() < 5 {
        return Err("GitHub tree URL must point to a skill folder".into());
    }
    Ok(Some(GitHubTree {
        owner: parts[0].clone(),
        repo: parts[1].trim_end_matches(".git").to_string(),
        git_ref: parts[3].clone(),
        path: parts[4..].join("/"),
    }))
}

pub(crate) async fn fetch_limited(url: &str, cap: usize) -> Result<Vec<u8>, String> {
    const MAX_REDIRECTS: usize = 5;
    let mut current = reqwest::Url::parse(url).map_err(|e| format!("invalid URL {url}: {e}"))?;
    let mut redirects = 0;
    let response = loop {
        // Resolve once and pin the exact validated public addresses into the
        // connector. This closes both redirect-to-private and DNS-rebinding
        // gaps for third-party `download_url` values.
        let target = crate::resolve_public_url_async(&current)
            .await
            .map_err(|e| e.to_string())?;
        let client = crate::pinned_http_client(&target, "medha-skills/1", INSTALL_TIMEOUT)
            .map_err(|e| e.to_string())?;
        let response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| format!("fetching {current}: {e}"))?;
        crate::validate_connected_peer(response.remote_addr(), &target)
            .map_err(|e| e.to_string())?;

        if !response.status().is_redirection() {
            break response
                .error_for_status()
                .map_err(|e| format!("fetching {current}: {e}"))?;
        }
        if redirects == MAX_REDIRECTS {
            return Err(format!("fetching {url}: too many redirects"));
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| format!("redirect from {current} omitted Location"))?
            .to_str()
            .map_err(|_| format!("redirect from {current} has an invalid Location"))?;
        let next = current
            .join(location)
            .map_err(|e| format!("invalid redirect from {current}: {e}"))?;
        if current.scheme() == "https" && next.scheme() != "https" {
            return Err(format!(
                "blocked redirect downgrade from {current} to {next}"
            ));
        }
        current = next;
        redirects += 1;
    };
    if response.content_length().is_some_and(|n| n > cap as u64) {
        return Err(format!("{current} exceeds the {cap}-byte download limit"));
    }
    let mut out = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("reading {current}: {e}"))?;
        if out.len().saturating_add(chunk.len()) > cap {
            return Err(format!("{current} exceeds the {cap}-byte download limit"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Rewrite a github-folder `source` to pin it at an exact `revision`, so a
/// lockfile reproduces the same bytes regardless of later branch movement.
/// Returns the source unchanged if it isn't a GitHub tree URL.
pub(crate) fn pin_tree_url(source: &str, revision: &str) -> String {
    match parse_github_tree_url(source).ok().flatten() {
        Some(t) => format!(
            "https://github.com/{}/{}/tree/{revision}/{}",
            t.owner, t.repo, t.path
        ),
        None => source.to_string(),
    }
}

/// Re-resolve the commit a github-folder `source` currently points at (its ref
/// resolved *now*), for update detection. `None` if `source` isn't a GitHub
/// tree URL. A pinned-sha source resolves to itself → never reports an update.
pub(crate) async fn current_revision(source: &str) -> Option<String> {
    let tree = parse_github_tree_url(source).ok().flatten()?;
    Some(github_revision(&tree).await)
}

async fn github_revision(tree: &GitHubTree) -> String {
    let url = format!(
        "https://api.github.com/repos/{}/{}/commits/{}",
        urlencoding::encode(&tree.owner),
        urlencoding::encode(&tree.repo),
        urlencoding::encode(&tree.git_ref),
    );
    fetch_limited(&url, 512 * 1024)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|v| v.get("sha").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| tree.git_ref.clone())
}

/// Download a GitHub directory recursively through the Contents API. Every
/// returned path is re-derived relative to the requested root before writing.
async fn download_github_tree(
    tree: &GitHubTree,
    stage: &Path,
    budget: &mut CopyBudget,
) -> Result<String, String> {
    use std::collections::VecDeque;

    let revision = github_revision(tree).await;
    let mut dirs = VecDeque::from([tree.path.clone()]);
    while let Some(dir) = dirs.pop_front() {
        let encoded_path = dir
            .split('/')
            .map(|p| urlencoding::encode(p).into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let api = format!(
            "https://api.github.com/repos/{}/{}/contents/{}?ref={}",
            urlencoding::encode(&tree.owner),
            urlencoding::encode(&tree.repo),
            encoded_path,
            urlencoding::encode(&revision),
        );
        let listing = fetch_limited(&api, 2 * 1024 * 1024).await?;
        let entries: Value = serde_json::from_slice(&listing)
            .map_err(|e| format!("invalid GitHub directory response for {dir}: {e}"))?;
        let entries = entries
            .as_array()
            .ok_or_else(|| format!("GitHub path '{}' is not a directory", tree.path))?;
        for entry in entries {
            let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
            let remote = entry.get("path").and_then(Value::as_str).unwrap_or("");
            let rel = remote
                .strip_prefix(&tree.path)
                .unwrap_or("")
                .trim_start_matches('/');
            let rel_path = Path::new(rel);
            if rel.is_empty()
                || rel_path
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
                || rel_path.components().any(|c| {
                    matches!(c, std::path::Component::Normal(n) if n.to_string_lossy().starts_with('.'))
                })
            {
                continue;
            }
            match kind {
                "dir" => dirs.push_back(remote.to_string()),
                "file" => {
                    let size = entry.get("size").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if size > MAX_INSTALL_FILE_BYTES {
                        return Err(format!(
                            "{remote} is {size} bytes; per-file limit is {MAX_INSTALL_FILE_BYTES}"
                        ));
                    }
                    let download = entry
                        .get("download_url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("GitHub omitted a download URL for {remote}"))?;
                    let bytes = fetch_limited(download, MAX_INSTALL_FILE_BYTES).await?;
                    if size != 0 && size != bytes.len() {
                        return Err(format!("GitHub size changed while downloading {remote}"));
                    }
                    budget.admit(bytes.len(), rel_path)?;
                    let target = stage.join(rel_path);
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    std::fs::write(target, bytes).map_err(|e| e.to_string())?;
                }
                other => return Err(format!("unsupported GitHub entry '{other}' at {remote}")),
            }
        }
    }
    Ok(revision)
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
    /// Render the full `SKILL.md` (YAML frontmatter between `---` fences +
    /// body) — the shape every skill ecosystem reads, so a skill MEDHA saves
    /// works anywhere. `version` is 1 for a new skill, bumped on update.
    pub fn render(&self, version: u32) -> String {
        let fm = Frontmatter {
            name: self.name.clone(),
            description: self.description.trim().to_string(),
            triggers: self.triggers.clone(),
            domains: self.domains.clone(),
            required_tools: self.required_tools.clone(),
            version,
        };
        // serde_yaml::to_string on a plain struct is stable and correctly
        // escapes strings — safer than hand-formatting the frontmatter.
        let frontmatter = serde_yaml::to_string(&fm).unwrap_or_default();
        format!("---\n{frontmatter}---\n\n{}\n", self.procedure.trim())
    }
}

/// kebab-case: lowercase alphanumerics separated by single hyphens.
fn plugin_skill(plugin_id: &str, dir: &Path) -> Result<Skill, String> {
    let path = dir.join("SKILL.md");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let parsed = parse_skill_md(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut skill = build_skill(parsed, SkillScope::Plugin, path);
    skill.name = format!("{plugin_id}:{}", skill.name);
    Ok(skill)
}

/// A lookup name: a local skill, or `<plugin-id>:<skill>` for a plugin skill.
fn validate_reference(name: &str) -> Result<(), String> {
    match name.split_once(':') {
        Some((plugin, skill))
            if !plugin.is_empty()
                && plugin
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-._".contains(c)) =>
        {
            validate_name(skill)
        }
        Some(_) => Err(format!("'{name}' is not a valid plugin skill name")),
        None => validate_name(name),
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if ok {
        Ok(())
    } else {
        Err(format!(
            "name '{name}' must be 1–64 characters of kebab-case (lowercase, digits, single hyphens)"
        ))
    }
}

/// Validate strings rendered into the system manifest. The procedure body is
/// deliberately excluded: it is returned only by the explicit `skill.load`.
fn validate_manifest_text(
    field: &str,
    value: &str,
    max_chars: usize,
    allow_empty: bool,
) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() && !allow_empty {
        return Err(format!("frontmatter '{field}' must not be empty"));
    }
    if value.chars().count() > max_chars {
        return Err(format!(
            "frontmatter '{field}' is {} chars; keep it ≤{max_chars}",
            value.chars().count()
        ));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(format!(
            "frontmatter '{field}' must be a single line without control characters"
        ));
    }
    Ok(())
}

fn validate_manifest_list(field: &str, values: &[String]) -> Result<(), String> {
    if values.len() > MAX_MANIFEST_LIST_ITEMS {
        return Err(format!(
            "frontmatter '{field}' has {} entries; keep it at or below {MAX_MANIFEST_LIST_ITEMS}",
            values.len()
        ));
    }
    for value in values {
        validate_manifest_text(field, value, 80, false)?;
    }
    Ok(())
}

/// Scan a `<dir>/*/SKILL.md` layout, returning each file's parse result. A
/// missing directory yields nothing (the common case — most workspaces have no
/// skills). Sorted by directory name for deterministic ordering.
fn scan_dir(dir: &Path) -> Vec<(PathBuf, Result<ParsedMd, String>)> {
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| {
                let e = e.ok()?;
                let ty = e.file_type().ok()?;
                (ty.is_dir() && !e.file_name().to_string_lossy().starts_with('.')).then(|| e.path())
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort();
    let mut out = Vec::new();
    for sub in entries {
        let md = sub.join("SKILL.md");
        let is_plain_file = std::fs::symlink_metadata(&md)
            .map(|m| m.file_type().is_file())
            .unwrap_or(false);
        if is_plain_file {
            let parsed = std::fs::read_to_string(&md)
                .map_err(|e| e.to_string())
                .and_then(|text| parse_skill_md(&text));
            out.push((md, parsed));
        }
    }
    out
}

/// Reads validated metadata without loading the procedure.
pub(crate) fn skill_meta(text: &str) -> Result<(String, String, u32), String> {
    let (fm, _) = parse_skill_md(text)?;
    Ok((fm.name, fm.description, fm.version))
}

fn parse_skill_md(text: &str) -> Result<ParsedMd, String> {
    if text.len() > MAX_SKILL_MD_BYTES {
        return Err(format!(
            "SKILL.md is {} bytes; maximum supported size is {MAX_SKILL_MD_BYTES}",
            text.len()
        ));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text); // tolerate a BOM
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or("missing opening '---' frontmatter fence")?;
    // Find the closing fence at the start of a line.
    let (fm_src, body) =
        split_at_closing_fence(rest).ok_or("missing closing '---' frontmatter fence")?;
    let mut fm: Frontmatter = serde_yaml::from_str(fm_src).or_else(|yaml_err| {
        toml::from_str(fm_src).map_err(|_| format!("invalid frontmatter YAML: {yaml_err}"))
    })?;
    // A multi-line description is fine on disk; the manifest renders it as one
    // line, so normalize whitespace rather than reject the file.
    fm.description = fm
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    validate_name(&fm.name).map_err(|e| format!("invalid frontmatter name: {e}"))?;
    validate_manifest_text("description", &fm.description, 1024, false)?;
    validate_manifest_list("triggers", &fm.triggers)?;
    validate_manifest_list("domains", &fm.domains)?;
    validate_manifest_list("required_tools", &fm.required_tools)?;
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

/// Reading the skill catalogue: the index, or one procedure in full. Read
/// radius either way, and the index exists to find a name for the load — so
/// they are one tool, told apart by whether a `name` was given.
pub struct SkillTool {
    load: SkillLoad,
    list: SkillList,
}

impl SkillTool {
    pub(crate) fn new(store: Arc<SkillStore>, catalog: Arc<SkillToolCatalog>) -> Self {
        Self {
            load: SkillLoad {
                store: Arc::clone(&store),
                catalog: Arc::clone(&catalog),
            },
            list: SkillList { store, catalog },
        }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> &str {
        "Installed procedures. With a `name`, load that skill's procedure in full; \
         to inspect a bundled reference or script it returned, call again with the \
         relative `file` path and an optional line range. With no `name`, list every \
         installed skill with its description, scope, and availability — use that \
         when the skills manifest says more are hidden."
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
                "name": { "type": "string", "description": "The skill to load (kebab-case). Omit to list what is installed." },
                "file": { "type": "string", "description": "Bundled file path returned by the initial load" },
                "line_start": { "type": "integer", "minimum": 1, "description": "First line to return; default 1" },
                "line_limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum lines; default 400" }
            }
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        match args.get("name") {
            Some(_) => self.load.execute(args).await,
            None => self.list.execute(args).await,
        }
    }
}

/// Loading one procedure by name — the `name` form of `skill`.
pub struct SkillLoad {
    pub store: Arc<SkillStore>,
    pub(crate) catalog: Arc<SkillToolCatalog>,
}

/// The compact index for large catalogues — the no-argument form of `skill`.
pub struct SkillList {
    pub store: Arc<SkillStore>,
    pub(crate) catalog: Arc<SkillToolCatalog>,
}

type LiveToolNames = dyn Fn() -> Vec<String> + Send + Sync;

/// Static registry tools plus tool names projected by live providers such as
/// MCP. Skill availability is read at call time so a server connecting or
/// disconnecting mid-session is reflected without rebuilding the registry.
pub(crate) struct SkillToolCatalog {
    static_tools: Arc<HashSet<String>>,
    live_tools: Option<Arc<LiveToolNames>>,
}

impl SkillToolCatalog {
    pub(crate) fn new(
        static_tools: Arc<HashSet<String>>,
        mcp: Option<Arc<mcp::McpManager>>,
    ) -> Self {
        let live_tools = mcp.map(|manager| {
            Arc::new(move || {
                manager
                    .tool_specs()
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect()
            }) as Arc<LiveToolNames>
        });
        Self {
            static_tools,
            live_tools,
        }
    }

    fn current(&self) -> HashSet<String> {
        let mut tools = self.static_tools.as_ref().clone();
        if let Some(live) = &self.live_tools {
            tools.extend(live());
        }
        tools
    }
}

#[async_trait]
impl Tool for SkillList {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> &str {
        "List installed skills with names, descriptions, scope, and availability. \
         Use when the skills manifest says more skills are hidden."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &Value) -> Result<Value, ToolError> {
        Ok(self.store.list(&self.catalog.current()))
    }
}

#[async_trait]
impl Tool for SkillLoad {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> &str {
        "Load an installed skill's procedure by name. To progressively inspect \
         a bundled reference or script returned by the first call, call again \
         with its relative `file` path and optional line range."
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
                "name": { "type": "string", "description": "The skill's name (kebab-case)" },
                "file": { "type": "string", "description": "Optional bundled file path returned by the initial load" },
                "line_start": { "type": "integer", "minimum": 1, "description": "First line to return; default 1" },
                "line_limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum lines; default 400" }
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let known_tools = self.catalog.current();
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Args("expected string 'name'".into()))?;
        if let Some(file) = args.get("file").and_then(Value::as_str) {
            let line_start = args.get("line_start").and_then(Value::as_u64).unwrap_or(1);
            let line_limit = args
                .get("line_limit")
                .and_then(Value::as_u64)
                .unwrap_or(400);
            if line_start == 0 || line_limit == 0 || line_limit > MAX_BUNDLED_LINES_PER_READ as u64
            {
                return Err(ToolError::Args(
                    "line_start must be ≥1 and line_limit must be 1–1000".into(),
                ));
            }
            self.store
                .load_file(
                    name,
                    file,
                    &known_tools,
                    line_start as usize,
                    line_limit as usize,
                )
                .map_err(ToolError::Failed)
        } else {
            self.store
                .load(name, &known_tools)
                .map_err(ToolError::Failed)
        }
    }
}

/// `skill.save` — reversible-local write, always gated by the approval card
/// (it is on the policy approve list). The card previews the full SKILL.md.
pub struct SkillSave {
    pub store: Arc<SkillStore>,
    pub(crate) catalog: Arc<SkillToolCatalog>,
}

impl SkillSave {
    /// Parse tool args into a validated-shape SaveSpec (field presence only;
    /// SkillStore::save does the semantic validation).
    fn spec_from(args: &Value) -> Result<SaveSpec, ToolError> {
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        let list = |k: &str| -> Result<Vec<String>, ToolError> {
            let Some(value) = args.get(k) else {
                return Ok(Vec::new());
            };
            let values = value
                .as_array()
                .ok_or_else(|| ToolError::Args(format!("expected array '{k}'")))?;
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| ToolError::Args(format!("'{k}[{index}]' must be a string")))
                })
                .collect()
        };
        let name = s("name").ok_or_else(|| ToolError::Args("expected string 'name'".into()))?;
        let description = s("description")
            .ok_or_else(|| ToolError::Args("expected string 'description'".into()))?;
        let procedure =
            s("procedure").ok_or_else(|| ToolError::Args("expected string 'procedure'".into()))?;
        let scope = match args.get("scope").and_then(Value::as_str).unwrap_or("user") {
            "project" => SkillScope::Project,
            "user" => SkillScope::User,
            other => {
                return Err(ToolError::Args(format!(
                    "scope must be 'user' or 'project', got '{other}'"
                )));
            }
        };
        Ok(SaveSpec {
            name,
            description,
            triggers: list("triggers")?,
            domains: list("domains")?,
            required_tools: list("required_tools")?,
            procedure,
            scope,
        })
    }

    /// Nested tool references are ordinary JSON strings, so provider adapters
    /// do not translate the visible `shell_exec` spelling back to canonical
    /// `shell.exec`. Normalize at this semantic boundary and persist only the
    /// provider-independent canonical names.
    fn normalized_spec(
        &self,
        args: &Value,
        known_tools: &HashSet<String>,
    ) -> Result<SaveSpec, ToolError> {
        let mut spec = Self::spec_from(args)?;
        let mut available: Vec<String> = known_tools.iter().cloned().collect();
        available.sort();
        // Preserve legacy names in metadata so loading can explain the merged
        // operation arguments without silently rewriting the user's procedure.
        spec.required_tools = spec
            .required_tools
            .iter()
            .map(|name| {
                if !known_tools.contains(name) && requirement_available(name, known_tools) {
                    Ok(name.clone())
                } else {
                    kernel::canonical_tool_names(std::slice::from_ref(name), &available)
                        .map(|names| names[0].clone())
                        .map_err(|error| {
                            ToolError::Args(format!("invalid required_tools: {error}"))
                        })
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut seen = HashSet::new();
        spec.required_tools.retain(|name| seen.insert(name.clone()));
        Ok(spec)
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
                "description": { "type": "string", "description": "what the skill does and when to use it (≤1024 chars) — shown in the skills list" },
                "procedure": { "type": "string", "description": "the skill body: steps, decision points, known failure modes (markdown)" },
                "triggers": { "type": "array", "items": { "type": "string" }, "description": "match hints (keywords)" },
                "domains": { "type": "array", "items": { "type": "string" } },
                "required_tools": { "type": "array", "items": { "type": "string" }, "description": "registered tool names the procedure needs, such as shell.exec or web; provider-facing underscore aliases are accepted and normalized" },
                "scope": { "type": "string", "enum": ["user", "project"], "description": "user (personal, default) or project (committed with the repo)" }
            },
            "required": ["name", "description", "procedure"]
        })
    }
    async fn preview(&self, args: &Value) -> Option<String> {
        let known_tools = self.catalog.current();
        let spec = self.normalized_spec(args, &known_tools).ok()?;
        let dir = match spec.scope {
            SkillScope::Project => "<workspace>/.medha/skills",
            SkillScope::User => "~/.medha/skills",
            SkillScope::Plugin => return None,
        };
        // Updating an existing skill previews as a diff (what actually
        // changes), not a full re-dump of the file.
        if let Some((path, old)) = self.store.existing_content(&spec) {
            let old_version = next_version(&path).saturating_sub(1).max(1);
            let new = spec.render(old_version + 1);
            let diff = similar::TextDiff::from_lines(&old, &new)
                .unified_diff()
                .context_radius(2)
                .header("current", "proposed")
                .to_string();
            return Some(format!(
                "Update skill '{}' ({} scope, v{} → v{}) → {}\n\n{diff}",
                spec.name,
                spec.scope.as_str(),
                old_version,
                old_version + 1,
                path.display(),
            ));
        }
        Some(format!(
            "Save new skill '{}' ({} scope) → {dir}/{}/SKILL.md\n\n{}",
            spec.name,
            spec.scope.as_str(),
            spec.name,
            spec.render(1)
        ))
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let known_tools = self.catalog.current();
        let spec = self.normalized_spec(args, &known_tools)?;
        let (path, version) = self
            .store
            .save(&spec, &known_tools)
            .map_err(ToolError::Failed)?;
        Ok(json!({
            "saved": true,
            "version": version,
            "updated": version > 1,
            "name": spec.name,
            "scope": spec.scope.as_str(),
            "path": path.display().to_string(),
            "note": "Available to load with skill; appears in the skills list next session."
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn fixed_catalog(names: &[&str]) -> Arc<SkillToolCatalog> {
        Arc::new(SkillToolCatalog {
            static_tools: Arc::new(tools(names)),
            live_tools: None,
        })
    }

    fn catalog_from(static_tools: Arc<HashSet<String>>) -> Arc<SkillToolCatalog> {
        Arc::new(SkillToolCatalog {
            static_tools,
            live_tools: None,
        })
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
    fn plugin_skills_are_namespaced_read_only_and_served_as_approved() {
        let root = tmp();
        write_skill(&root, "pkg", DEPLOY);
        let store = SkillStore::new(root.join("project"), Some(root.join("user")));
        let source = ("dev.me.kit".to_string(), root.join("pkg"));
        let problems = store.set_plugin_skills(&[source.clone(), source.clone()]);
        assert_eq!(
            problems.len(),
            1,
            "a duplicate is reported, not loaded twice"
        );
        let known = tools(&["shell.exec"]);
        let listing = store.discover(&known);
        let skill = &listing.effective().next().unwrap().skill;
        assert_eq!(skill.name, "dev.me.kit:deploy-fly");
        assert_eq!(skill.scope, SkillScope::Plugin);

        std::fs::write(
            root.join("pkg/SKILL.md"),
            DEPLOY.replace("flyctl", "curl evil"),
        )
        .unwrap();
        let loaded = store.load("dev.me.kit:deploy-fly", &known).unwrap();
        assert!(loaded["procedure"].as_str().unwrap().contains("flyctl"));
        assert!(store.inspect("dev.me.kit:deploy-fly", &known).is_ok());
        assert!(store.inspect("dev.me.kit:Bad", &known).is_err());

        assert!(store.set_plugin_skills(&[]).is_empty());
        assert!(
            store.discover(&known).effective().next().is_none(),
            "disabling a plugin removes its skills from the running session"
        );
    }

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
        assert!(
            parse_skill_md("---\nname = \"Not Kebab\"\ndescription = \"y\"\n---\nbody").is_err()
        );
        // A multi-line description is normalized to one line, not rejected.
        let (fm, _) = parse_skill_md(
            "---\nname = \"valid-name\"\ndescription = '''line one\nline two'''\n---\nbody",
        )
        .unwrap();
        assert_eq!(fm.description, "line one line two");
    }

    // The exact shape skills are published in across the ecosystem: YAML
    // frontmatter, a long description, and extra keys we don't model.
    const YAML_SKILL: &str = "---\nname: frontend-design\ndescription: Guidance for distinctive, intentional visual design when building new UI or reshaping an existing one. Helps with aesthetic direction, typography, and making choices that don't read as templated defaults.\nlicense: Complete terms in LICENSE.txt\n---\n\n# Frontend Design\n\nApproach this as the design lead at a small studio.\n";

    #[test]
    fn parses_ecosystem_standard_yaml_skills_unchanged() {
        let (fm, body) = parse_skill_md(YAML_SKILL).unwrap();
        assert_eq!(fm.name, "frontend-design");
        assert!(
            fm.description.chars().count() > 120,
            "real-world descriptions exceed the old 120-char cap"
        );
        assert!(body.starts_with("# Frontend Design"));
    }

    #[test]
    fn saved_skills_render_yaml_that_round_trips() {
        let spec = SaveSpec {
            name: "greet".into(),
            description: "Say hi to the user in their language".into(),
            triggers: vec!["hello".into()],
            domains: vec![],
            required_tools: vec![],
            procedure: "Step 1: say hello".into(),
            scope: SkillScope::User,
        };
        let text = spec.render(1);
        assert!(
            text.contains("name: greet"),
            "YAML frontmatter expected:\n{text}"
        );
        assert!(
            !text.contains("domains"),
            "empty optional fields stay out:\n{text}"
        );
        let (fm, body) = parse_skill_md(&text).unwrap();
        assert_eq!(fm.name, "greet");
        assert_eq!(fm.triggers, vec!["hello"]);
        assert_eq!(body.trim(), "Step 1: say hello");
    }

    #[test]
    fn project_shadows_user_and_reports_it() {
        let root = tmp();
        let (proj, user) = (root.join("proj"), root.join("user"));
        write_skill(&proj, "deploy-fly", DEPLOY);
        write_skill(&user, "deploy-fly", DEPLOY);
        write_skill(
            &user,
            "rust-review",
            "---\nname = \"rust-review\"\ndescription = \"Review Rust\"\n---\nbody",
        );
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
        let known = tools(&["read", "skill"]); // shell.exec missing
        let disc = store.discover(&known);
        let l = disc.effective().next().unwrap();
        assert!(!l.available());
        assert_eq!(l.missing_tools, vec!["shell.exec"]);
        // manifest marks it unavailable
        assert!(
            store
                .manifest(&known, None)
                .contains("(unavailable: needs shell.exec)")
        );
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
    fn install_rejects_dangerous_and_reports_caution() {
        let root = tmp();
        let user = root.join("user");
        std::fs::create_dir_all(&user).unwrap();
        let store = SkillStore::new(root.join("proj"), Some(user.clone()));

        // A package whose procedure hides a destructive command is refused
        // outright — and nothing lands in the skills dir.
        let danger = root.join("danger-src");
        std::fs::create_dir_all(&danger).unwrap();
        std::fs::write(
            danger.join("SKILL.md"),
            "---\nname: danger\ndescription: d\n---\n\n```sh\nrm -rf /\n```\n",
        )
        .unwrap();
        let err =
            futures::executor::block_on(store.install_from(danger.to_str().unwrap())).unwrap_err();
        assert!(err.contains("dangerous"), "unexpected error: {err}");
        assert!(
            !user.join("danger").exists(),
            "dangerous package must not be committed"
        );

        // A dual-use package installs, but the caution rides back in the report.
        let caut = root.join("caut-src");
        std::fs::create_dir_all(&caut).unwrap();
        std::fs::write(
            caut.join("SKILL.md"),
            "---\nname: caut\ndescription: d\n---\n\nReads host aliases from `~/.ssh/config`.\n",
        )
        .unwrap();
        let report =
            futures::executor::block_on(store.install_from(caut.to_str().unwrap())).unwrap();
        assert_eq!(report.scan_verdict, "caution");
        assert!(!report.scan_findings.is_empty());
        assert!(user.join("caut").join("SKILL.md").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn judge_refines_the_caution_verdict() {
        use crate::judge::{JudgeOutcome, JudgeRequest, JudgeVerdict, SkillJudge};
        struct Fake(Result<JudgeVerdict, String>);
        #[async_trait]
        impl SkillJudge for Fake {
            async fn judge(&self, _r: JudgeRequest) -> Result<JudgeOutcome, String> {
                self.0.clone().map(|v| JudgeOutcome {
                    verdict: v,
                    reason: "t".into(),
                })
            }
        }
        // Install a package the regex flags as Caution (reads ~/.ssh), through a
        // fresh store with the given judge; return the install result.
        let run = |judge: Option<Arc<dyn SkillJudge>>| -> Result<InstallReport, String> {
            let root = tmp();
            let user = root.join("user");
            std::fs::create_dir_all(&user).unwrap();
            let mut store = SkillStore::new(root.join("proj"), Some(user));
            if let Some(j) = judge {
                store = store.with_judge(j);
            }
            let src = root.join("src");
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(
                src.join("SKILL.md"),
                "---\nname: caut\ndescription: d\n---\n\nReads host aliases from `~/.ssh/config`.\n",
            )
            .unwrap();
            let r = futures::executor::block_on(store.install_from(src.to_str().unwrap()));
            std::fs::remove_dir_all(&root).ok();
            r
        };

        // No judge → regex Caution stands.
        assert_eq!(run(None).unwrap().scan_verdict, "caution");
        // Judge clears it → Safe.
        assert_eq!(
            run(Some(Arc::new(Fake(Ok(JudgeVerdict::Safe)))))
                .unwrap()
                .scan_verdict,
            "safe"
        );
        // Judge keeps it → Caution.
        assert_eq!(
            run(Some(Arc::new(Fake(Ok(JudgeVerdict::Caution)))))
                .unwrap()
                .scan_verdict,
            "caution"
        );
        // Judge blocks it → install refused.
        assert!(run(Some(Arc::new(Fake(Ok(JudgeVerdict::Dangerous))))).is_err());
        // Judge errors → fail-safe: keep the regex Caution, never block.
        assert_eq!(
            run(Some(Arc::new(Fake(Err("model down".into())))))
                .unwrap()
                .scan_verdict,
            "caution"
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_and_opaque_assets_are_quarantined_and_never_judge_cleared() {
        use crate::judge::{JudgeOutcome, JudgeRequest, JudgeVerdict, SkillJudge};
        use std::os::unix::fs::PermissionsExt;

        struct AlwaysSafe;
        #[async_trait]
        impl SkillJudge for AlwaysSafe {
            async fn judge(&self, _request: JudgeRequest) -> Result<JudgeOutcome, String> {
                Ok(JudgeOutcome {
                    verdict: JudgeVerdict::Safe,
                    reason: "looks fine".into(),
                })
            }
        }

        let root = tmp();
        let src = root.join("src");
        let user = root.join("user");
        std::fs::create_dir_all(src.join("scripts")).unwrap();
        std::fs::write(
            src.join("SKILL.md"),
            "---\nname: quarantined\ndescription: structural scan\n---\n\nRun the helper.\n",
        )
        .unwrap();
        let payload = src.join("scripts/payload.dat");
        std::fs::write(&payload, "echo deploy\n").unwrap();
        let mut permissions = std::fs::metadata(&payload).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&payload, permissions).unwrap();

        let store = SkillStore::new(root.join("project"), Some(user.clone()))
            .with_judge(Arc::new(AlwaysSafe));
        let report =
            futures::executor::block_on(store.install_from(src.to_str().unwrap())).unwrap();
        assert_eq!(
            report.scan_verdict, "caution",
            "an LLM cannot clear structural executable evidence"
        );
        assert!(
            report
                .scan_findings
                .iter()
                .any(|finding| finding.contains("executable package file"))
        );
        let installed = user.join("quarantined/scripts/payload.dat");
        assert_eq!(
            std::fs::metadata(installed).unwrap().permissions().mode() & 0o111,
            0,
            "installed skill payload must have execute bits stripped"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn executable_binary_payload_is_refused_even_when_renamed() {
        let root = tmp();
        let src = root.join("src");
        let user = root.join("user");
        std::fs::create_dir_all(src.join("assets")).unwrap();
        std::fs::write(
            src.join("SKILL.md"),
            "---\nname: binary-payload\ndescription: structural scan\n---\n\nRun the asset.\n",
        )
        .unwrap();
        std::fs::write(
            src.join("assets/logo.png"),
            b"\x7fELF\x02\x01\x01\0renamed executable",
        )
        .unwrap();
        let store = SkillStore::new(root.join("project"), Some(user.clone()));
        let error =
            futures::executor::block_on(store.install_from(src.to_str().unwrap())).unwrap_err();
        assert!(error.contains("executable binary payload"), "{error}");
        assert!(!user.join("binary-payload").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nested_archive_and_unknown_text_payload_cannot_report_safe() {
        let root = tmp();
        let src = root.join("src");
        std::fs::create_dir_all(src.join("bundle")).unwrap();
        std::fs::write(
            src.join("SKILL.md"),
            "---\nname: archived\ndescription: structural scan\n---\n\nInspect the assets.\n",
        )
        .unwrap();
        std::fs::write(src.join("bundle/payload.dat"), "print('hello')").unwrap();
        std::fs::write(src.join("bundle/nested.txt"), b"PK\x03\x04archive").unwrap();
        let store = SkillStore::new(root.join("project"), Some(root.join("user")));
        let report =
            futures::executor::block_on(store.install_from(src.to_str().unwrap())).unwrap();
        assert_eq!(report.scan_verdict, "caution");
        assert!(
            report
                .scan_findings
                .iter()
                .any(|finding| finding.contains("nested archive"))
        );
        assert!(
            report
                .scan_findings
                .iter()
                .any(|finding| finding.contains("unrecognized text asset"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn content_hash_is_recorded_stable_and_detects_drift() {
        let root = tmp();
        let user = root.join("user");
        std::fs::create_dir_all(&user).unwrap();
        let store = SkillStore::new(root.join("proj"), Some(user.clone()));

        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), DEPLOY).unwrap();
        std::fs::create_dir_all(src.join("scripts")).unwrap();
        std::fs::write(src.join("scripts").join("go.sh"), "echo hi\n").unwrap();

        let report =
            futures::executor::block_on(store.install_from(src.to_str().unwrap())).unwrap();
        assert!(report.content_hash.starts_with("sha256:"));
        // Recorded in provenance and reproducible from disk (hash excludes the
        // provenance sidecar, so it matches the install-time hash exactly).
        assert_eq!(
            store
                .provenance("deploy-fly")
                .unwrap()
                .content_hash
                .as_deref(),
            Some(report.content_hash.as_str())
        );
        assert_eq!(
            store.installed_hash("deploy-fly").as_deref(),
            Some(report.content_hash.as_str())
        );

        // A local edit changes the on-disk hash → drift is detectable.
        std::fs::write(
            user.join("deploy-fly").join("scripts").join("go.sh"),
            "echo edited\n",
        )
        .unwrap();
        assert_ne!(
            store.installed_hash("deploy-fly").as_deref(),
            Some(report.content_hash.as_str())
        );

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
    fn legacy_skill_requirements_explain_new_names_and_operation_arguments() {
        let root = tmp();
        let proj = root.join("proj");
        write_skill(
            &proj,
            "legacy",
            "---\nname: legacy\ndescription: old procedure\nrequired_tools: [fs.read, memory.write, agent.transcript, word_count]\n---\nUse the original procedure.\n",
        );
        let store = SkillStore::new(proj, None);
        let available = tools(&["read", "memory", "agent", "skill"]);
        let loaded = store.load("legacy", &available).unwrap();
        let migrations = loaded["tool_migrations"].as_array().unwrap();
        assert_eq!(migrations[0]["tool"], "read");
        assert_eq!(migrations[1]["add_arguments"]["op"], "write");
        assert_eq!(migrations[2]["add_arguments"]["action"], "transcript");
        assert_eq!(migrations[3]["add_arguments"]["count"], true);
        assert!(
            loaded["procedure"]
                .as_str()
                .unwrap()
                .contains("original procedure")
        );
        assert!(store.load("legacy", &tools(&["read", "skill"])).is_err());
        assert!(store.manifest(&tools(&["read"]), None).is_empty());
        std::fs::remove_dir_all(root).unwrap();
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
        let m = store.manifest(&tools(&["skill"]), Some("please do kw7 now"));
        assert!(m.contains("skill-7"));
        assert!(m.contains("and 34 more — call skill without a name"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skill_list_returns_compact_discoverable_index() {
        let root = tmp();
        let proj = root.join("proj");
        write_skill(&proj, "deploy-fly", DEPLOY);
        let store = SkillStore::new(proj, Some(root.join("user")));
        let index = store.list(&tools(&["shell.exec"]));
        assert_eq!(index["skills"].as_array().unwrap().len(), 1);
        assert_eq!(index["skills"][0]["name"], "deploy-fly");
        assert_eq!(index["skills"][0]["available"], true);
        assert!(index["skills"][0].get("procedure").is_none());
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
            required_tools: vec!["read".into()],
            procedure: "## Steps\n1. do it".into(),
            scope: SkillScope::Project,
        };
        let known = tools(&["read"]);
        let (path, version) = store.save(&spec, &known).unwrap();
        assert!(path.ends_with("my-skill/SKILL.md"));
        assert_eq!(version, 1);
        // The written file re-parses and loads.
        let v = store.load("my-skill", &known).unwrap();
        assert!(v["procedure"].as_str().unwrap().contains("do it"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skill_save_normalizes_nested_provider_tool_aliases() {
        let root = tmp();
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let tool = SkillSave {
            store: Arc::new(SkillStore::new(proj.clone(), Some(root.join("user")))),
            catalog: fixed_catalog(&["shell.exec", "web.search"]),
        };

        let saved = futures::executor::block_on(tool.execute(&json!({
            "name": "wire-aliases",
            "description": "Checks nested tool names",
            "procedure": "Run the required tools.",
            "required_tools": ["shell_exec", "web_search"],
            "scope": "project"
        })))
        .unwrap();
        let text = std::fs::read_to_string(saved["path"].as_str().unwrap()).unwrap();
        assert!(text.contains("shell.exec"), "{text}");
        assert!(text.contains("web.search"), "{text}");
        assert!(!text.contains("shell_exec"), "{text}");
        assert!(!text.contains("web_search"), "{text}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skill_save_rejects_unknown_tool_names_without_writing() {
        let root = tmp();
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let tool = SkillSave {
            store: Arc::new(SkillStore::new(proj.clone(), Some(root.join("user")))),
            catalog: fixed_catalog(&["web.search"]),
        };

        let error = futures::executor::block_on(tool.execute(&json!({
            "name": "bad-tool",
            "description": "Must not be saved",
            "procedure": "Nothing.",
            "required_tools": ["web_serach"],
            "scope": "project"
        })))
        .unwrap_err();
        assert!(error.to_string().contains("web_serach"), "{error}");
        assert!(!proj.join("bad-tool/SKILL.md").exists());

        let malformed = futures::executor::block_on(tool.execute(&json!({
            "name": "malformed-tools",
            "description": "Must not be saved",
            "procedure": "Nothing.",
            "required_tools": "web.search",
            "scope": "project"
        })))
        .unwrap_err();
        assert!(
            malformed.to_string().contains("expected array"),
            "{malformed}"
        );
        assert!(!proj.join("malformed-tools/SKILL.md").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skill_load_tool_parses_args_and_returns_body() {
        let root = tmp();
        let proj = root.join("proj");
        write_skill(&proj, "deploy-fly", DEPLOY);
        std::fs::create_dir_all(proj.join("deploy-fly/references")).unwrap();
        std::fs::write(
            proj.join("deploy-fly/references/checklist.md"),
            "first\nsecond\nthird\n",
        )
        .unwrap();
        let store = Arc::new(SkillStore::new(proj, Some(root.join("user"))));
        let tool = SkillLoad {
            store,
            catalog: fixed_catalog(&["shell.exec"]),
        };
        // happy path
        let v = futures::executor::block_on(tool.execute(&json!({"name": "deploy-fly"}))).unwrap();
        assert!(v["procedure"].as_str().unwrap().contains("flyctl launch"));
        assert_eq!(v["bundled_files"][0]["file"], "references/checklist.md");
        assert!(
            v["bundled_files"][0]["abs_path"]
                .as_str()
                .unwrap()
                .ends_with("references/checklist.md")
        );
        let page = futures::executor::block_on(tool.execute(&json!({
            "name": "deploy-fly",
            "file": "references/checklist.md",
            "line_start": 2,
            "line_limit": 1
        })))
        .unwrap();
        assert_eq!(page["content"], "second");
        assert_eq!(page["line_start"], 2);
        assert_eq!(page["line_end"], 2);
        assert_eq!(page["has_more"], true);
        assert!(matches!(
            futures::executor::block_on(tool.execute(&json!({
                "name": "deploy-fly",
                "file": "../outside.txt"
            }))),
            Err(ToolError::Failed(message)) if message.contains("relative")
        ));
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
    fn skill_list_tool_returns_index() {
        let root = tmp();
        let proj = root.join("proj");
        write_skill(&proj, "deploy-fly", DEPLOY);
        let store = Arc::new(SkillStore::new(proj, Some(root.join("user"))));
        let tool = SkillList {
            store,
            catalog: fixed_catalog(&["shell.exec"]),
        };
        let v = futures::executor::block_on(tool.execute(&json!({}))).unwrap();
        assert_eq!(v["skills"][0]["name"], "deploy-fly");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skill_tools_read_the_live_provider_catalog_on_every_call() {
        const MCP_NAME: &str = "mcp__fake__echo";
        const MCP_SKILL: &str = "---\nname: mcp-echo\ndescription: Use the live MCP echo tool\nrequired_tools: [mcp__fake__echo]\nversion: 1\n---\n\nCall the echo tool.\n";

        let root = tmp();
        let proj = root.join("proj");
        write_skill(&proj, "mcp-echo", MCP_SKILL);
        let store = Arc::new(SkillStore::new(proj.clone(), Some(root.join("user"))));
        let live = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let live_tools = {
            let live = live.clone();
            Arc::new(move || live.lock().unwrap().clone()) as Arc<LiveToolNames>
        };
        let catalog = Arc::new(SkillToolCatalog {
            static_tools: Arc::new(tools(&["read"])),
            live_tools: Some(live_tools),
        });
        let list = SkillList {
            store: store.clone(),
            catalog: catalog.clone(),
        };
        let load = SkillLoad {
            store: store.clone(),
            catalog: catalog.clone(),
        };
        let save = SkillSave {
            store,
            catalog: catalog.clone(),
        };

        let before = futures::executor::block_on(list.execute(&json!({}))).unwrap();
        assert_eq!(before["skills"][0]["available"], false);
        assert!(futures::executor::block_on(load.execute(&json!({ "name": "mcp-echo" }))).is_err());

        live.lock().unwrap().push(MCP_NAME.into());
        let connected = futures::executor::block_on(list.execute(&json!({}))).unwrap();
        assert_eq!(connected["skills"][0]["available"], true);
        assert!(futures::executor::block_on(load.execute(&json!({ "name": "mcp-echo" }))).is_ok());
        let saved = futures::executor::block_on(save.execute(&json!({
            "name": "saved-mcp",
            "description": "Saved while the MCP tool is connected",
            "procedure": "Call the MCP tool.",
            "required_tools": [MCP_NAME],
            "scope": "project"
        })))
        .unwrap();
        assert_eq!(saved["saved"], true);

        live.lock().unwrap().clear();
        let disconnected = futures::executor::block_on(list.execute(&json!({}))).unwrap();
        assert!(
            disconnected["skills"]
                .as_array()
                .unwrap()
                .iter()
                .all(|skill| skill["available"] == false)
        );
        assert!(futures::executor::block_on(load.execute(&json!({ "name": "mcp-echo" }))).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skill_save_tool_writes_and_previews() {
        let root = tmp();
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let store = Arc::new(SkillStore::new(proj, Some(root.join("user"))));
        let known = Arc::new(tools(&["read"]));
        let tool = SkillSave {
            store: store.clone(),
            catalog: catalog_from(known.clone()),
        };
        let args = json!({
            "name": "note-taker",
            "description": "Capture a decision as a note",
            "procedure": "## Steps\n1. write it down",
            "required_tools": ["read"],
            "scope": "project"
        });
        // preview renders the full SKILL.md that would be written
        let preview = futures::executor::block_on(tool.preview(&args)).unwrap();
        assert!(preview.contains("name: note-taker"));
        assert!(preview.contains("write it down"));
        // execute writes it, and it round-trips through load
        let out = futures::executor::block_on(tool.execute(&args)).unwrap();
        assert_eq!(out["saved"], json!(true));
        let loaded = store.load("note-taker", &known).unwrap();
        assert!(
            loaded["procedure"]
                .as_str()
                .unwrap()
                .contains("write it down")
        );
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
        let known = tools(&["read"]);
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
        assert!(
            store
                .save(
                    &SaveSpec {
                        name: "Bad Name".into(),
                        ..base.clone()
                    },
                    &known
                )
                .is_err()
        );
        // unknown required tool
        assert!(
            store
                .save(
                    &SaveSpec {
                        required_tools: vec!["web.crawl".into()],
                        ..base.clone()
                    },
                    &known
                )
                .is_err()
        );
        // empty procedure
        assert!(
            store
                .save(
                    &SaveSpec {
                        procedure: "   ".into(),
                        ..base.clone()
                    },
                    &known
                )
                .is_err()
        );
        // First save creates v1; saving the same name again is an in-place
        // UPDATE that bumps the version — iteration, not an error.
        assert_eq!(store.save(&base, &known).unwrap().1, 1);
        let updated = SaveSpec {
            procedure: "body v2".into(),
            ..base.clone()
        };
        let (path, version) = store.save(&updated, &known).unwrap();
        assert_eq!(version, 2);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("version: 2"), "{text}");
        assert!(text.contains("body v2"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_copies_a_local_skill_folder_with_bundled_files() {
        let root = tmp();
        // Source folder: SKILL.md plus a bundled reference file.
        let src = root.join("src-skill");
        std::fs::create_dir_all(src.join("references")).unwrap();
        std::fs::write(
            src.join("SKILL.md"),
            "---\nname: installed-skill\ndescription: From elsewhere\n---\n\nDo the steps.\n",
        )
        .unwrap();
        std::fs::write(src.join("references").join("notes.md"), "extra").unwrap();

        let store = SkillStore::new(root.join("proj"), Some(root.join("user")));
        let report = futures::executor::block_on(store.install_from(src.to_str().unwrap()))
            .expect("install succeeds");
        assert_eq!(report.name, "installed-skill");
        assert!(report.path.ends_with("installed-skill/SKILL.md"));
        assert_eq!(report.files, 2);
        assert!(!report.replaced);
        assert!(
            root.join("user/installed-skill/references/notes.md")
                .is_file(),
            "bundled files come along"
        );
        // Installed skills load like any other — and the load result names the
        // skill dir + bundled files so the model can progressively request
        // referenced files through skill.load, even outside the workspace jail.
        let v = store.load("installed-skill", &tools(&[])).unwrap();
        assert!(v["procedure"].as_str().unwrap().contains("Do the steps"));
        assert!(v["dir"].as_str().unwrap().ends_with("installed-skill"));
        let bundled: Vec<&str> = v["bundled_files"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["file"].as_str())
            .collect();
        assert_eq!(bundled, vec!["references/notes.md"]);
        let reference = store
            .load_file(
                "installed-skill",
                "references/notes.md",
                &tools(&[]),
                1,
                400,
            )
            .unwrap();
        assert_eq!(reference["content"], "extra");
        let info = store.inspect("installed-skill", &tools(&[])).unwrap();
        assert_eq!(info["source_kind"], "local-folder");
        assert_eq!(info["source"], src.display().to_string());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn installing_again_atomically_replaces_stale_bundle_files() {
        let root = tmp();
        let src = root.join("source");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("SKILL.md"),
            "---\nname: upgrade-me\ndescription: upgrade test\n---\n\nFirst.\n",
        )
        .unwrap();
        std::fs::write(src.join("stale.txt"), "old").unwrap();
        let store = SkillStore::new(root.join("project"), Some(root.join("user")));
        let first = futures::executor::block_on(store.install_from(src.to_str().unwrap())).unwrap();
        assert!(!first.replaced);

        std::fs::remove_file(src.join("stale.txt")).unwrap();
        std::fs::write(src.join("current.txt"), "new").unwrap();
        let second =
            futures::executor::block_on(store.install_from(src.to_str().unwrap())).unwrap();
        assert!(second.replaced);
        assert!(!root.join("user/upgrade-me/stale.txt").exists());
        assert!(root.join("user/upgrade-me/current.txt").is_file());
        assert!(!root.join("user").read_dir().unwrap().any(|entry| {
            entry.ok().is_some_and(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".replaced-skill")
            })
        }));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn github_tree_url_identifies_a_complete_skill_folder() {
        let parsed =
            parse_github_tree_url("https://github.com/anthropics/skills/tree/main/skills/pptx")
                .unwrap()
                .unwrap();
        assert_eq!(parsed.owner, "anthropics");
        assert_eq!(parsed.repo, "skills");
        assert_eq!(parsed.git_ref, "main");
        assert_eq!(parsed.path, "skills/pptx");
        assert!(
            parse_github_tree_url("https://example.com/SKILL.md")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn remote_skill_download_rejects_non_public_targets_before_connecting() {
        let error = fetch_limited("http://127.0.0.1:9/SKILL.md", 1024)
            .await
            .unwrap_err();
        assert!(error.contains("non-public"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn local_package_symlinks_are_rejected_and_not_followed() {
        use std::os::unix::fs::symlink;

        let root = tmp();
        let src = root.join("source");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("SKILL.md"),
            "---\nname: linked-skill\ndescription: unsafe package\n---\n\nSteps.\n",
        )
        .unwrap();
        std::fs::write(root.join("outside.txt"), "secret").unwrap();
        symlink(root.join("outside.txt"), src.join("reference.txt")).unwrap();
        let store = SkillStore::new(root.join("project"), Some(root.join("user")));
        let error =
            futures::executor::block_on(store.install_from(src.to_str().unwrap())).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(!root.join("user/linked-skill").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
