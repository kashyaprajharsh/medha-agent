//! Skills — Phase A: the *consumption* side of spec §4.11 plus user/agent
//! authoring with a human approval gate. A skill is a folder with a `SKILL.md`
//! in the ecosystem-standard shape: YAML frontmatter + a markdown procedure
//! body. Skills written for other agent harnesses drop in unchanged — unknown
//! frontmatter keys (`license`, …) are tolerated, and legacy TOML frontmatter
//! still parses as a fallback.
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

/// Where an installed user skill came from. Stored beside the package so `/skill
/// info` can explain provenance and future update support has a stable base.
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

/// The frontmatter of a `SKILL.md` (YAML; legacy TOML still reads). Only
/// `name` and `description` are required — the rest are optional extensions,
/// skipped on write when empty so saved skills stay portable to other
/// harnesses. Unknown keys (`license`, `allowed-tools`, …) parse fine and are
/// simply not carried.
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
        Self {
            project_dir,
            user_dir,
        }
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
                            .filter(|t| !known_tools.contains(*t))
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
        if !files.is_empty() {
            // Give each bundled file BOTH its relative name (pass to skill.load
            // `file=` to READ text) and its absolute `abs_path` (use with
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
                 skill.load with `name`+`file`; to RUN a script, pass its `abs_path` to shell.exec \
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
        validate_name(name)?;
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
            "file": relative.display().to_string(),
            "content": page.join("\n"),
            "line_start": start,
            "line_end": end,
            "total_lines": total_lines,
            "has_more": end < total_lines,
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
             matches the request, skill.load it and follow it before doing the task your own way.\n",
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
                "- … and {hidden} more — call skill.list to browse them\n"
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

    /// Validate and write a skill. Saving over an existing name in the same
    /// scope is an UPDATE: the version bumps and the approval card previews a
    /// diff, so iterating on a skill is first-class rather than "go edit the
    /// file". Writes directly: the approval card (skill.save is on the policy
    /// approve list) already showed the change, so a second permission prompt
    /// would be redundant. Returns the written path and the version written.
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
        let unknown: Vec<&String> = spec
            .required_tools
            .iter()
            .filter(|t| !known_tools.contains(*t))
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
        };
        let target = dir.join(&spec.name).join("SKILL.md");
        let text = std::fs::read_to_string(&target).ok()?;
        Some((target, text))
    }

    /// Describe an effective skill without loading its procedure into model
    /// context. Unlike `load`, this also works for skills whose required tools
    /// are unavailable, making it suitable for `/skill info` diagnostics.
    pub fn inspect(&self, name: &str, known_tools: &HashSet<String>) -> Result<Value, String> {
        validate_name(name)?;
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

    /// Install a complete skill package into the user scope. Accepts a GitHub
    /// `/tree/<ref>/<path>` folder URL, a raw `SKILL.md` URL, a local directory,
    /// or a local `SKILL.md`. Folder sources retain scripts/references/assets.
    ///
    /// The skill is validated before anything is written; the name comes from
    /// its frontmatter (kebab-validated, so it can't escape the skills dir).
    /// Installing over an existing name replaces it (that's an upgrade).
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
                let client = install_client()?;
                let body = fetch_limited(&client, src, MAX_SKILL_MD_BYTES).await?;
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
            // Hash the package now — before the provenance sidecar is written —
            // so the hash covers only real content and never itself.
            let content_hash = hash_package(&stage);
            let provenance = SkillProvenance {
                source: src.to_string(),
                kind,
                revision: revision.clone(),
                installed_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                content_hash: Some(content_hash.clone()),
            };
            let provenance_json = serde_json::to_vec_pretty(&provenance)
                .map_err(|e| format!("serializing skill provenance: {e}"))?;
            std::fs::write(stage.join(PROVENANCE_FILE), provenance_json)
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((fm.name, revision, content_hash, budget))
        }
        .await;

        let (name, revision, content_hash, budget) = match staged {
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
        let scan = scan_staged(&stage);
        let scan_findings = format_findings(&scan.findings);
        if scan.verdict == policy::guard::ScanVerdict::Dangerous {
            std::fs::remove_dir_all(&stage).ok();
            return Err(format!(
                "refusing to install '{name}': the package contains dangerous content:\n  {}",
                scan_findings.join("\n  ")
            ));
        }
        let scan_verdict = if scan.verdict == policy::guard::ScanVerdict::Caution {
            "caution"
        } else {
            "safe"
        };
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

/// Version a save should write: one past whatever is on disk (1 for new or
/// unparseable — overwriting a broken file restarts its history honestly).
fn next_version(target: &Path) -> u32 {
    std::fs::read_to_string(target)
        .ok()
        .and_then(|text| parse_skill_md(&text).ok())
        .map(|(fm, _)| fm.version.saturating_add(1))
        .unwrap_or(1)
}

/// Files shipped alongside `SKILL.md` (references, scripts, assets), as paths
/// relative to the skill dir — sorted, hidden files skipped, capped so a huge
/// folder can't flood the tool result. Contents are never read here: the
/// model pulls specific files on demand (progressive disclosure).
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
        } else if ty.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                let rel = rel.display().to_string();
                if rel != "SKILL.md" {
                    out.push(rel);
                }
            }
        }
    }
}

