//! Writer isolation (§6.4) — a child that modifies code gets its own checkout.
//!
//! Read-only children may share the parent's tree; writers may not. Two agents
//! editing one working tree collide through files, the index and cwd, and the
//! damage is silent: whichever wrote last wins and neither knows. So a writer is
//! given a `git worktree` cut from the parent's HEAD, works only there, and
//! returns a patch. Nothing it does touches the parent's tree until a human
//! approves the merge.
//!
//! Two writers cannot share a worktree *structurally*: the path is derived from
//! the child's session ULID, and [`WorktreePool`] refuses a second lease on a
//! path it already owns. Reaping is a [`Drop`] responsibility plus a sweep at
//! startup — the same reap-on-drop discipline the MCP and LSP managers use,
//! because an orphaned worktree wedges the next `git worktree add` on that path.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use thiserror::Error;
use tokio::io::AsyncReadExt;
use ulid::Ulid;

/// Branch prefix for agent worktrees. Namespaced so a sweep can recognise its
/// own leftovers without guessing, and so `git branch` reads intelligibly.
pub const BRANCH_PREFIX: &str = "medha/agent";

/// Where the owning process is recorded for a checkout: a *sibling* of the
/// worktree, never a file inside it. Anything inside is part of the child's
/// tree and lands in its diff — an ownership marker would make every patch
/// non-empty, including an idle child's.
fn owner_marker(worktree: &Path) -> PathBuf {
    let mut marker = worktree.as_os_str().to_os_string();
    marker.push(".owner");
    PathBuf::from(marker)
}

/// Marks a checkout whose work was never captured as a patch. A sibling, like
/// the owner marker, so it stays out of the child's diff.
fn keep_marker(worktree: &Path) -> PathBuf {
    let mut marker = worktree.as_os_str().to_os_string();
    marker.push(".keep");
    PathBuf::from(marker)
}

/// Whether this checkout holds work that exists nowhere else.
///
/// On disk rather than in memory, because the process that decided to keep it
/// is usually gone by the time anything sweeps: the in-memory lease dies with
/// it, and the next launch would read a live rescue as an abandoned directory
/// and force it away — losing exactly what the rescue was for.
fn keeps_work(worktree: &Path) -> bool {
    keep_marker(worktree).exists()
}

/// Whether the process that claimed `worktree` is still running.
///
/// A worktree is only abandoned if its owner is gone. Without this check a
/// second Medha in the same repository reads another's live checkout as stale
/// — it holds no lease for it — and removes it while a child is editing.
fn owner_alive(worktree: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(owner_marker(worktree)) else {
        // No marker: either a pre-existing checkout from before this mechanism,
        // or a crash between `worktree add` and the write. Treated as abandoned,
        // which is the same behaviour as before and safe for both.
        return false;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return false;
    };
    if pid == std::process::id() {
        return true;
    }
    #[cfg(unix)]
    {
        // Signal 0 tests for existence without delivering anything.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        // No cheap portable liveness test; err towards keeping a checkout that
        // might be live, since the cost of a leaked directory is far below the
        // cost of deleting work in progress.
        true
    }
}

/// Cap on the diff handed back to the parent, in bytes. A patch past this is
/// still returned whole to the caller — which owns the artifact store — but
/// [`Patch::is_large`] flags it so the summary path can spill instead of
/// flooding a context window.
pub const LARGE_PATCH_BYTES: usize = 64 * 1024;
/// Default hard ceiling for an extracted patch. A diff is duplicated into the
/// result and durable event, so allowing it to grow with the checkout makes a
/// single generated file an OOM vector.
pub const DEFAULT_MAX_PATCH_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("writer agents need a git repository; '{0}' is not inside one")]
    NotARepo(PathBuf),
    #[error("the repository has no commits yet — commit once before delegating writes")]
    NoCommits,
    #[error("worktree path '{0}' is already leased to another agent")]
    AlreadyLeased(PathBuf),
    #[error("git {command} failed: {message}")]
    Git { command: String, message: String },
    #[error("could not run git: {0}")]
    Spawn(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("patch exceeds the configured {limit}-byte limit; the checkout was preserved instead")]
    PatchTooLarge { limit: usize },
}

/// Run a fixed git command while reading at most `limit + 1` stdout bytes.
/// Seeing the extra byte is a distinct error; callers must never truncate a
/// patch because a truncated binary/unified diff is not safely applicable.
async fn git_bounded(dir: &Path, args: &[&str], limit: usize) -> Result<String, WorktreeError> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| WorktreeError::Spawn(error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| WorktreeError::Spawn("git stdout was not captured".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| WorktreeError::Spawn("git stderr was not captured".into()))?;
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes).await;
        bytes
    });
    let mut bytes = Vec::with_capacity(limit.min(1024 * 1024).saturating_add(1));
    let read_limit = limit.saturating_add(1) as u64;
    stdout
        .take(read_limit)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| WorktreeError::Io(error.to_string()))?;
    if bytes.len() > limit {
        let _ = child.kill().await;
        let _ = child.wait().await;
        stderr_task.abort();
        return Err(WorktreeError::PatchTooLarge { limit });
    }
    let status = child
        .wait()
        .await
        .map_err(|error| WorktreeError::Spawn(error.to_string()))?;
    let stderr = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let message = String::from_utf8_lossy(&stderr).trim().to_string();
        return Err(WorktreeError::Git {
            command: args.join(" "),
            message: if message.is_empty() {
                format!("exit {:?}", status.code())
            } else {
                message
            },
        });
    }
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

