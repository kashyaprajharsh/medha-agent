//! Protected content is repaired only after the owned process tree has settled.

use crate::{
    exec::ExecError,
    git_snapshot::{Node, read_below, restore_below},
};
use kernel::protected_paths::matters;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

const BYTES: usize = 16 * 1024 * 1024;
const ENTRIES: usize = 4096;
/// Folders holding protected content are walked even when Git ignores them.
const ALWAYS_WALKED: [&str; 3] = [".github/workflows", ".medha/prompts", ".vscode"];

pub(crate) struct Snapshot {
    root: PathBuf,
    skipped: BTreeSet<PathBuf>,
    before: BTreeMap<PathBuf, Entry>,
    digests: BTreeMap<PathBuf, [u8; 32]>,
    reviews: Arc<kernel::mutations::Guard>,
    done: bool,
}

/// A protected path as it stood: its content, or where it pointed when it was a link.
#[derive(Clone, PartialEq)]
enum Entry {
    Node(Node),
    Link(OsString),
}

const MISSING: Entry = Entry::Node(Node::Missing);

fn contents(entry: &Entry) -> Option<&[u8]> {
    if let Entry::Node(Node::File(bytes, _)) = entry {
        Some(bytes)
    } else {
        None
    }
}

/// What a person reviewing the change reads for this entry.
fn shown(entry: &Entry) -> Option<Vec<u8>> {
    match entry {
        Entry::Link(target) => {
            Some(format!("(link to {})\n", target.to_string_lossy()).into_bytes())
        }
        _ => contents(entry).map(<[u8]>::to_vec),
    }
}

fn digest(entry: &Entry) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update([match entry {
        Entry::Node(Node::File(..)) => 1,
        Entry::Link(_) => 2,
        Entry::Node(_) => 0,
    }]);
    hash.update(shown(entry).unwrap_or_default());
    hash.finalize().into()
}

#[cfg(unix)]
fn link_at(root: &Path, relative: &Path) -> std::io::Result<Option<OsString>> {
    crate::git_snapshot_unix::read_link(root, relative)
}

