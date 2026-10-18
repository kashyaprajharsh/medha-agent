use super::*;

#[test]
fn rootful_and_rootless_mappings_keep_host_ownership_without_adding_privileges() {
    assert_eq!(
        mapping(false, false, false, 1000, 1001).unwrap(),
        ["--user", "1000:1001"]
    );
    assert_eq!(
        mapping(false, true, false, 1000, 1001).unwrap(),
        ["--user", "0:0"]
    );
    assert_eq!(
        mapping(true, true, false, 1000, 1001).unwrap(),
        ["--user", "1000:1001", "--userns=keep-id"]
    );
    assert!(mapping(false, false, true, 1000, 1001).is_err());
    assert!(mapping(false, false, false, 0, 0).is_err());
    assert!(explicit("--privileged").is_err());
    assert!(explicit("x --privileged").is_err());
    assert!(explicit("").is_err());
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires an existing Linux container image and daemon"]
async fn real_container_preserves_bind_ownership_and_confines_git_controls() {
    use crate::exec::{ContainerBackend, ExecBackend, ExecRequest};
    use std::os::unix::fs::MetadataExt;
    let scratch = test_support::scratch("container-ownership");
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
    let runtime = std::env::var("MEDHA_TEST_CONTAINER_RUNTIME").unwrap_or_else(|_| "docker".into());
    let image =
        std::env::var("MEDHA_TEST_CONTAINER_IMAGE").expect("select an already-present test image");
    let backend =
        ContainerBackend::new(runtime, image, crate::exec::NetPolicy::Deny, None, Some(64));
    let script = "set -eu; if printf planted >>.git/config 2>/dev/null; then exit 41; fi; if touch .git/hooks/planted 2>/dev/null; then exit 42; fi; printf owned > owned; touch .git/index; printf private > \"$HOME/note\"; printf temporary > \"$TMPDIR/note\"; id -u; id -g";
    let request = ExecRequest {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: root.clone(),
        env: Vec::new(),
        clear_env: true,
        read_roots: Vec::new(),
        write_roots: Vec::new(),
    };
    let result = backend.run(request).await.unwrap();
    assert_eq!(
        result.status,
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let metadata = std::fs::metadata(root.join("owned")).unwrap();
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(metadata.gid(), unsafe { libc::getegid() });
    assert_eq!(std::fs::read(root.join(".git/config")).unwrap(), original);
    assert!(!root.join(".git/hooks/planted").exists());
    println!(
        "real container: host_uid={}, file_uid={}, file_gid={}, Git controls readonly, index writable, private HOME/TMP writable",
        unsafe { libc::geteuid() },
        metadata.uid(),
        metadata.gid()
    );
}