/// Screen every file in a staged package with the Skills Guard before it is
/// committed. `collect_files` omits `SKILL.md` (it lists *bundled* extras), so
/// it is added back explicitly — the procedure body is the first thing to scan.
/// Binary files are read but skipped by the guard (non-UTF-8 → inert).
fn scan_staged(stage: &Path) -> policy::guard::ScanReport {
    let mut rels = Vec::new();
    collect_files(stage, stage, &mut rels);
    rels.push("SKILL.md".to_string());
    let files: Vec<(String, Vec<u8>)> = rels
        .into_iter()
        .filter_map(|rel| std::fs::read(stage.join(&rel)).ok().map(|b| (rel, b)))
        .collect();
    policy::guard::scan_package(files.iter().map(|(p, b)| (p.as_str(), b.as_slice())))
}

/// Deterministic content hash of a package directory (`sha256:<hex>`). Every
/// file — including `SKILL.md`, excluding the `.`-prefixed provenance sidecar
/// (`collect_files` skips dotfiles) — contributes its relative path and bytes,
/// length-prefixed and in sorted order, so the same package always hashes
/// identically regardless of enumeration order. Powers update drift-detection
/// and the skills lockfile.
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

fn install_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(INSTALL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("medha-skills/1")
        .build()
        .map_err(|e| e.to_string())
}

