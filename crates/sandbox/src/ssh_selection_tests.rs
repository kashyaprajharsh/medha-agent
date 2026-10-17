use super::*;

#[tokio::test]
async fn an_unconfigured_remote_never_runs_the_payload_locally() {
    let scratch = test_support::scratch("medha-ssh-denial");
    for host in [None, Some(String::new()), Some("   ".into())] {
        let backend = select_backend(
            &SandboxConfig {
                backend: BackendKind::Ssh,
                host,
                ..Default::default()
            },
            vec![],
            ApprovedRoots::default(),
            NetworkGrant::default(),
        );
        let marker = scratch.join("must-not-run");
        let (program, args) = shell_argv(
            backend.label(),
            &format!("echo leaked > {}", shell_quote(&marker.to_string_lossy())),
        );
        let result = backend
            .run(ExecRequest {
                program,
                args,
                cwd: scratch.to_path_buf(),
                env: vec![],
                clear_env: true,
                read_roots: vec![],
                write_roots: vec![],
            })
            .await;
        assert!(matches!(result, Err(ExecError::Unavailable(_))));
        assert!(
            !marker.exists(),
            "remote fallback executed on the local machine"
        );
    }
}
