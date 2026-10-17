use super::*;

#[tokio::test]
async fn jailed_children_cannot_read_agent_or_unrelated_process_environments() {
    assert!(landlock_supported(), "this attacker test requires Landlock");
    let workspace = test_support::scratch("medha-proc-boundary");
    let canary = format!("proc-canary-{}", ulid::Ulid::new());
    let mut victim = tokio::process::Command::new("/bin/sh")
        .args(["-c", "printf ready; read hold"])
        .env_clear()
        .env("MEDHA_TEST_SECRET", &canary)
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    use tokio::io::AsyncReadExt;
    let mut ready = [0; 5];
    victim
        .stdout
        .take()
        .unwrap()
        .read_exact(&mut ready)
        .await
        .unwrap();
    assert_eq!(&ready, b"ready");
    let victim_path = format!("/proc/{}/environ", victim.id().unwrap());
    let agent_path = format!("/proc/{}/environ", std::process::id());
    let baseline = std::fs::read(&victim_path).unwrap();
    assert!(String::from_utf8_lossy(&baseline).contains(&canary));
    assert!(!std::fs::read(&agent_path).unwrap().is_empty());
    println!("unjailed baseline: same-UID canary and agent environment readable");

    let backend = LandlockBackend::new(NetPolicy::Deny, vec![], ApprovedRoots::default());
    for target in [&victim_path, &agent_path] {
        let output = backend
            .run(ExecRequest {
                program: "/bin/cat".into(),
                args: vec![target.clone()],
                cwd: workspace.to_path_buf(),
                env: vec![],
                clear_env: true,
                read_roots: vec![],
                write_roots: vec![],
            })
            .await
            .unwrap();
        assert_ne!(output.status, Some(0), "environment escaped: {target}");
        assert!(
            output.stdout.is_empty(),
            "environment bytes escaped: {target}"
        );
        println!(
            "jailed read of {target}: status={:?}, stdout={} bytes, {}",
            output.status,
            output.stdout.len(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let own = backend
        .run(ExecRequest {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "cat /proc/self/environ; readlink /proc/self/exe".into(),
            ],
            cwd: workspace.to_path_buf(),
            env: vec![("SAFE_CONTROL".into(), "self-proc-works".into())],
            clear_env: true,
            read_roots: vec![],
            write_roots: vec![],
        })
        .await
        .unwrap();
    assert_eq!(own.status, Some(0));
    assert!(String::from_utf8_lossy(&own.stdout).contains("SAFE_CONTROL=self-proc-works"));
    assert!(!String::from_utf8_lossy(&own.stdout).contains(&canary));
    println!("jailed self environ and executable lookup: allowed");
    victim.kill().await.unwrap();
    victim.wait().await.unwrap();
}