/// Run git in `dir` and return stdout, or the failure with git's own stderr —
/// which is far more actionable than an exit code ("fatal: not a git
/// repository" tells the model what to do; "status 128" does not).
async fn git(dir: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        // git needs $HOME (config, credentials) and $PATH, so the environment is
        // inherited. These are fixed subcommands, not an arbitrary-command
        // surface, so there is nothing here for a model to inject into.
        .output()
        .await
        .map_err(|error| WorktreeError::Spawn(error.to_string()))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(WorktreeError::Git {
            command: args.join(" "),
            message: if message.is_empty() {
                format!("exit {:?}", output.status.code())
            } else {
                message
            },
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// What a writer child hands back instead of a prose summary of its edits.
///
/// A summary of a change cannot be reviewed, verified or applied; a diff can.
/// The verification evidence travels with it because a patch that has not been
/// built is not a finished patch — §6.4 requires both, and the merge gate reads
/// `verified` rather than trusting the child's account of itself.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Patch {
    /// Unified diff against `base`, empty when the child changed nothing.
    pub diff: String,
    /// The commit the worktree was cut from. A patch is only meaningful
    /// relative to its base, and three-way merge needs it by name.
    pub base: String,
    /// Paths touched, for the merge preview and for conflict reporting.
    pub files: Vec<String>,
    /// What the child ran to prove the change works, and what came back.
    /// `None` means it never ran anything — which the gate must show, because
    /// "no evidence" and "evidence of success" are not the same answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        self.diff.trim().is_empty()
    }

    /// Whether this diff is too big to hand to a model inline.
    pub fn is_large(&self) -> bool {
        self.diff.len() > LARGE_PATCH_BYTES
    }

    /// Whether a merge gate should treat this as verified. Unverified is the
    /// default: absence of evidence is never evidence of success.
    pub fn verified(&self) -> bool {
        self.verification.as_ref().is_some_and(|v| v.passed)
    }
}

/// Evidence that a patch works, captured from the child's own build/test run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Verification {
    pub command: String,
    pub passed: bool,
    /// Tail of the output — enough to see the failure, bounded so a noisy build
    /// cannot flood the parent.
    pub output: String,
}

/// A checkout leased to exactly one writer child.
///
/// Removal happens in [`Drop`] as well as [`Worktree::reap`], so a cancelled or
/// panicking child cannot leave one behind. `git worktree add` fails on a path
/// that already exists, so a leak is not merely untidy — it breaks the next run.
pub struct Worktree {
    repo: PathBuf,
    path: PathBuf,
    branch: String,
    base: String,
    /// Shared with the pool, so the lease is released by the same guard that
    /// removes the directory and cannot drift out of step with it.
    leases: Leases,
    /// Set when the child's work could not be captured as a patch. Both `reap`
    /// and the `Drop` guard then leave the checkout alone: the directory is the
    /// only remaining copy of the work, and removing it is unrecoverable.
    preserve: std::sync::atomic::AtomicBool,
    max_patch_bytes: usize,
}

type Leases = Arc<Mutex<HashSet<PathBuf>>>;