#[cfg(not(unix))]
fn link_at(root: &Path, relative: &Path) -> std::io::Result<Option<OsString>> {
    let path = root.join(relative);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Ok(Some(std::fs::read_link(path)?.into_os_string()))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn put_link(root: &Path, relative: &Path, target: &OsStr) -> std::io::Result<()> {
    crate::git_snapshot_unix::write_link(root, relative, Some(target))
}

#[cfg(not(unix))]
fn put_link(root: &Path, relative: &Path, target: &OsStr) -> std::io::Result<()> {
    let path = root.join(relative);
    restore_below(root, relative, &Node::Missing)?;
    std::os::windows::fs::symlink_file(target, path)
}

/// A link is read as a link, never followed; anything else as before.
fn read_entry(root: &Path, relative: &Path, budget: &mut (usize, usize)) -> std::io::Result<Entry> {
    match link_at(root, relative)? {
        Some(target) => Ok(Entry::Link(target)),
        None => read_below(root, relative, budget).map(Entry::Node),
    }
}

fn write_entry(root: &Path, relative: &Path, entry: &Entry) -> std::io::Result<()> {
    match entry {
        Entry::Node(node) => restore_below(root, relative, node),
        Entry::Link(target) => put_link(root, relative, target),
    }
}

/// Directories Git ignores, listed once before the command so the command
/// cannot hide a new file by editing ignore rules. Empty means walk everything.
fn ignored_directories(root: &Path) -> BTreeSet<PathBuf> {
    let args = [
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
    ]
    .map(String::from);
    let Ok(args) = git_safety::raw_arguments(root, &args) else {
        return BTreeSet::new();
    };
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(root)
        .env_clear()
        .envs(git_safety::environment());
    let Ok(output) = crate::exec::run_command_bounded_sync(
        command,
        std::time::Duration::from_secs(10),
        16 * 1024 * 1024,
        4096,
    ) else {
        return BTreeSet::new();
    };
    if output.status != Some(0) || output.timed_out || output.stdout_truncated {
        return BTreeSet::new();
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|line| line.strip_suffix(b"/"))
        .filter_map(|line| std::str::from_utf8(line).ok())
        .map(PathBuf::from)
        .filter(|path| {
            path.components()
                .all(|part| matches!(part, Component::Normal(_)))
        })
        .filter(|path| {
            !ALWAYS_WALKED.iter().any(|walked| {
                let walked = walked.to_ascii_lowercase();
                let path = path.to_string_lossy().to_ascii_lowercase();
                Path::new(&walked).starts_with(&path) || Path::new(&path).starts_with(&walked)
            })
        })
        .collect()
}

fn inventory(
    root: &Path,
    skipped: &BTreeSet<PathBuf>,
    reviews: &kernel::mutations::Guard,
    tolerate_changed_entries: bool,
) -> std::io::Result<BTreeMap<PathBuf, Entry>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![(root.to_path_buf(), false)];
    let extra_patterns = !reviews.settings().protected_paths.is_empty();
    let mut budget = (BYTES, ENTRIES);
    let mut visited = 0usize;
    while let Some((directory, protected_tree)) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            visited += 1;
            if visited > 1_000_000 {
                return Err(std::io::Error::other(
                    "protected-path inventory exceeds one million entries",
                ));
            }
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(std::io::Error::other)?;
            let name = entry.file_name();
            if name.eq_ignore_ascii_case(".git") {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                let parent = directory.file_name().unwrap_or_default();
                let protected = protected_tree
                    || (parent.eq_ignore_ascii_case(".github")
                        && name.eq_ignore_ascii_case("workflows"))
                    || (parent.eq_ignore_ascii_case(".medha")
                        && name.eq_ignore_ascii_case("prompts"));
                if protected || !skipped.contains(relative) {
                    pending.push((path.clone(), protected));
                }
                continue;
            }
            let candidate = protected_tree
                || extra_patterns
                || [
                    "AGENTS.md",
                    "MEDHA.md",
                    "CLAUDE.md",
                    "package.json",
                    ".envrc",
                    ".gitlab-ci.yml",
                    "tasks.json",
                    "launch.json",
                ]
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate));
            if !candidate {
                continue;
            }
            if !kind.is_dir() && reviews.classify(relative, true).is_some() {
                let entry = match read_entry(root, relative, &mut budget) {
                    Ok(entry) => entry,
                    Err(_) if tolerate_changed_entries => MISSING,
                    Err(error) => {
                        return Err(std::io::Error::new(
                            error.kind(),
                            format!("{} ({error})", relative.display()),
                        ));
                    }
                };
                files.insert(relative.to_path_buf(), entry);
            }
        }
    }
    Ok(files)
}

