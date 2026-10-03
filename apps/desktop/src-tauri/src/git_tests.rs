use super::branch;
use std::fs;
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("medha-desktop-git-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn reads_the_branch_from_a_subdirectory() {
    let repo = scratch("branch");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/desktop\n").unwrap();
    fs::create_dir_all(repo.join("crates/kernel")).unwrap();
    assert_eq!(
        branch(&repo.join("crates/kernel")).as_deref(),
        Some("feature/desktop")
    );
}

#[test]
fn a_detached_head_shows_a_short_commit() {
    let repo = scratch("detached");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::write(
        repo.join(".git/HEAD"),
        "f93973c1d2e3a4b5c6d7e8f9a0b1c2d3e4f5a6b7\n",
    )
    .unwrap();
    assert_eq!(branch(&repo).as_deref(), Some("f93973c"));
}

#[test]
fn follows_a_worktree_pointer() {
    let repo = scratch("worktree");
    let real = repo.join("real-git");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("HEAD"), "ref: refs/heads/scratch\n").unwrap();
    fs::write(repo.join(".git"), "gitdir: real-git\n").unwrap();
    assert_eq!(branch(&repo).as_deref(), Some("scratch"));
}

#[test]
fn outside_a_repository_there_is_no_branch() {
    let dir = scratch("none");
    assert_eq!(branch(&dir.join("missing-child")), None);
}

/// A sandboxed command can edit a repository's config; the app must not run what it names.
#[cfg(unix)]
#[test]
fn a_repositorys_own_config_cannot_run_a_program() {
    use std::process::Command;
    let root = scratch("hostile").canonicalize().unwrap();
    let (repo, marker) = (root.join("repo"), root.join("ran"));
    let nested = repo.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let git = |dir: &PathBuf, args: &[&str]| {
        let done = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(done.status.success(), "git {args:?}");
    };
    let run = format!("sh -c 'touch {}; cat'", marker.display());
    for (dir, filter) in [(&nested, "inner"), (&repo, "outer")] {
        git(dir, &["init", "-q"]);
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        fs::write(
            dir.join(".gitattributes"),
            format!("a.txt filter={filter}\n"),
        )
        .unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "first"]);
        git(dir, &["config", &format!("filter.{filter}.clean"), &run]);
        git(dir, &["config", "core.fsmonitor", &format!("{run} #")]);
    }
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(nested.join("a.txt"), "b\n").unwrap();
    fs::write(repo.join("a.txt"), "b\n").unwrap();

    git(&repo, &["status", "--porcelain"]);
    assert!(marker.exists(), "the control repository is not hostile");
    fs::remove_file(&marker).unwrap();

    let rows = super::status(&repo).unwrap();
    assert_eq!(rows["files"][0]["path"], "a.txt");
    super::diff(&repo, "a.txt", false).unwrap();
    assert!(!marker.exists(), "the repository's config ran a program");
}
