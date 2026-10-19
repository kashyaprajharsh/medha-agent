use super::*;

#[tokio::test]
async fn a_subfolder_uses_the_existing_checkout_without_creating_a_nested_one() {
    let scratch = test_support::scratch("medha-git-subfolder");
    let root = scratch.canonicalize().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    let sub = root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("note"), "subfolder content\n").unwrap();
    let config = std::fs::read(root.join(".git/config")).unwrap();
    let backend = select_backend(
        &SandboxConfig::default(),
        vec![root.clone()],
        ApprovedRoots::default(),
        NetworkGrant::default(),
    );
    let output = backend.run(ExecRequest { program:"/bin/sh".into(), args:vec!["-c".into(), "printf planted >> ../.git/config; git add note && git -c core.hooksPath=/dev/null -c user.name=Test -c user.email=test@example.invalid commit -qm saved".into()], cwd:sub.clone(), env:vec![], clear_env:true, read_roots:vec![], write_roots:vec![] }).await.unwrap();
    assert_eq!(
        output.status,
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!sub.join(".git").exists());
    assert_eq!(std::fs::read(root.join(".git/config")).unwrap(), config);
    let saved = std::process::Command::new("git")
        .args(["show", "HEAD:sub/note"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(saved.status.success());
    assert_eq!(saved.stdout, b"subfolder content\n");
}

#[tokio::test]
async fn native_commands_cannot_plant_git_controls_but_can_stage_and_commit() {
    assert!(native_backend_available(), "requires a real native jail");
    let scratch = test_support::scratch("medha-git-boundary");
    let root = scratch.canonicalize().unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(init.status.success());
    let original = std::fs::read(root.join(".git/config")).unwrap();
    let had_commondir = root.join(".git/commondir").exists();
    std::fs::write(root.join("note"), "real staged content\n").unwrap();
    let backend = select_backend(
        &SandboxConfig::default(),
        vec![],
        ApprovedRoots::default(),
        NetworkGrant::default(),
    );
    let output = backend.run(ExecRequest {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "printf planted >> .git/config; printf planted > .git/commondir; printf planted > .git/hooks/pre-commit; mv .git .moved; git add note && git -c core.hooksPath=/dev/null -c user.name=Test -c user.email=test@example.invalid commit -qm saved && git rev-parse HEAD".into()],
        cwd: root.clone(), env: vec![], clear_env: true, read_roots: vec![], write_roots: vec![],
    }).await.unwrap();
    println!(
        "native Git boundary: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status, Some(0), "legitimate Git stopped working");
    assert_eq!(std::fs::read(root.join(".git/config")).unwrap(), original);
    assert_eq!(root.join(".git/commondir").exists(), had_commondir);
    assert!(!root.join(".git/hooks/pre-commit").exists());
    assert!(!root.join(".moved").exists());
    assert!(root.join(".git/index").is_file());
    let head = std::process::Command::new("git")
        .args(["show", "HEAD:note"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(head.status.success());
    assert_eq!(head.stdout, b"real staged content\n");
}

/// A refusing system must leave the command unrun, including its first write.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn where_the_system_refuses_the_private_view_commands_fail_closed() {
    crate::git_namespace::REFUSED.with(|refused| refused.set(true));
    let scratch = test_support::scratch("medha-git-refused");
    let root = scratch.canonicalize().unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(init.status.success());
    let original = std::fs::read(root.join(".git/config")).unwrap();
    std::fs::write(root.join("note"), "real staged content\n").unwrap();
    let backend = select_backend(
        &SandboxConfig::default(),
        vec![],
        ApprovedRoots::default(),
        NetworkGrant::default(),
    );
    let output = backend.run(ExecRequest {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "printf planted >> .git/config; printf planted > .git/hooks/pre-commit; git add note && git -c core.hooksPath=/dev/null -c user.name=Test -c user.email=test@example.invalid commit -qm saved".into()],
        cwd: root.clone(), env: vec![], clear_env: true, read_roots: vec![], write_roots: vec![],
    }).await;
    crate::git_namespace::REFUSED.with(|refused| refused.set(false));
    assert!(
        output
            .unwrap_err()
            .to_string()
            .contains("No command was run")
    );
    assert_eq!(std::fs::read(root.join(".git/config")).unwrap(), original);
    assert!(!root.join(".git/hooks/pre-commit").exists());
    assert!(
        !root.join(".git/index").exists(),
        "git add must not have run"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_commands_cannot_reenter_a_mount_namespace() {
    let scratch = test_support::scratch("medha-git-namespace");
    let backend = select_backend(
        &SandboxConfig::default(),
        vec![],
        ApprovedRoots::default(),
        NetworkGrant::default(),
    );
    let output = backend
        .run(ExecRequest {
            program: "/usr/bin/unshare".into(),
            args: vec![
                "-Urnm".into(),
                "/bin/sh".into(),
                "-c".into(),
                "echo escaped > marker".into(),
            ],
            cwd: scratch.to_path_buf(),
            env: vec![],
            clear_env: true,
            read_roots: vec![],
            write_roots: vec![],
        })
        .await
        .unwrap();
    println!(
        "namespace escape: status={:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(output.status, Some(0));
    assert!(!scratch.join("marker").exists());
    assert!(
        !scratch.join(".git").exists(),
        "temporary control mounts were not cleaned"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_preexisting_hardlink_cannot_bypass_the_readonly_mount() {
    let scratch = test_support::scratch("medha-git-alias");
    std::fs::create_dir_all(scratch.join(".git/hooks")).unwrap();
    std::fs::write(scratch.join(".git/config"), "original").unwrap();
    std::fs::hard_link(scratch.join(".git/config"), scratch.join("alias")).unwrap();
    let backend = select_backend(
        &SandboxConfig::default(),
        vec![],
        ApprovedRoots::default(),
        NetworkGrant::default(),
    );
    let result = backend
        .run(ExecRequest {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "echo planted > alias".into()],
            cwd: scratch.to_path_buf(),
            env: vec![],
            clear_env: true,
            read_roots: vec![],
            write_roots: vec![],
        })
        .await;
    assert!(result.unwrap_err().to_string().contains("linked control"));
    assert_eq!(std::fs::read(scratch.join("alias")).unwrap(), b"original");
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires a cached container image and a real Docker daemon"]
async fn container_controls_are_readonly_while_git_can_commit() {
    let image = std::env::var("MEDHA_GIT_TEST_IMAGE").expect("set a cached image with /bin/sh");
    let scratch = test_support::scratch("medha-container-git");
    let root = scratch.canonicalize().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    let original = std::fs::read(root.join(".git/config")).unwrap();
    std::fs::write(root.join("note"), "container staged content\n").unwrap();
    fn writable_fixture(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path).unwrap();
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(if metadata.is_dir() { 0o777 } else { 0o666 }),
        )
        .unwrap();
        if metadata.is_dir() {
            for child in std::fs::read_dir(path).unwrap() {
                writable_fixture(&child.unwrap().path());
            }
        }
    }
    writable_fixture(&root);
    // Use the host's Git and its libraries without downloading or changing the image.
    let runtime = root.join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::copy("/usr/bin/git", runtime.join("git")).unwrap();
    let linked = std::process::Command::new("ldd")
        .arg("/usr/bin/git")
        .output()
        .unwrap();
    assert!(linked.status.success());
    let mut loader = None;
    for token in std::str::from_utf8(&linked.stdout)
        .unwrap()
        .split_whitespace()
        .filter(|value| value.starts_with('/'))
    {
        let path = Path::new(token);
        let name = path.file_name().unwrap();
        std::fs::copy(path, runtime.join(name)).unwrap();
        if name.to_string_lossy().contains("ld-linux") {
            loader = Some(name.to_string_lossy().into_owned());
        }
    }
    let loader = loader.expect("dynamic loader");
    let script = format!(
        "git() {{ /workspace/runtime/{loader} --library-path /workspace/runtime /workspace/runtime/git -c safe.directory=/workspace -c maintenance.auto=false \"$@\"; }}; printf planted >> .git/config; printf planted > .git/commondir; printf planted > .git/hooks/pre-commit; mv .git .moved; git add note && git -c core.hooksPath=/dev/null -c user.name=Test -c user.email=test@example.invalid commit -qm saved && git rev-parse HEAD"
    );
    let backend = ContainerBackend::new(
        "docker".into(),
        image,
        NetPolicy::Deny,
        Some("512m".into()),
        Some(64),
    );
    let output = backend
        .run(ExecRequest {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script],
            cwd: root.clone(),
            env: vec![],
            clear_env: true,
            read_roots: vec![],
            write_roots: vec![],
        })
        .await
        .unwrap();
    println!(
        "container Git boundary: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status, Some(0));
    assert_eq!(std::fs::read(root.join(".git/config")).unwrap(), original);
    assert!(!root.join(".git/commondir").exists());
    assert!(!root.join(".git/hooks/pre-commit").exists());
    assert!(!root.join(".moved").exists());
    let head = std::process::Command::new("git")
        .args(["show", "HEAD:note"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(head.status.success());
    assert_eq!(head.stdout, b"container staged content\n");
}