impl Snapshot {
    pub fn prepare(root: &Path, command: &str) -> Result<Option<Self>, ExecError> {
        let Some((reviews, true)) = kernel::egress::mutation_context() else {
            return Ok(None);
        };
        let root = root
            .canonicalize()
            .map_err(|error| ExecError::Unavailable(error.to_string()))?;
        if kernel::egress::execution_workspace().is_some_and(|workspace| workspace != root) {
            return Ok(None);
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let Some(root) = git_safety::project_root(&root, &home)
            .map_err(|error| ExecError::Unavailable(error.to_string()))?
        else {
            return Ok(None);
        };
        let skipped = ignored_directories(&root);
        let before = inventory(&root, &skipped, &reviews, false).map_err(|error| {
            let reason = format!(
                "Command not run. After outside content, Medha keeps a copy of instruction and CI files before each \
                 command, and could not read {error}. Every command will fail the same way until that file can be \
                 read; tell the person which file, do not retry."
            );
            let _ = reviews.refuse(
                format!("{root:?}\0{command}\0{reason}").as_bytes(),
                &root.to_string_lossy(),
                &reason,
                command,
            );
            ExecError::Held(reason)
        })?;
        let digests = before
            .iter()
            .map(|(path, node)| (path.clone(), digest(node)))
            .collect();
        Ok(Some(Self {
            root,
            skipped,
            before,
            digests,
            reviews,
            done: false,
        }))
    }

    pub fn finish(mut self) -> Option<String> {
        let result = self.restore();
        self.done = true;
        result.err().map(|error| format!("Protected content restoration failed: {error}. Repair the checkout before using its execution settings."))
    }

    fn restore(&self) -> Result<(), String> {
        let after_result = inventory(&self.root, &self.skipped, &self.reviews, true);
        let after = after_result.as_ref().ok();
        let mut problem = after_result.as_ref().err().map(ToString::to_string);
        let empty = BTreeMap::new();
        let after = after.unwrap_or(&empty);
        let mut paths = self
            .before
            .keys()
            .chain(after.keys())
            .cloned()
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        let mut changes = Vec::new();
        for path in paths {
            let before = self.before.get(&path).unwrap_or(&MISSING);
            let inspected = match after.get(&path) {
                Some(entry) if *entry != MISSING => Ok(entry.clone()),
                _ => read_entry(&self.root, &path, &mut (BYTES, ENTRIES)),
            };
            let current = match &inspected {
                Ok(entry) => entry,
                Err(error) => {
                    problem.get_or_insert_with(|| error.to_string());
                    &MISSING
                }
            };
            let Some(kind) = self.reviews.classify(&path, true) else {
                continue;
            };
            let before_digest = self
                .digests
                .get(&path)
                .copied()
                .unwrap_or_else(|| digest(&MISSING));
            // A link has no content to weigh, so any change to or from one matters.
            let linked = matches!(before, Entry::Link(_)) || matches!(current, Entry::Link(_));
            if inspected.is_ok()
                && (before_digest == digest(current)
                    || (!linked && !matters(kind, contents(before), contents(current))))
            {
                continue;
            }
            write_entry(&self.root, &path, before).map_err(|error| error.to_string())?;
            changes.push((path, before.clone(), current.clone()));
        }
        if changes.is_empty() && problem.is_none() {
            return Ok(());
        }
        let identity = serde_json::to_vec(&serde_json::json!({"root":self.root,"changes":changes.iter().map(|(path,before,after)|serde_json::json!([path,shown(before),shown(after)])).collect::<Vec<_>>()})).map_err(|error| error.to_string())?;
        let mut preview = String::new();
        for (path, before, after) in &changes {
            preview.push_str(&format!("--- {}\n+++ {}\n", path.display(), path.display()));
            for (prefix, entry) in [("-", before), ("+", after)] {
                for line in
                    String::from_utf8_lossy(&shown(entry).unwrap_or_default()).split_inclusive('\n')
                {
                    preview.push_str(prefix);
                    preview.push_str(line);
                    if !line.ends_with('\n') {
                        preview.push('\n');
                    }
                }
            }
        }
        if let Some(problem) = problem {
            return self.reviews.refuse(&identity, &self.root.to_string_lossy(), &format!("needed protected-content repair; some changed content could not be captured safely: {problem}"), &preview).map(|_| ());
        }
        self.reviews
            .propose(
                &identity,
                &self.root.to_string_lossy(),
                "was restored after a command changed protected content",
                &preview,
                Arc::new(Change {
                    root: self.root.clone(),
                    changes,
                }),
            )
            .map(|_| ())
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.restore();
        }
    }
}

struct Change {
    root: PathBuf,
    changes: Vec<(PathBuf, Entry, Entry)>,
}

#[async_trait::async_trait]
impl kernel::mutations::Apply for Change {
    async fn apply(&self) -> Result<serde_json::Value, String> {
        for (path, before, _) in &self.changes {
            if read_entry(&self.root, path, &mut (BYTES, ENTRIES))
                .map_err(|error| error.to_string())?
                != *before
            {
                return Err(format!(
                    "{} changed since restoration; review a new diff",
                    path.display()
                ));
            }
        }
        for (path, _, after) in &self.changes {
            write_entry(&self.root, path, after).map_err(|error| error.to_string())?;
        }
        Ok(
            serde_json::json!({"applied":true,"files":self.changes.iter().map(|(path,_,_)|path).collect::<Vec<_>>()}),
        )
    }
}

#[cfg(test)]
#[path = "protected_commands_tests.rs"]
mod tests;