impl Worktree {
    /// Where the child works. Both its cwd and its sandbox root must be this —
    /// if either still points at the parent, the isolation is decorative.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The commit this checkout was cut from.
    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Everything the child changed, as a patch against its base.
    ///
    /// Untracked files are staged with `--intent-to-add` first: without it a
    /// newly created file is invisible to `git diff`, and a child whose whole
    /// job was to add a module would return an empty patch and look like it had
    /// done nothing.
    pub async fn patch(&self) -> Result<Patch, WorktreeError> {
        git(&self.path, &["add", "--all", "--intent-to-add", "."]).await?;
        let diff = git_bounded(
            &self.path,
            // Binary files cannot be applied from a textual patch, so a marker
            // beats a corrupt hunk. `--no-color` because a configured
            // `color.ui = always` would otherwise embed escape codes in a diff
            // that has to survive being parsed and re-applied.
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--binary",
                &self.base,
            ],
            self.max_patch_bytes,
        )
        .await?;
        let files = git(
            &self.path,
            &["diff", "--name-only", "--no-ext-diff", &self.base],
        )
        .await?
        .lines()
        .map(str::to_string)
        .filter(|line| !line.is_empty())
        .collect();
        Ok(Patch {
            diff,
            base: self.base.clone(),
            files,
            verification: None,
        })
    }

    /// Remove the checkout and its branch. Idempotent, and safe to call before
    /// the [`Drop`] guard runs — the pool lease is what makes the second call a
    /// no-op rather than a spurious git failure.
    /// Lay a previous patch into this checkout, so a follow-up continues from
    /// where the agent left off.
    ///
    /// Without it a resumed writer starts from a clean HEAD and has to redo —
    /// or silently discard — everything it had already done, which is the one
    /// outcome a follow-up exists to avoid.
    pub async fn restore(&self, patch: &Patch) -> Result<(), WorktreeError> {
        if patch.is_empty() {
            return Ok(());
        }
        apply(&self.path, patch, &["--3way"], None).await
    }

    /// Keep this checkout: its work was never captured, so the directory is the
    /// only copy left. Survives `reap`, the `Drop` guard, and — because the mark
    /// is on disk — the next process's sweep.
    pub fn preserve(&self) {
        self.preserve
            .store(true, std::sync::atomic::Ordering::Release);
        let marker = keep_marker(&self.path);
        if let Err(error) = std::fs::write(
            &marker,
            format!(
                "Medha kept this checkout: an agent's changes could not be captured as a patch.\n\
                 Recover them from here, then delete this file and the directory.\n\
                 branch: {}\nbase: {}\n",
                self.branch, self.base
            ),
        ) {
            // Nothing else can carry this across a restart, so say so loudly
            // rather than leaving a rescue that the next sweep will undo.
            tracing::error!(
                target: "medha_orchestrator",
                path = %self.path.display(), %error,
                "could not mark a checkout as preserved; it may be swept on restart"
            );
        }
    }

    fn preserved(&self) -> bool {
        self.preserve.load(std::sync::atomic::Ordering::Acquire)
    }

    pub async fn reap(&self) {
        if self.preserved() || keeps_work(&self.path) {
            if let Ok(mut leases) = self.leases.lock() {
                leases.remove(&self.path);
            }
            return;
        }
        if !self
            .leases
            .lock()
            .map(|l| l.contains(&self.path))
            .unwrap_or(false)
        {
            return;
        }
        // `--force` because the child is expected to leave uncommitted work:
        // the patch has already been taken, and refusing to clean up a dirty
        // worktree would strand every writer that did its job.
        let _ = git(
            &self.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.to_string_lossy(),
            ],
        )
        .await;
        let _ = git(&self.repo, &["worktree", "prune"]).await;
        let _ = git(&self.repo, &["branch", "-D", &self.branch]).await;
        // The marker is a sibling of the checkout, so removing the worktree does
        // not take it with it.
        let _ = std::fs::remove_file(owner_marker(&self.path));
        if let Ok(mut leases) = self.leases.lock() {
            leases.remove(&self.path);
        }
    }
}

impl Drop for Worktree {
    /// Last-resort cleanup for the paths `reap` never reaches — a panic, or a
    /// run future dropped mid-cancellation. `Drop` cannot await, so this hands
    /// the removal to a detached blocking command; the lease is released here so
    /// the pool's view is correct immediately either way.
    fn drop(&mut self) {
        // Preserved: hold the lease as well as the directory. Releasing it would
        // let this process's own sweep treat the checkout as abandoned and
        // force it away — the exact loss the flag exists to prevent.
        if self.preserved() || keeps_work(&self.path) {
            return;
        }
        let held = self
            .leases
            .lock()
            .map(|mut leases| leases.remove(&self.path))
            .unwrap_or(false);
        if !held {
            return;
        }
        let repo = self.repo.clone();
        let path = self.path.to_string_lossy().to_string();
        let branch = self.branch.clone();
        std::thread::spawn(move || {
            let run = |args: &[&str]| {
                let _ = std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            };
            run(&["worktree", "remove", "--force", &path]);
            run(&["worktree", "prune"]);
            run(&["branch", "-D", &branch]);
        });
    }
}

/// Hands out isolated checkouts, one per writer child.
///
/// Owns a directory outside the working tree — a worktree inside the repo would
/// show up in the parent's own `git status`, its greps and its builds, which is
/// the collision this exists to prevent.
pub struct WorktreePool {
    repo: PathBuf,
    dir: PathBuf,
    leases: Leases,
    max_patch_bytes: usize,
}

impl WorktreePool {
    /// `dir` must be Medha-owned state (the per-workspace state directory), not
    /// a path inside the repository.
    pub fn new(repo: impl Into<PathBuf>, dir: impl Into<PathBuf>) -> Self {
        Self {
            repo: repo.into(),
            dir: dir.into(),
            leases: Arc::new(Mutex::new(HashSet::new())),
            max_patch_bytes: DEFAULT_MAX_PATCH_BYTES,
        }
    }

