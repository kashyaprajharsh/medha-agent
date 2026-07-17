//! Skill sources ("taps") — remembered GitHub repositories to search and
//! install skills from, so a source is registered once instead of pasted as a
//! full URL every time. Persisted as TOML in the user's medha home. The hub
//! reuses the guard-gated installer in [`crate::skills`] for anything it fetches.

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// SKILL.md fetches per tap to run concurrently. Bounds outbound requests so a
/// large catalog can't open hundreds of sockets at once.
const SEARCH_CONCURRENCY: usize = 8;
/// Cap on skill folders inspected per tap, so one huge repo can't dominate a
/// search (and to keep the request count bounded against rate limits).
const MAX_SKILLS_PER_TAP: usize = 200;

/// The default subdirectory a tap's skill folders live under, matching the
/// prevailing `repo/skills/<name>/SKILL.md` convention across the ecosystem.
pub const DEFAULT_TAP_PATH: &str = "skills";

fn default_tap_path() -> String {
    DEFAULT_TAP_PATH.to_string()
}

/// Sources shipped enabled out of the box, so browse/search works with zero
/// setup — a user never has to register a source to find common skills (mirrors
/// how mature agents ship an official catalog on). Merged with the user's own
/// taps for browse/search; a user can still add more or shadow these.
pub fn default_taps() -> Vec<Tap> {
    vec![Tap {
        repo: "anthropics/skills".into(),
        path: DEFAULT_TAP_PATH.into(),
        git_ref: None,
    }]
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

// ── search ────────────────────────────────────────────────────────────────

/// One skill found in a source during search — metadata only (progressive
/// disclosure at the registry level). The full package is pulled, scanned, and
/// approved only on install via `install_url`.
#[derive(Debug, Clone)]
pub struct SkillHit {
    pub name: String,
    pub description: String,
    pub version: u32,
    /// `owner/repo` the hit came from.
    pub repo: String,
    /// GitHub `/tree/` URL that installs this exact skill folder.
    pub install_url: String,
    /// Query-match rank (higher = better); name matches outrank description.
    score: u8,
}

/// Outcome of a search: ranked hits plus any per-source errors, so a
/// rate-limited or unreachable source is reported rather than silently dropped.
#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub hits: Vec<SkillHit>,
    pub errors: Vec<String>,
}

/// Search every registered source for skills whose name or description matches
/// `query` (empty query lists everything). Metadata only — nothing is
/// downloaded beyond each candidate's `SKILL.md`. One failing source does not
/// abort the others.
pub async fn search(taps: &[Tap], query: &str) -> Result<SearchResults, String> {
    let client = crate::skills::install_client()?;
    let q = query.trim().to_lowercase();
    let mut out = SearchResults::default();
    for tap in taps {
        match search_tap(&client, tap, &q).await {
            Ok(mut hits) => out.hits.append(&mut hits),
            Err(e) => out.errors.push(format!("{}: {e}", tap.key())),
        }
    }
    out.hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

async fn search_tap(client: &reqwest::Client, tap: &Tap, q: &str) -> Result<Vec<SkillHit>, String> {
    // 1) List the tap's skills directory — one API call.
    let ref_qs = tap
        .git_ref
        .as_deref()
        .map(|r| format!("?ref={}", urlencoding::encode(r)))
        .unwrap_or_default();
    let api = format!(
        "https://api.github.com/repos/{}/contents/{}{ref_qs}",
        tap.repo,
        encode_path(&tap.path),
    );
    let body = crate::skills::fetch_limited(client, &api, 4 * 1024 * 1024).await?;
    let entries: Value = serde_json::from_slice(&body)
        .map_err(|e| format!("unexpected listing for {}: {e}", tap.path))?;
    let dirs: Vec<String> = entries
        .as_array()
        .ok_or("source path is not a directory")?
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("dir"))
        .filter_map(|e| e.get("name").and_then(Value::as_str).map(str::to_string))
        .take(MAX_SKILLS_PER_TAP)
        .collect();

    // 2) Fetch each folder's SKILL.md via raw.githubusercontent (not subject to
    //    the API rate limit), bounded-concurrently, keeping only query matches.
    let hits = futures::stream::iter(dirs)
        .map(|dir| {
            let client = client.clone();
            let (repo, path, git_ref) = (tap.repo.clone(), tap.path.clone(), tap.git_ref.clone());
            async move { fetch_hit(&client, &repo, &path, &dir, git_ref.as_deref()).await }
        })
        .buffer_unordered(SEARCH_CONCURRENCY)
        .filter_map(|hit| async move { hit.and_then(|h| score(h, q)) })
        .collect::<Vec<SkillHit>>()
        .await;
    Ok(hits)
}

