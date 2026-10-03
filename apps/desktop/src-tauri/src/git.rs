use std::fs;
use std::path::Path;

/// The checked-out branch for `dir`, or a short commit id when HEAD is detached.
pub fn branch(dir: &Path) -> Option<String> {
    let root = dir.ancestors().find(|path| path.join(".git").exists())?;
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_file() {
        let pointer = fs::read_to_string(&dot_git).ok()?;
        root.join(pointer.strip_prefix("gitdir:")?.trim())
    } else {
        dot_git
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(name) => name.to_owned(),
        None => head.chars().take(7).collect(),
    })
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;

use serde_json::{Value, json};
use std::{
    io::Read,
    process::{Command, Stdio},
};
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;

/// Overrides for every key in the repository's own config that would run a program.
fn config_overrides(dir: &Path) -> Vec<String> {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut overrides = vec![
        "core.fsmonitor=false".to_owned(),
        format!("core.hooksPath={null}"),
    ];
    let Ok(listed) = Command::new("git")
        .args(["config", "--list", "--show-scope", "--name-only", "-z"])
        .current_dir(dir)
        .stderr(Stdio::null())
        .output()
    else {
        return overrides;
    };
    let mut fields = listed.stdout.split(|byte| *byte == 0);
    while let (Some(scope), Some(key)) = (fields.next(), fields.next()) {
        // The user's own global and system filters, such as large-file storage, stay on.
        if !matches!(scope, b"local" | b"worktree") {
            continue;
        }
        let driver = std::str::from_utf8(key)
            .ok()
            .and_then(|key| key.strip_prefix("filter."))
            .and_then(|rest| rest.rsplit_once('.'));
        if let Some((driver, _)) = driver {
            overrides.extend(
                ["clean=", "smudge=", "process=", "required=false"]
                    .map(|setting| format!("filter.{driver}.{setting}")),
            );
        }
    }
    overrides.sort();
    overrides.dedup();
    overrides
}

fn run(dir: &Path, args: &[&str]) -> Result<(i32, Vec<u8>), String> {
    let overrides = config_overrides(dir);
    let mut child = Command::new("git")
        .arg("--no-pager")
        .args(overrides.iter().flat_map(|setting| ["-c", setting]))
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .ok_or("Git output unavailable")?
        .take(MAX_OUTPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_OUTPUT {
        let _ = child.kill();
        let _ = child.wait();
        return Err(
            "This Git result exceeds the 4 MB preview limit. Use the terminal to review it.".into(),
        );
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    Ok((status.code().unwrap_or(-1), bytes))
}
fn repository(dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
    let (code, bytes) = run(dir, &["rev-parse", "--show-toplevel"])?;
    if code != 0 {
        return Ok(None);
    }
    let root = std::path::PathBuf::from(
        String::from_utf8(bytes)
            .map_err(|e| e.to_string())?
            .trim_end_matches(['\r', '\n']),
    );
    root.canonicalize().map(Some).map_err(|e| e.to_string())
}
/// Changes under `dir`. A folder that is not itself a repository, such as one
/// holding several projects, reports the repositories directly inside it.
pub fn status(dir: &Path) -> Result<Value, String> {
    if !dir.exists() {
        return Ok(json!({"repository": false, "files": []}));
    }
    if repository(dir)?.is_some() {
        return Ok(json!({"repository": true, "files": entries(dir, dir)?}));
    }
    let mut files = Vec::new();
    let mut found = false;
    for repo in nested_repositories(dir)? {
        found = true;
        files.extend(entries(&repo, dir)?);
    }
    Ok(json!({"repository": found, "files": files}))
}

const NESTED_LIMIT: usize = 16;

fn nested_repositories(dir: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    let mut children: Vec<_> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.join(".git").exists()
                && !path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        })
        .collect();
    children.sort();
    children.truncate(NESTED_LIMIT);
    Ok(children
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(dir))
        .collect())
}

