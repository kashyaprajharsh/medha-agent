//! Skill sources ("taps") — remembered GitHub repositories to search and
//! install skills from, so a source is registered once instead of pasted as a
//! full URL every time. Persisted as TOML in the user's medha home. The hub
//! reuses the guard-gated installer in [`crate::skills`] for anything it fetches.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The default subdirectory a tap's skill folders live under, matching the
/// prevailing `repo/skills/<name>/SKILL.md` convention across the ecosystem.
pub const DEFAULT_TAP_PATH: &str = "skills";

fn default_tap_path() -> String {
    DEFAULT_TAP_PATH.to_string()
}

/// One registered source: a GitHub repo plus the subpath its skills live under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tap {
    /// `owner/repo`.
    pub repo: String,
    /// Subdirectory that holds the skill folders (default [`DEFAULT_TAP_PATH`]).
    #[serde(default = "default_tap_path")]
    pub path: String,
    /// Git ref (branch/tag/sha) to resolve against; `None` = the repo default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}

impl Tap {
    /// Parse a `sources add` spec: `owner/repo[/sub/path][@ref]`, with an
    /// optional explicit path argument that overrides an inline subpath.
    /// Rejects anything that isn't a well-formed `owner/repo`.
    pub fn parse(spec: &str, path_arg: Option<&str>) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("expected owner/repo".into());
        }
        let (spec, git_ref) = match spec.split_once('@') {
            Some((s, r)) if !r.trim().is_empty() => (s.trim(), Some(r.trim().to_string())),
            _ => (spec, None),
        };
        let mut segs = spec.split('/').filter(|s| !s.is_empty());
        let owner = segs.next().ok_or("expected owner/repo")?;
        let repo = segs.next().ok_or("expected owner/repo (missing repo)")?;
        let inline_path = segs.collect::<Vec<_>>().join("/");
        if !is_github_segment(owner) || !is_github_segment(repo.trim_end_matches(".git")) {
            return Err(format!("'{owner}/{repo}' is not a valid owner/repo"));
        }
        let path = match path_arg.map(str::trim).filter(|s| !s.is_empty()) {
            Some(p) => p.trim_matches('/').to_string(),
            None if !inline_path.is_empty() => inline_path,
            None => DEFAULT_TAP_PATH.to_string(),
        };
        Ok(Tap {
            repo: format!("{owner}/{}", repo.trim_end_matches(".git")),
            path: if path.is_empty() { DEFAULT_TAP_PATH.to_string() } else { path },
            git_ref,
        })
    }

    /// Stable identity for dedup/removal: a source is the repo + subpath (the
    /// same repo may be tapped twice at different paths).
    pub fn key(&self) -> String {
        format!("{}/{}", self.repo, self.path)
    }
}

/// GitHub owner/repo segment: letters, digits, `-`, `_`, `.` — never a path
/// traversal or a slashless-but-empty token.
fn is_github_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// On-disk `taps.toml` shape.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TapsFile {
    #[serde(default, rename = "tap")]
    taps: Vec<Tap>,
}

/// The persisted set of registered sources. Backed by a single TOML file;
/// reads tolerate its absence (no sources yet) and a corrupt file surfaces as
/// an error rather than silently dropping a user's sources.
pub struct TapStore {
    path: PathBuf,
}

impl TapStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// All registered taps in insertion order. Empty when the file is absent.
    pub fn list(&self) -> Result<Vec<Tap>, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("reading {}: {e}", self.path.display())),
        };
        let parsed: TapsFile = toml::from_str(&text)
            .map_err(|e| format!("{} is malformed: {e}", self.path.display()))?;
        Ok(parsed.taps)
    }

    /// Register a tap. Idempotent by [`Tap::key`]: re-adding the same repo+path
    /// updates its ref rather than duplicating. Returns `true` if it was new.
    pub fn add(&self, tap: Tap) -> Result<bool, String> {
        let mut taps = self.list()?;
        let key = tap.key();
        let is_new = if let Some(existing) = taps.iter_mut().find(|t| t.key() == key) {
            *existing = tap;
            false
        } else {
            taps.push(tap);
            true
        };
        self.write(&taps)?;
        Ok(is_new)
    }

    /// Remove every tap matching `repo` (any subpath) or an exact `repo/path`
    /// key. Returns the number removed.
    pub fn remove(&self, spec: &str) -> Result<usize, String> {
        let spec = spec.trim().trim_matches('/');
        let mut taps = self.list()?;
        let before = taps.len();
        taps.retain(|t| t.repo != spec && t.key() != spec);
        let removed = before - taps.len();
        if removed > 0 {
            self.write(&taps)?;
        }
        Ok(removed)
    }

    fn write(&self, taps: &[Tap]) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let body = toml::to_string_pretty(&TapsFile { taps: taps.to_vec() })
            .map_err(|e| format!("serializing taps: {e}"))?;
        atomic_write(&self.path, body.as_bytes())
    }
}

/// Write-through a temp file + rename so a crash never leaves a half-written
/// sources file (a corrupt one would lose every registered source).
fn atomic_write(target: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let parent = target.parent().ok_or("taps file has no parent")?;
    let tmp = parent.join(format!(".taps-{}.tmp", std::process::id()));
    let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, target).map_err(|e| {
        std::fs::remove_file(&tmp).ok();
        e.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("medha-taps-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parses_repo_forms() {
        assert_eq!(
            Tap::parse("anthropics/skills", None).unwrap(),
            Tap { repo: "anthropics/skills".into(), path: "skills".into(), git_ref: None }
        );
        // inline subpath
        let t = Tap::parse("myorg/tools/internal/skills", None).unwrap();
        assert_eq!(t.repo, "myorg/tools");
        assert_eq!(t.path, "internal/skills");
        // explicit path arg overrides inline, and @ref is captured
        let t = Tap::parse("myorg/tools@dev", Some("catalog")).unwrap();
        assert_eq!(t.path, "catalog");
        assert_eq!(t.git_ref.as_deref(), Some("dev"));
        // trailing .git is trimmed
        assert_eq!(Tap::parse("o/r.git", None).unwrap().repo, "o/r");
    }

    #[test]
    fn rejects_malformed_specs() {
        for bad in ["", "no-slash", "bad!/repo", "../etc/passwd", "owner/"] {
            assert!(Tap::parse(bad, None).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn add_list_remove_round_trip_and_persist() {
        let dir = tmp();
        let store = TapStore::new(dir.join("taps.toml"));
        assert!(store.list().unwrap().is_empty()); // absent file is empty, not an error

        assert!(store.add(Tap::parse("anthropics/skills", None).unwrap()).unwrap());
        assert!(store.add(Tap::parse("myorg/tools", Some("catalog")).unwrap()).unwrap());
        assert_eq!(store.list().unwrap().len(), 2);

        // A fresh store reads the same file back (persistence).
        let reopened = TapStore::new(dir.join("taps.toml"));
        assert_eq!(reopened.list().unwrap().len(), 2);

        // Re-adding same repo+path is an update, not a duplicate.
        assert!(!store.add(Tap::parse("anthropics/skills@v2", None).unwrap()).unwrap());
        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(
            store.list().unwrap().iter().find(|t| t.repo == "anthropics/skills").unwrap().git_ref.as_deref(),
            Some("v2")
        );

        assert_eq!(store.remove("anthropics/skills").unwrap(), 1);
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(store.remove("nope/nope").unwrap(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_file_is_an_error_not_silent_loss() {
        let dir = tmp();
        let path = dir.join("taps.toml");
        std::fs::write(&path, "this is not valid toml : : :").unwrap();
        assert!(TapStore::new(path).list().is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
