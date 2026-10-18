//! Protected content is repaired only after the owned process tree has settled.

use crate::{
    exec::ExecError,
    git_snapshot::{Node, read_below, restore_below},
};
use kernel::protected_paths::matters;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    before: BTreeMap<PathBuf, Node>,
    digests: BTreeMap<PathBuf, [u8; 32]>,
    reviews: Arc<kernel::mutations::Guard>,
    done: bool,
}

fn contents(node: &Node) -> Option<&[u8]> {
    if let Node::File(bytes, _) = node {
        Some(bytes)
    } else {
        None
    }
}

fn digest(node: &Node) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update([u8::from(matches!(node, Node::File(..)))]);
    hash.update(contents(node).unwrap_or_default());
    hash.finalize().into()
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
) -> std::io::Result<BTreeMap<PathBuf, Node>> {
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
                let node = match read_below(root, relative, &mut budget) {
                    Ok(node) => node,
                    Err(_) if tolerate_changed_entries => Node::Missing,
                    Err(error) => return Err(error),
                };
                files.insert(relative.to_path_buf(), node);
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
            let reason =
                format!("Protected content could not be backed up; command left unrun: {error}");
            let _ = reviews.refuse(
                format!("{root:?}\0{command}\0{reason}").as_bytes(),
                &root.to_string_lossy(),
                &reason,
                command,
            );
            ExecError::Unavailable(reason)
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
            let before = self.before.get(&path).unwrap_or(&Node::Missing);
            let inspected = match after.get(&path) {
                Some(node) if *node != Node::Missing => Ok(node.clone()),
                _ => read_below(&self.root, &path, &mut (BYTES, ENTRIES)),
            };
            let current = match &inspected {
                Ok(node) => node,
                Err(error) => {
                    problem.get_or_insert_with(|| error.to_string());
                    &Node::Missing
                }
            };
            let Some(kind) = self.reviews.classify(&path, true) else {
                continue;
            };
            let before_digest = self
                .digests
                .get(&path)
                .copied()
                .unwrap_or_else(|| digest(&Node::Missing));
            if inspected.is_ok()
                && (before_digest == digest(current)
                    || !matters(kind, contents(before), contents(current)))
            {
                continue;
            }
            restore_below(&self.root, &path, before).map_err(|error| error.to_string())?;
            changes.push((path, before.clone(), current.clone()));
        }
        if changes.is_empty() && problem.is_none() {
            return Ok(());
        }
        let identity = serde_json::to_vec(&serde_json::json!({"root":self.root,"changes":changes.iter().map(|(path,before,after)|serde_json::json!([path,contents(before),contents(after)])).collect::<Vec<_>>()})).map_err(|error| error.to_string())?;
        let mut preview = String::new();
        for (path, before, after) in &changes {
            preview.push_str(&format!("--- {}\n+++ {}\n", path.display(), path.display()));
            for (prefix, node) in [("-", before), ("+", after)] {
                for line in String::from_utf8_lossy(contents(node).unwrap_or_default())
                    .split_inclusive('\n')
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
    changes: Vec<(PathBuf, Node, Node)>,
}

#[async_trait::async_trait]
impl kernel::mutations::Apply for Change {
    async fn apply(&self) -> Result<serde_json::Value, String> {
        for (path, before, _) in &self.changes {
            if read_below(&self.root, path, &mut (BYTES, ENTRIES))
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
            restore_below(&self.root, path, after).map_err(|error| error.to_string())?;
        }
        Ok(
            serde_json::json!({"applied":true,"files":self.changes.iter().map(|(path,_,_)|path).collect::<Vec<_>>()}),
        )
    }
}

#[cfg(test)]
#[path = "protected_commands_tests.rs"]
mod tests;