/// Fetch and parse one folder's `SKILL.md`. `None` (skipped, not an error) when
/// a subdirectory has no valid SKILL.md — a repo folder need not be a skill.
async fn fetch_hit(
    client: &reqwest::Client,
    repo: &str,
    path: &str,
    dir: &str,
    git_ref: Option<&str>,
) -> Option<SkillHit> {
    let r = git_ref.unwrap_or("HEAD");
    let raw = format!(
        "https://raw.githubusercontent.com/{repo}/{}/{}/{}/SKILL.md",
        urlencoding::encode(r),
        encode_path(path),
        urlencoding::encode(dir),
    );
    let bytes = crate::skills::fetch_limited(client, &raw, 128 * 1024).await.ok()?;
    let (name, description, version) = crate::skills::skill_meta(std::str::from_utf8(&bytes).ok()?).ok()?;
    // Browser-style tree URL (unencoded segments); the installer re-resolves it.
    let install_url = format!("https://github.com/{repo}/tree/{r}/{path}/{dir}");
    Some(SkillHit { name, description, version, repo: repo.to_string(), install_url, score: 0 })
}

/// Attach a match score, or drop the hit when the (non-empty) query matches
/// neither name nor description. Exact name > name substring > description.
fn score(mut hit: SkillHit, q: &str) -> Option<SkillHit> {
    hit.score = match_score(&hit.name, &hit.description, q)?;
    Some(hit)
}

fn match_score(name: &str, description: &str, q: &str) -> Option<u8> {
    if q.is_empty() {
        return Some(1);
    }
    let (name, description) = (name.to_lowercase(), description.to_lowercase());
    if name == q {
        Some(4)
    } else if name.contains(q) {
        Some(3)
    } else if description.contains(q) {
        Some(2)
    } else {
        None
    }
}

/// URL-encode each `/`-separated path segment (empty segments dropped) so a
/// subpath is safe in a GitHub URL without escaping its slashes.
fn encode_path(path: &str) -> String {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| urlencoding::encode(s).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

// ── update / drift ──────────────────────────────────────────────────────────

/// Update status of one installed skill relative to its recorded source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// Installed revision matches the current upstream.
    UpToDate,
    /// Upstream has moved; `from`/`to` are the recorded and current revisions.
    Available { from: Option<String>, to: String },
    /// The on-disk package no longer matches its install hash — the user edited
    /// it. Protected: never overwritten by an update.
    ModifiedLocally,
    /// No re-fetchable remote to update from (local install, missing source, …).
    Unmanaged(&'static str),
}

/// Decide whether an installed user skill can/should be updated. Drift is
/// checked first and wins: a locally edited skill is protected, never clobbered.
/// Otherwise a re-fetchable GitHub source's current revision is compared with
/// the one recorded at install.
pub async fn check_update(store: &crate::skills::SkillStore, name: &str) -> UpdateStatus {
    let Some(prov) = store.provenance(name) else {
        return UpdateStatus::Unmanaged("no recorded source");
    };
    if let (Some(recorded), Some(disk)) = (prov.content_hash.as_deref(), store.installed_hash(name)) {
        if recorded != disk {
            return UpdateStatus::ModifiedLocally;
        }
    }
    if prov.kind != "github-folder" {
        return UpdateStatus::Unmanaged("installed from a non-GitHub source");
    }
    match crate::skills::current_revision(&prov.source).await {
        Some(cur) if Some(cur.as_str()) == prov.revision.as_deref() => UpdateStatus::UpToDate,
        Some(cur) => UpdateStatus::Available { from: prov.revision, to: cur },
        None => UpdateStatus::Unmanaged("source is not a resolvable GitHub folder"),
    }
}

// ── lockfile ────────────────────────────────────────────────────────────────

/// One locked skill: enough to reproduce an exact install. Committed with the
/// repo so a team shares the same skill set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockEntry {
    pub name: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LockDoc {
    #[serde(default, rename = "skill")]
    skills: Vec<LockEntry>,
}

/// The skills lockfile: name → exact source/revision/hash for every installed
/// skill, so a teammate reproduces the same set byte-for-byte. Reads tolerate
/// absence; a corrupt file is an error, never a silent empty lock.
pub struct SkillLock {
    path: PathBuf,
}

impl SkillLock {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn read(&self) -> Result<Vec<LockEntry>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => Ok(toml::from_str::<LockDoc>(&t)
                .map_err(|e| format!("{} is malformed: {e}", self.path.display()))?
                .skills),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("reading {}: {e}", self.path.display())),
        }
    }

    /// Write the lock, sorted by name for a stable, review-friendly diff.
    pub fn write(&self, mut entries: Vec<LockEntry>) -> Result<(), String> {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let body = toml::to_string_pretty(&LockDoc { skills: entries })
            .map_err(|e| format!("serializing lockfile: {e}"))?;
        atomic_write(&self.path, body.as_bytes())
    }
}

