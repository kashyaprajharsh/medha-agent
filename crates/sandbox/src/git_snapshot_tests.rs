use super::*;

#[test]
fn restoration_removes_planted_programs_and_preserves_normal_git_writes() {
    let scratch = test_support::scratch("medha-git-snapshot");
    let root = scratch.canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
    std::fs::write(root.join(".git/config"), "original config").unwrap();
    let snapshot = Snapshot::new(&root).unwrap();
    std::fs::write(root.join(".git/config"), "planted fsmonitor").unwrap();
    std::fs::write(root.join(".git/commondir"), "planted directory").unwrap();
    std::fs::write(root.join(".git/hooks/pre-commit"), "planted program").unwrap();
    std::fs::write(root.join(".git/index"), "staged bytes").unwrap();
    std::fs::rename(root.join(".git"), root.join("moved")).unwrap();
    std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
    std::fs::write(root.join(".git/hooks/pre-commit"), "replacement program").unwrap();
    assert!(snapshot.restore().unwrap().contains("were restored"));
    assert_eq!(
        std::fs::read_to_string(root.join(".git/config")).unwrap(),
        "original config"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".git/index")).unwrap(),
        "staged bytes"
    );
    assert!(!root.join(".git/hooks/pre-commit").exists());
    assert!(!root.join(".git/commondir").exists());
    assert!(!root.join("moved").exists());
}

#[test]
fn unchanged_controls_do_not_produce_a_notice() {
    let scratch = test_support::scratch("medha-git-snapshot-clean");
    assert!(Snapshot::new(&scratch).unwrap().restore().is_none());
    std::fs::create_dir_all(scratch.join(".git/refs/heads")).unwrap();
    let snapshot = Snapshot::new(&scratch).unwrap();
    std::fs::write(scratch.join(".git/refs/heads/main"), "new reference").unwrap();
    assert!(snapshot.restore().is_none());
    assert!(scratch.join(".git/refs/heads/main").is_file());
}

#[cfg(windows)]
#[tokio::test]
async fn a_real_windows_shell_restores_controls_and_can_stage_and_commit() {
    use crate::exec::{ExecBackend, ExecRequest, HostBackend};
    let scratch = test_support::scratch("medha-windows-git-command");
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&scratch)
            .status()
            .unwrap()
            .success()
    );
    let config = std::fs::read(scratch.join(".git/config")).unwrap();
    std::fs::write(scratch.join("note"), "Windows staged content\n").unwrap();
    let output = HostBackend.run(ExecRequest { program:"cmd.exe".into(), args:vec!["/d".into(), "/s".into(), "/c".into(), "echo planted>.git\\hooks\\pre-commit & git -c core.hooksPath=NUL add note && git -c core.hooksPath=NUL -c user.name=Test -c user.email=test@example.invalid commit -qm saved".into()], cwd:scratch.to_path_buf(), env:vec![], clear_env:false, read_roots:vec![], write_roots:vec![] }).await.unwrap();
    assert_eq!(output.status, Some(0));
    assert!(String::from_utf8_lossy(&output.stderr).contains("were restored"));
    assert!(!scratch.join(".git/hooks/pre-commit").exists());
    assert_eq!(std::fs::read(scratch.join(".git/config")).unwrap(), config);
    let head = std::process::Command::new("git")
        .args(["show", "HEAD:note"])
        .current_dir(&scratch)
        .output()
        .unwrap();
    assert!(head.status.success());
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).replace("\r\n", "\n"),
        "Windows staged content\n"
    );
}