async fn fetch_limited(client: &reqwest::Client, url: &str, cap: usize) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("fetching {url}: {e}"))?;
    if response.content_length().is_some_and(|n| n > cap as u64) {
        return Err(format!("{url} exceeds the {cap}-byte download limit"));
    }
    let mut out = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("reading {url}: {e}"))?;
        if out.len().saturating_add(chunk.len()) > cap {
            return Err(format!("{url} exceeds the {cap}-byte download limit"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

async fn github_revision(client: &reqwest::Client, tree: &GitHubTree) -> String {
    let url = format!(
        "https://api.github.com/repos/{}/{}/commits/{}",
        urlencoding::encode(&tree.owner),
        urlencoding::encode(&tree.repo),
        urlencoding::encode(&tree.git_ref),
    );
    fetch_limited(client, &url, 512 * 1024)
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

    let client = install_client()?;
    let revision = github_revision(&client, tree).await;
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
        let listing = fetch_limited(&client, &api, 2 * 1024 * 1024).await?;
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
                    let bytes = fetch_limited(&client, download, MAX_INSTALL_FILE_BYTES).await?;
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

/// Split a `SKILL.md` into its frontmatter and markdown body. The file must
/// open with a `---` fence, contain a closing `---` fence, and the frontmatter
/// must carry a non-empty `name` and `description`. YAML is the standard
/// format (skills authored for any other harness drop in unchanged); TOML is
/// accepted as a read-only legacy fallback for skills saved by older builds.
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

// ---- Tools ---------------------------------------------------------------

/// `skill.load` — read radius. The model pulls a full procedure by name.
pub struct SkillLoad {
    pub store: Arc<SkillStore>,
    pub known_tools: Arc<HashSet<String>>,
}

/// `skill.list` — compact index for large catalogs, separate from `skill.load`
/// so a model can discover names before loading a full procedure.
pub struct SkillList {
    pub store: Arc<SkillStore>,
    pub known_tools: Arc<HashSet<String>>,
}

#[async_trait]
impl Tool for SkillList {
    fn name(&self) -> &str {
        "skill.list"
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
        Ok(self.store.list(&self.known_tools))
    }
}

#[async_trait]
impl Tool for SkillLoad {
    fn name(&self) -> &str {
        "skill.load"
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
                    &self.known_tools,
                    line_start as usize,
                    line_limit as usize,
                )
                .map_err(ToolError::Failed)
        } else {
            self.store
                .load(name, &self.known_tools)
                .map_err(ToolError::Failed)
        }
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
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
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
                "description": { "type": "string", "description": "what the skill does and when to use it (≤1024 chars) — shown in the skills list" },
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
        let spec = Self::spec_from(args)?;
        let (path, version) = self
            .store
            .save(&spec, &self.known_tools)
            .map_err(ToolError::Failed)?;
        Ok(json!({
            "saved": true,
            "version": version,
            "updated": version > 1,
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
        let known = tools(&["fs.read"]); // shell.exec missing
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
        let err = futures::executor::block_on(store.install_from(danger.to_str().unwrap()))
            .unwrap_err();
        assert!(err.contains("dangerous"), "unexpected error: {err}");
        assert!(!user.join("danger").exists(), "dangerous package must not be committed");

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

        let report = futures::executor::block_on(store.install_from(src.to_str().unwrap())).unwrap();
        assert!(report.content_hash.starts_with("sha256:"));
        // Recorded in provenance and reproducible from disk (hash excludes the
        // provenance sidecar, so it matches the install-time hash exactly).
        assert_eq!(store.provenance("deploy-fly").unwrap().content_hash.as_deref(), Some(report.content_hash.as_str()));
        assert_eq!(store.installed_hash("deploy-fly").as_deref(), Some(report.content_hash.as_str()));

        // A local edit changes the on-disk hash → drift is detectable.
        std::fs::write(user.join("deploy-fly").join("scripts").join("go.sh"), "echo edited\n").unwrap();
        assert_ne!(store.installed_hash("deploy-fly").as_deref(), Some(report.content_hash.as_str()));

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
        assert!(m.contains("and 34 more — call skill.list"));
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
            required_tools: vec!["fs.read".into()],
            procedure: "## Steps\n1. do it".into(),
            scope: SkillScope::Project,
        };
        let known = tools(&["fs.read"]);
        let (path, version) = store.save(&spec, &known).unwrap();
        assert!(path.ends_with("my-skill/SKILL.md"));
        assert_eq!(version, 1);
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
        std::fs::create_dir_all(proj.join("deploy-fly/references")).unwrap();
        std::fs::write(
            proj.join("deploy-fly/references/checklist.md"),
            "first\nsecond\nthird\n",
        )
        .unwrap();
        let store = Arc::new(SkillStore::new(proj, Some(root.join("user"))));
        let known = Arc::new(tools(&["shell.exec"]));
        let tool = SkillLoad {
            store,
            known_tools: known,
        };
        // happy path
        let v = futures::executor::block_on(tool.execute(&json!({"name": "deploy-fly"}))).unwrap();
        assert!(v["procedure"].as_str().unwrap().contains("flyctl launch"));
        assert_eq!(v["bundled_files"][0]["file"], "references/checklist.md");
        assert!(v["bundled_files"][0]["abs_path"].as_str().unwrap().ends_with("references/checklist.md"));
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
            known_tools: Arc::new(tools(&["shell.exec"])),
        };
        let v = futures::executor::block_on(tool.execute(&json!({}))).unwrap();
        assert_eq!(v["skills"][0]["name"], "deploy-fly");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skill_save_tool_writes_and_previews() {
        let root = tmp();
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let store = Arc::new(SkillStore::new(proj, Some(root.join("user"))));
        let known = Arc::new(tools(&["fs.read"]));
        let tool = SkillSave {
            store: store.clone(),
            known_tools: known.clone(),
        };
        let args = json!({
            "name": "note-taker",
            "description": "Capture a decision as a note",
            "procedure": "## Steps\n1. write it down",
            "required_tools": ["fs.read"],
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