/// A relative path as Git writes it: forward slashes on every system, so the panel
/// and the diff lookup see the same string on Windows as everywhere else.
fn slash(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// One repository's changes, with paths relative to `base` and the repository
/// they belong to, so a diff can be read from the right place.
fn entries(repo_dir: &Path, base: &Path) -> Result<Vec<Value>, String> {
    let Some(root) = repository(repo_dir)? else {
        return Ok(Vec::new());
    };
    let repo = repo_dir.strip_prefix(base).map(slash).unwrap_or_default();
    let (code, bytes) = run(
        repo_dir,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            // A nested repository's own filters would run while Git checks it for edits.
            "--ignore-submodules=dirty",
            "--",
            ".",
        ],
    )?;
    if code != 0 {
        return Err("Could not read Git status".into());
    }
    let mut records = bytes.split(|b| *b == 0).filter(|r| !r.is_empty());
    let mut files = Vec::new();
    while let Some(record) = records.next() {
        if record.len() < 4 {
            return Err("Invalid Git status entry".into());
        }
        let index = record[0] as char;
        let working = record[1] as char;
        let git_path =
            std::str::from_utf8(&record[3..]).map_err(|_| "Git filename is not valid UTF-8")?;
        let original = if index == 'R' || index == 'C' || working == 'R' || working == 'C' {
            records.next().and_then(|b| std::str::from_utf8(b).ok())
        } else {
            None
        };
        let absolute = root.join(git_path);
        let Ok(path) = absolute.strip_prefix(base) else {
            continue;
        };
        files.push(json!({"path": slash(path), "repo": repo, "index": index.to_string(), "working": working.to_string(), "untracked": index == '?', "original": original.and_then(|p| root.join(p).strip_prefix(base).ok().map(slash))}));
    }
    Ok(files)
}
pub fn diff(dir: &Path, path: &str, staged: bool) -> Result<Value, String> {
    let changes = status(dir)?;
    let entry = changes["files"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["path"] == path))
        .ok_or("This file no longer has changes. Refresh the list.")?;
    let untracked = entry["untracked"] == true;
    let repo = entry["repo"].as_str().unwrap_or_default();
    let repo_dir = dir.join(repo);
    let inside = |path: &str| -> String {
        Path::new(path)
            .strip_prefix(repo)
            .map(slash)
            .unwrap_or_else(|_| path.to_owned())
    };
    let (local, original) = (inside(path), entry["original"].as_str().map(inside));
    let mut args = vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv"];
    if untracked {
        crate::files::resolve(dir, path)?;
        args.extend([
            "--no-index",
            "--",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
            &local,
        ]);
    } else {
        args.extend(["--relative", "--ignore-submodules=dirty"]);
        if staged {
            args.push("--cached");
        }
        args.extend(["--", &local]);
        if let Some(original) = &original {
            args.push(original);
        }
    }
    let (code, bytes) = run(&repo_dir, &args)?;
    if code != 0 && !(untracked && code == 1) {
        return Err("Could not read this Git diff".into());
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(json!({"text": text, "staged": staged, "untracked": untracked}))
}

#[cfg(test)]
mod working_tests {
    use super::*;
    #[test]
    fn shows_untracked_staged_unstaged_and_deleted_files() {
        let root = std::env::temp_dir().join(format!("medha-git-preview-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        let root = root.canonicalize().unwrap();
        assert_eq!(status(&root).unwrap()["repository"], false);
        assert_eq!(run(&root, &["init", "-q"]).unwrap().0, 0);
        fs::write(root.join("sub/new report.md"), "# report\n").unwrap();
        let rows = status(&root).unwrap();
        assert_eq!(rows["files"][0]["path"], "sub/new report.md");
        assert!(
            diff(&root, "sub/new report.md", false).unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("+# report")
        );
        assert_eq!(
            run(&root, &["add", "--", "sub/new report.md"]).unwrap().0,
            0
        );
        assert!(
            diff(&root, "sub/new report.md", true).unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("+# report")
        );
        fs::write(root.join("sub/new report.md"), "# amended\n").unwrap();
        assert!(
            diff(&root, "sub/new report.md", false).unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("+# amended")
        );
        let sub = root.join("sub").canonicalize().unwrap();
        assert_eq!(status(&sub).unwrap()["files"][0]["path"], "new report.md");
        fs::remove_file(root.join("sub/new report.md")).unwrap();
        assert!(
            diff(&root, "sub/new report.md", false).unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("-# report")
        );
        assert!(diff(&root, "../escape", false).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_folder_of_projects_shows_the_changes_in_each_repository_inside_it() {
        let parent = std::env::temp_dir().join(format!("medha-git-nested-{}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir_all(parent.join("antarman/docs")).unwrap();
        fs::create_dir_all(parent.join("notes")).unwrap();
        let parent = parent.canonicalize().unwrap();
        let repo = parent.join("antarman");
        assert_eq!(run(&repo, &["init", "-q"]).unwrap().0, 0);
        fs::write(repo.join("docs/spec.json"), "{}\n").unwrap();
        let rows = status(&parent).unwrap();
        assert_eq!(rows["repository"], true);
        assert_eq!(rows["files"][0]["path"], "antarman/docs/spec.json");
        assert_eq!(rows["files"][0]["repo"], "antarman");
        assert!(
            diff(&parent, "antarman/docs/spec.json", false).unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("+{}")
        );
        fs::remove_dir_all(parent).unwrap();
    }
}