    /// Configure the hard extraction ceiling. Exceeding it preserves the
    /// checkout; it never returns a truncated patch.
    pub fn with_max_patch_bytes(mut self, max_patch_bytes: usize) -> Self {
        self.max_patch_bytes = max_patch_bytes;
        self
    }

    /// Resolve the repository root for `path`, so a pool created from a
    /// subdirectory still cuts worktrees from the real repo.
    pub async fn discover(
        path: impl AsRef<Path>,
        dir: impl Into<PathBuf>,
    ) -> Result<Self, WorktreeError> {
        let path = path.as_ref();
        let root = git(path, &["rev-parse", "--show-toplevel"])
            .await
            .map_err(|_| WorktreeError::NotARepo(path.to_path_buf()))?;
        if root.is_empty() {
            return Err(WorktreeError::NotARepo(path.to_path_buf()));
        }
        Ok(Self::new(PathBuf::from(root), dir))
    }

    /// Cut a checkout for `session` from the repository's current HEAD.
    ///
    /// The base is resolved to a concrete commit rather than left as `HEAD`: the
    /// parent keeps working while the child runs, so a symbolic base would move
    /// underneath the diff and the returned patch would describe changes nobody
    /// made.
    pub async fn checkout(&self, session: Ulid) -> Result<Worktree, WorktreeError> {
        let base = git(&self.repo, &["rev-parse", "HEAD"])
            .await
            .map_err(|_| WorktreeError::NoCommits)?;
        // Canonicalize the parent directory *before* git ever sees the path.
        // The sandbox jail compares canonical paths (on macOS `/var` is a
        // symlink to `/private/var`), but git remembers a worktree by the exact
        // string it was registered with — so handing git one spelling and the
        // sandbox another leaves a worktree that cannot be removed by name.
        std::fs::create_dir_all(&self.dir).map_err(|e| WorktreeError::Io(e.to_string()))?;
        let dir = self.dir.canonicalize().unwrap_or_else(|_| self.dir.clone());
        let name = session.to_string().to_lowercase();
        let path = dir.join(&name);
        let branch = format!("{BRANCH_PREFIX}/{name}");

        {
            // Structural, not conventional: the lease is taken before the
            // directory exists, so two callers racing on one path cannot both
            // believe they own it.
            let mut leases = self
                .leases
                .lock()
                .map_err(|_| WorktreeError::Io("lease table poisoned".into()))?;
            if !leases.insert(path.clone()) {
                return Err(WorktreeError::AlreadyLeased(path));
            }
        }
        // From here on any failure must release the lease, or the path is
        // wedged for the rest of the process.
        let created = git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                &path.to_string_lossy(),
                &base,
            ],
        )
        .await;
        if let Err(error) = created {
            if let Ok(mut leases) = self.leases.lock() {
                leases.remove(&path);
            }
            return Err(error);
        }
        // Claim it on disk as well as in memory. The lease table is per-process,
        // so a second Medha running in the same repository would otherwise see a
        // live worktree as abandoned and force-remove it mid-edit — two
        // terminals in one project is ordinary, and the child's uncommitted work
        // would go with it.
        if let Err(error) = std::fs::write(owner_marker(&path), std::process::id().to_string()) {
            let _ = git(
                &self.repo,
                &["worktree", "remove", "--force", &path.to_string_lossy()],
            )
            .await;
            let _ = git(&self.repo, &["worktree", "prune"]).await;
            let _ = git(&self.repo, &["branch", "-D", &branch]).await;
            if let Ok(mut leases) = self.leases.lock() {
                leases.remove(&path);
            }
            return Err(WorktreeError::Io(format!(
                "could not record the worktree owner: {error}"
            )));
        }
        Ok(Worktree {
            preserve: std::sync::atomic::AtomicBool::new(false),
            repo: self.repo.clone(),
            path,
            branch,
            base,
            leases: Arc::clone(&self.leases),
            max_patch_bytes: self.max_patch_bytes,
        })
    }

    /// Remove agent worktrees left by a previous process.
    ///
    /// Called at startup: a crash between `worktree add` and reaping leaves both
    /// a registration and a directory, and `git worktree add` refuses a path
    /// that already exists — so without this the *next* run of the same agent
    /// fails for a reason that has nothing to do with it.
    pub async fn sweep(&self) {
        let listed = git(&self.repo, &["worktree", "list", "--porcelain"])
            .await
            .unwrap_or_default();
        let mut current: Option<String> = None;
        let mut stale: Vec<String> = Vec::new();
        for line in listed.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                current = Some(path.to_string());
            } else if let Some(branch) = line.strip_prefix("branch ") {
                // Only ours. Someone else's worktree in the same repo is not
                // Medha's to remove, however stale it looks.
                let prefix = format!("refs/heads/{BRANCH_PREFIX}/");
                if branch.starts_with(&prefix)
                    && let Some(path) = current.take()
                {
                    stale.push(path);
                }
            }
        }
        for path in stale {
            // A live lease means this process owns it and it is not stale.
            if self
                .leases
                .lock()
                .map(|leases| leases.contains(Path::new(&path)))
                .unwrap_or(false)
            {
                continue;
            }
            // …and another live Medha's checkout is not this one's to remove.
            // The lease table only knows about this process.
            if owner_alive(Path::new(&path)) {
                continue;
            }
            // …and a checkout holding uncaptured work is never abandoned, however
            // dead its owner. The marker outlives the process that wrote it,
            // which is the whole point: the rescue has to survive the restart.
            if keeps_work(Path::new(&path)) {
                tracing::info!(
                    target: "medha_orchestrator",
                    %path,
                    "keeping a checkout whose work was never captured as a patch"
                );
                continue;
            }
            // Missing markers are not permission to destroy uncommitted work:
            // the owner/keep write may have failed or this may be a checkout
            // from an older Medha. Fail closed on both dirty output and an
            // inability to inspect it.
            match git_bounded(Path::new(&path), &["status", "--porcelain"], 0).await {
                Ok(_) => {}
                Err(WorktreeError::PatchTooLarge { .. }) => {
                    tracing::warn!(
                        target: "medha_orchestrator",
                        %path,
                        "keeping an unmarked checkout because it has uncommitted work"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        target: "medha_orchestrator",
                        %path, %error,
                        "keeping an unmarked checkout because its state could not be inspected"
                    );
                    continue;
                }
            }
            let _ = git(&self.repo, &["worktree", "remove", "--force", &path]).await;
            let _ = std::fs::remove_file(owner_marker(Path::new(&path)));
        }
        let _ = git(&self.repo, &["worktree", "prune"]).await;
        // Branches outlive their worktree's removal, so prune them too or the
        // namespace grows without bound across sessions.
        let branches = git(
            &self.repo,
            &["branch", "--list", &format!("{BRANCH_PREFIX}/*")],
        )
        .await
        .unwrap_or_default();
        for branch in branches.lines() {
            let branch = branch.trim_start_matches('*').trim();
            if !branch.is_empty() {
                let _ = git(&self.repo, &["branch", "-D", branch]).await;
            }
        }
    }
}