/// Build lock entries from the recorded provenance of the given installed
/// skills. Skills without provenance (hand-authored, no source) are skipped —
/// there is nothing to reproduce them from.
pub fn lock_entries(store: &crate::skills::SkillStore, names: &[String]) -> Vec<LockEntry> {
    names
        .iter()
        .filter_map(|n| {
            let p = store.provenance(n)?;
            Some(LockEntry {
                name: n.clone(),
                source: p.source,
                revision: p.revision,
                content_hash: p.content_hash,
            })
        })
        .collect()
}

/// The source to install a locked entry from — pinned to its exact revision
/// when known, so a sync reproduces the recorded bytes rather than the latest.
pub fn locked_source(entry: &LockEntry) -> String {
    match &entry.revision {
        Some(rev) => crate::skills::pin_tree_url(&entry.source, rev),
        None => entry.source.clone(),
    }
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

    #[test]
    fn match_score_ranks_name_over_description_and_drops_misses() {
        assert_eq!(match_score("pdf", "make pdfs", "pdf"), Some(4)); // exact name
        assert_eq!(match_score("pdf-tools", "x", "pdf"), Some(3)); // name substring
        assert_eq!(match_score("charts", "render a pdf", "pdf"), Some(2)); // description
        assert_eq!(match_score("charts", "render images", "pdf"), None); // no match → dropped
        assert_eq!(match_score("anything", "x", ""), Some(1)); // empty query lists all
        // case-insensitive
        assert_eq!(match_score("PDF", "X", "pdf"), Some(4));
    }

    #[test]
    fn encode_path_escapes_segments_not_slashes() {
        assert_eq!(encode_path("skills"), "skills");
        assert_eq!(encode_path("a/b c/d"), "a/b%20c/d");
        assert_eq!(encode_path("/leading//double/"), "leading/double");
    }

    #[test]
    fn check_update_protects_edits_and_reports_unmanaged() {
        let dir = tmp();
        let user = dir.join("user");
        std::fs::create_dir_all(&user).unwrap();
        let store = crate::skills::SkillStore::new(dir.join("proj"), Some(user.clone()));

        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), "---\nname: demo\ndescription: d\n---\n\nbody\n").unwrap();
        futures::executor::block_on(store.install_from(src.to_str().unwrap())).unwrap();

        // Installed from a local folder → nothing remote to update from.
        assert_eq!(
            futures::executor::block_on(check_update(&store, "demo")),
            UpdateStatus::Unmanaged("installed from a non-GitHub source")
        );
        // Edit on disk → drift wins and the skill is protected.
        std::fs::write(
            user.join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: d\n---\n\nedited\n",
        )
        .unwrap();
        assert_eq!(
            futures::executor::block_on(check_update(&store, "demo")),
            UpdateStatus::ModifiedLocally
        );
        // Unknown skill → unmanaged, not a panic.
        assert_eq!(
            futures::executor::block_on(check_update(&store, "nope")),
            UpdateStatus::Unmanaged("no recorded source")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lockfile_round_trips_and_captures_installed_provenance() {
        let dir = tmp();
        let user = dir.join("user");
        std::fs::create_dir_all(&user).unwrap();
        let store = crate::skills::SkillStore::new(dir.join("proj"), Some(user.clone()));

        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), "---\nname: demo\ndescription: d\n---\n\nbody\n").unwrap();
        futures::executor::block_on(store.install_from(src.to_str().unwrap())).unwrap();

        let entries = lock_entries(&store, &["demo".to_string(), "missing".to_string()]);
        assert_eq!(entries.len(), 1); // 'missing' has no provenance → skipped
        assert_eq!(entries[0].name, "demo");
        assert!(entries[0].content_hash.as_deref().unwrap().starts_with("sha256:"));

        let lock = SkillLock::new(dir.join("skills.lock"));
        assert!(lock.read().unwrap().is_empty()); // absent = empty, not error
        lock.write(entries.clone()).unwrap();
        assert_eq!(SkillLock::new(dir.join("skills.lock")).read().unwrap(), entries);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn locked_source_pins_github_to_revision() {
        let entry = LockEntry {
            name: "pdf".into(),
            source: "https://github.com/anthropics/skills/tree/main/document/pdf".into(),
            revision: Some("abc123".into()),
            content_hash: None,
        };
        assert_eq!(
            locked_source(&entry),
            "https://github.com/anthropics/skills/tree/abc123/document/pdf"
        );
        // A local source (no revision) is used verbatim.
        let local = LockEntry {
            name: "x".into(),
            source: "/home/u/x".into(),
            revision: None,
            content_hash: None,
        };
        assert_eq!(locked_source(&local), "/home/u/x");
    }
}
