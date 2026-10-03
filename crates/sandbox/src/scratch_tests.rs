use super::*;

fn base(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("medha-scratch-test-{tag}-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A crashed process leaves its folder behind; a running one must keep its own.
#[test]
fn a_scratch_folder_is_removed_at_exit_and_swept_after_a_crash() {
    let temp = base("life");
    let live = Scratch::create_in(&temp).unwrap();
    assert_eq!(std::fs::read_dir(live.path()).unwrap().count(), 0);
    let root = live.path().parent().unwrap().to_path_buf();
    let crashed = root.join("crashed");
    std::fs::create_dir_all(crashed.join("leftovers")).unwrap();
    std::fs::write(root.join("crashed.lock"), "").unwrap();

    let next = Scratch::create_in(&temp).unwrap();
    assert!(!crashed.exists() && !root.join("crashed.lock").exists());
    assert!(live.path().exists(), "a live process keeps its folder");

    let (path, lock) = (next.path().to_path_buf(), next.lock.clone());
    drop(next);
    assert!(!path.exists() && !lock.exists());
    drop(live);
    std::fs::remove_dir_all(&temp).ok();
}

/// The folder is Medha's own, so neither a file tool nor a jailed command waits for a card.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn scratch_is_open_to_file_tools_and_jailed_commands_without_a_prompt() {
    use crate::{WorkspaceSandbox, exec};
    use permissions::{ApprovedRoots, NetworkGrant};
    use std::sync::Arc;
    if !exec::native_backend_available() {
        return;
    }
    let temp = base("open");
    let (ws, state) = (temp.join("ws"), temp.join("state"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let scratch = Scratch::create_in(&temp).unwrap();
    let approved = ApprovedRoots::default();
    let backend = exec::select_backend(
        &exec::SandboxConfig::default(),
        Vec::new(),
        approved.clone(),
        NetworkGrant::default(),
    );
    let sandbox = WorkspaceSandbox::new_with_roots(
        &ws,
        state.join("trust.lock"),
        state.join("audit.log"),
        Some(Arc::new(kernel::AutoDeny)),
        approved,
    )
    .unwrap()
    .with_exec_backend(backend)
    .with_scratch(scratch.path());

    let inside = scratch.path().canonicalize().unwrap();
    let note = inside.join("note.txt");
    sandbox
        .write(&note.to_string_lossy(), "kept")
        .await
        .unwrap();
    let script = format!(
        "cd {} && mkdir .git && printf x > .git/config && cat note.txt",
        inside.display()
    );
    let output = sandbox
        .exec("/bin/sh", &["-c".into(), script], Vec::new(), false)
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "kept");
    assert!(inside.join(".git/config").exists());

    let beside = temp.join("beside.txt");
    assert!(sandbox.write(&beside.to_string_lossy(), "x").await.is_err());
    drop(scratch);
    assert!(!inside.exists());
    std::fs::remove_dir_all(&temp).ok();
}