/// How a patch stands against the tree it would be applied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeCheck {
    /// Applies cleanly.
    Clean,
    /// Applies with three-way resolution — the base moved but the edits do not
    /// overlap.
    ThreeWay,
    /// Overlapping edits. §6.4: this goes to reconciliation, never to
    /// last-writer-wins.
    Conflict,
    /// Nothing to apply.
    Empty,
}

/// Test a patch against the current tree *without* touching it.
///
/// Three-way, because by the time a child finishes its base is usually behind —
/// the parent kept working — and a strict apply would reject patches that merge
/// perfectly well.
///
/// `git apply --3way --check` looks like the dry run for this and is not: under
/// `--3way` git performs the merge and writes conflict markers into the working
/// tree, then exits **0**. Using it would corrupt the user's files while
/// reporting success. So the three-way attempt runs `--cached` against a
/// *scratch copy of the index*: git resolves the merge against real blobs, the
/// exit status tells the truth about conflicts, and neither the working tree nor
/// the real index is touched.
pub async fn check(repo: &Path, patch: &Patch) -> MergeCheck {
    if patch.is_empty() {
        return MergeCheck::Empty;
    }
    // Plain `--check` has no `--3way`, so it really is a dry run.
    if apply(repo, patch, &["--check"], None).await.is_ok() {
        return MergeCheck::Clean;
    }
    let Ok(scratch) = ScratchIndex::new(repo).await else {
        // Without a scratch index there is no non-destructive way to ask, and
        // guessing "mergeable" would be guessing in the direction that damages
        // the tree.
        return MergeCheck::Conflict;
    };
    match apply(repo, patch, &["--3way", "--cached"], Some(scratch.path())).await {
        Ok(()) => MergeCheck::ThreeWay,
        Err(_) => MergeCheck::Conflict,
    }
}

/// Apply a patch to `repo` for real. Only ever reached after the human gate:
/// merging a child's work is a consequential action on the user's tree.
///
/// Returns the failure verbatim, because a merge that half-applied is exactly
/// the situation where a paraphrased error is worthless.
pub async fn merge(repo: &Path, patch: &Patch) -> Result<MergeCheck, WorktreeError> {
    let check = check(repo, patch).await;
    match check {
        MergeCheck::Empty => Ok(check),
        MergeCheck::Conflict => Err(WorktreeError::Git {
            command: "apply --3way".into(),
            message: "the patch conflicts with the current tree".into(),
        }),
        // A clean patch is applied strictly. Reaching for `--3way` here would
        // buy nothing and cost the guarantee that a merge which passed a strict
        // check cannot leave conflict markers behind.
        MergeCheck::Clean => apply(repo, patch, &[], None).await.map(|()| check),
        MergeCheck::ThreeWay => apply(repo, patch, &["--3way"], None).await.map(|()| check),
    }
}

