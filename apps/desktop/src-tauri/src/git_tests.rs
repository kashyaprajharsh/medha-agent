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