/// A throwaway copy of the repository index, so a three-way apply can be
/// *asked about* without being performed. Removed on drop.
struct ScratchIndex {
    path: PathBuf,
}

impl ScratchIndex {
    async fn new(repo: &Path) -> Result<Self, WorktreeError> {
        let git_dir = git(repo, &["rev-parse", "--absolute-git-dir"]).await?;
        let index = Path::new(&git_dir).join("index");
        // Alongside the real index rather than in the temp dir: `git apply`
        // resolves relative worktree paths from the repository, and a scratch
        // index on another filesystem is a rename away from failing.
        let path = Path::new(&git_dir).join(format!("medha-scratch-{}.index", Ulid::new()));
        // A repository with nothing staged has no index file yet; an absent
        // scratch index is a valid empty one, so that is not an error.
        if index.exists() {
            std::fs::copy(&index, &path).map_err(|e| WorktreeError::Io(e.to_string()))?;
        }
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Feed a patch to `git apply` on stdin. Writing it to a temp file first would
/// be one more thing to clean up, and one more path for the diff to leak to.
///
/// `index` redirects git to a scratch index; `None` uses the repository's own.
async fn apply(
    repo: &Path,
    patch: &Patch,
    flags: &[&str],
    index: Option<&Path>,
) -> Result<(), WorktreeError> {
    use tokio::io::AsyncWriteExt;

    let mut command = tokio::process::Command::new("git");
    command.arg("apply").args(flags).current_dir(repo);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| WorktreeError::Spawn(error.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A diff that does not end in a newline is rejected as corrupt.
        let mut body = patch.diff.clone();
        if !body.ends_with('\n') {
            body.push('\n');
        }
        let _ = stdin.write_all(body.as_bytes()).await;
        drop(stdin);
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| WorktreeError::Spawn(error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(WorktreeError::Git {
        command: format!("apply {}", flags.join(" ")),
        message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real repository with one commit — worktrees are a git feature, so
    /// faking git here would test nothing that matters.
    async fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git(&root, &["init", "--initial-branch=main"])
            .await
            .unwrap();
        git(&root, &["config", "user.email", "t@example.com"])
            .await
            .unwrap();
        git(&root, &["config", "user.name", "t"]).await.unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(&root, &["add", "."]).await.unwrap();
        git(&root, &["commit", "-m", "base"]).await.unwrap();
        (dir, root)
    }

    fn pool(root: &Path) -> (tempfile::TempDir, WorktreePool) {
        let state = tempfile::tempdir().unwrap();
        let pool = WorktreePool::new(root, state.path().join("worktrees"));
        (state, pool)
    }

    #[tokio::test]
    async fn a_writer_works_in_its_own_checkout_not_the_parents() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();

        assert_ne!(tree.path(), root);
        assert!(!tree.path().starts_with(&root));
        std::fs::write(tree.path().join("a.txt"), "one\nCHANGED\nthree\n").unwrap();
        // The parent's tree is untouched — that is the whole point.
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
    }

    /// The `Drop` guard force-removes any checkout it still holds a lease for,
    /// so merely declining to call `reap` was not enough to keep work whose
    /// patch could not be captured — the directory went anyway, on a path with
    /// no other copy of it.
    #[tokio::test]
    async fn a_preserved_checkout_survives_being_dropped() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let path = {
            let tree = pool.checkout(Ulid::new()).await.unwrap();
            std::fs::write(tree.path().join("a.txt"), "work nobody captured\n").unwrap();
            tree.preserve();
            tree.path().to_path_buf()
        };
        // The guard has run by now; give the detached remover a chance to as
        // well, so this fails loudly if preservation is not honoured.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(path.exists(), "preserved checkout was removed anyway");
        assert_eq!(
            std::fs::read_to_string(path.join("a.txt")).unwrap(),
            "work nobody captured\n"
        );
        // And the sweep must not take it either: the lease is deliberately held.
        pool.sweep().await;
        assert!(path.exists(), "sweep removed a preserved checkout");
    }

    /// The rescue has to outlive the process that made it. A restart holds no
    /// lease and reads a dead owner pid, so without an on-disk mark the next
    /// launch sweeps away exactly the work the rescue existed to save.
    #[tokio::test]
    async fn a_preserved_checkout_survives_a_restart() {
        let (_repo, root) = repo().await;
        let (state, pool) = pool(&root);
        let path = {
            let tree = pool.checkout(Ulid::new()).await.unwrap();
            std::fs::write(tree.path().join("a.txt"), "unrecoverable elsewhere\n").unwrap();
            tree.preserve();
            tree.path().to_path_buf()
        };
        // A second pool over the same directory is what the next launch has:
        // no lease table, and an owner marker naming a process that is gone.
        std::fs::write(owner_marker(&path), dead_pid().to_string()).unwrap();
        let restarted = WorktreePool::new(&root, state.path().join("worktrees"));
        restarted.sweep().await;
        assert!(path.exists(), "restart swept away preserved work");
        assert_eq!(
            std::fs::read_to_string(path.join("a.txt")).unwrap(),
            "unrecoverable elsewhere\n"
        );
    }

    #[tokio::test]
    async fn two_writers_can_never_share_a_checkout() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let session = Ulid::new();
        let _first = pool.checkout(session).await.unwrap();
        // Same session id is the only way to collide, and it is refused rather
        // than handing a second writer the same directory.
        assert!(matches!(
            pool.checkout(session).await,
            Err(WorktreeError::AlreadyLeased(_))
        ));
        // Distinct children always get distinct paths.
        let other = pool.checkout(Ulid::new()).await.unwrap();
        assert_ne!(other.path(), _first.path());
    }

    #[tokio::test]
    async fn a_patch_carries_edits_new_files_and_its_base() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\nEDITED\nthree\n").unwrap();
        std::fs::write(tree.path().join("new.rs"), "fn added() {}\n").unwrap();

        let patch = tree.patch().await.unwrap();
        assert!(!patch.is_empty());
        assert!(patch.diff.contains("EDITED"));
        // Without intent-to-add a created file is invisible to `git diff`, and a
        // child that only added a module would report having done nothing.
        assert!(patch.diff.contains("fn added"));
        assert!(patch.files.contains(&"a.txt".to_string()));
        assert!(patch.files.contains(&"new.rs".to_string()));
        assert_eq!(patch.base.len(), 40);
    }

    #[tokio::test]
    async fn an_idle_writer_returns_an_empty_patch_not_an_error() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        let patch = tree.patch().await.unwrap();
        assert!(patch.is_empty());
        assert_eq!(check(&root, &patch).await, MergeCheck::Empty);
    }

    #[tokio::test]
    async fn a_patch_merges_back_into_the_parent() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\nEDITED\nthree\n").unwrap();
        let patch = tree.patch().await.unwrap();

        assert_eq!(check(&root, &patch).await, MergeCheck::Clean);
        merge(&root, &patch).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\nEDITED\nthree\n"
        );
    }

    #[tokio::test]
    async fn a_moved_base_still_merges_when_the_edits_do_not_overlap() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\ntwo\nthree\nfrom-child\n").unwrap();
        let patch = tree.patch().await.unwrap();

        // The parent kept working while the child ran — the usual case, not an
        // edge one. A strict apply would reject this.
        std::fs::write(root.join("b.txt"), "parent moved on\n").unwrap();
        git(&root, &["add", "."]).await.unwrap();
        git(&root, &["commit", "-m", "parent"]).await.unwrap();

        assert!(matches!(
            check(&root, &patch).await,
            MergeCheck::Clean | MergeCheck::ThreeWay
        ));
        merge(&root, &patch).await.unwrap();
        assert!(
            std::fs::read_to_string(root.join("a.txt"))
                .unwrap()
                .contains("from-child")
        );
    }

    #[tokio::test]
    async fn overlapping_edits_are_reported_as_conflicts_not_applied() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\nCHILD\nthree\n").unwrap();
        let patch = tree.patch().await.unwrap();

        // The parent rewrote the same line the child did.
        std::fs::write(root.join("a.txt"), "one\nPARENT\nthree\nplus\n").unwrap();
        git(&root, &["add", "."]).await.unwrap();
        git(&root, &["commit", "-m", "parent edit"]).await.unwrap();

        assert_eq!(check(&root, &patch).await, MergeCheck::Conflict);
        // Never last-writer-wins: the merge is refused and the tree is left as
        // the parent had it.
        assert!(merge(&root, &patch).await.is_err());
        assert!(
            std::fs::read_to_string(root.join("a.txt"))
                .unwrap()
                .contains("PARENT")
        );
    }

    #[tokio::test]
    async fn asking_whether_a_patch_merges_never_changes_the_tree() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\nCHILD\nthree\n").unwrap();
        let patch = tree.patch().await.unwrap();

        std::fs::write(root.join("a.txt"), "one\nPARENT\nthree\nplus\n").unwrap();
        git(&root, &["add", "."]).await.unwrap();
        git(&root, &["commit", "-m", "parent edit"]).await.unwrap();
        let before = std::fs::read_to_string(root.join("a.txt")).unwrap();

        // `git apply --3way --check` is not a dry run: it writes conflict
        // markers into the working tree and *exits zero*. Checking twice would
        // have left the file mangled and reported the patch as mergeable.
        for _ in 0..2 {
            assert_eq!(check(&root, &patch).await, MergeCheck::Conflict);
        }
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), before);
        assert!(!before.contains("<<<<<<<"));
        // The index is untouched too — the check runs against a scratch copy.
        assert!(
            git(&root, &["status", "--porcelain"])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_scratch_index_does_not_outlive_the_check() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\nCHILD\nthree\n").unwrap();
        let patch = tree.patch().await.unwrap();
        std::fs::write(root.join("a.txt"), "one\nPARENT\nthree\nplus\n").unwrap();
        git(&root, &["add", "."]).await.unwrap();
        git(&root, &["commit", "-m", "parent"]).await.unwrap();

        check(&root, &patch).await;
        let leftovers: Vec<_> = std::fs::read_dir(root.join(".git"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("medha-scratch-")
            })
            .collect();
        assert!(leftovers.is_empty());
    }

    #[tokio::test]
    async fn reaping_removes_the_checkout_and_frees_the_path() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let session = Ulid::new();
        let tree = pool.checkout(session).await.unwrap();
        let path = tree.path().to_path_buf();
        tree.reap().await;

        assert!(!path.exists());
        // The lease is released with the directory, so the same id can be
        // leased again — an orphaned lease would wedge it forever.
        assert!(pool.checkout(session).await.is_ok());
    }

    #[tokio::test]
    async fn a_sweep_clears_worktrees_a_crashed_process_left_behind() {
        let (_repo, root) = repo().await;
        let (state, pool) = pool(&root);
        let leaked = pool.checkout(Ulid::new()).await.unwrap();
        let path = leaked.path().to_path_buf();
        // Forget the guard: exactly what a crash does — the registration and the
        // directory survive with nothing left to clean them up. The owner marker
        // has to name a dead process too, since a real crash is followed by a
        // *new* process with a different pid; leaving this one's pid there would
        // be simulating a crash that somehow kept running.
        std::mem::forget(leaked);
        std::fs::write(owner_marker(&path), dead_pid().to_string()).unwrap();
        assert!(path.exists());

        let fresh = WorktreePool::new(&root, state.path().join("worktrees"));
        fresh.sweep().await;
        assert!(!path.exists());
        // And the next run of the same agent is not blocked by the leftovers.
        assert!(
            git(&root, &["branch", "--list", &format!("{BRANCH_PREFIX}/*")])
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A pid that has certainly exited: spawn something trivial and reap it.
    /// Inventing a large number would be a guess that some CI box falsifies.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a short-lived process");
        let pid = child.id();
        let _ = child.wait();
        pid
    }

    /// Two Medha processes in one repository is ordinary — two terminals. The
    /// lease table is per-process, so without an on-disk owner the second one
    /// reads the first's live checkout as abandoned and force-removes it, taking
    /// the child's uncommitted work with it.
    #[tokio::test]
    async fn a_sweep_leaves_another_live_medhas_worktree_alone() {
        let (_repo, root) = repo().await;
        let (state, pool) = pool(&root);
        let theirs = pool.checkout(Ulid::new()).await.unwrap();
        let path = theirs.path().to_path_buf();
        std::fs::write(path.join("work-in-progress.txt"), "half an edit").unwrap();

        // A different process: no lease for this path, and it must still refuse
        // to touch it because the owner is alive.
        let other = WorktreePool::new(&root, state.path().join("worktrees"));
        other.sweep().await;

        assert!(
            path.exists(),
            "a live checkout was removed by another process"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("work-in-progress.txt")).unwrap(),
            "half an edit"
        );
    }

    #[tokio::test]
    async fn a_sweep_leaves_worktrees_that_are_not_medhas_alone() {
        let (_repo, root) = repo().await;
        let (state, pool) = pool(&root);
        let theirs = state.path().join("someone-elses");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "feature/theirs",
                &theirs.to_string_lossy(),
                "HEAD",
            ],
        )
        .await
        .unwrap();

        pool.sweep().await;
        assert!(theirs.exists());
    }

    #[tokio::test]
    async fn a_non_repository_is_refused_rather_than_silently_unisolated() {
        let dir = tempfile::tempdir().unwrap();
        // Degrading to "write in the parent's tree" here would be the one
        // outcome writer isolation exists to prevent.
        assert!(matches!(
            WorktreePool::discover(dir.path(), dir.path().join("wt")).await,
            Err(WorktreeError::NotARepo(_))
        ));
    }

    #[tokio::test]
    async fn a_repository_with_no_commits_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git(&root, &["init"]).await.unwrap();
        let (_state, pool) = pool(&root);
        assert!(matches!(
            pool.checkout(Ulid::new()).await,
            Err(WorktreeError::NoCommits)
        ));
    }

    #[test]
    fn evidence_of_success_and_absence_of_evidence_are_different_answers() {
        let mut patch = Patch {
            diff: "diff".into(),
            ..Default::default()
        };
        assert!(!patch.verified());
        patch.verification = Some(Verification {
            command: "cargo test".into(),
            passed: false,
            output: "1 failed".into(),
        });
        assert!(!patch.verified());
        patch.verification = Some(Verification {
            command: "cargo test".into(),
            passed: true,
            output: "ok".into(),
        });
        assert!(patch.verified());
    }
}
