use super::*;

#[test]
fn authorization_and_command_building_share_the_cached_backend() {
    let folder = tempfile::tempdir().unwrap();
    let options = SessionOptions {
        sandbox: Some(sandbox::BackendKind::Host),
        ..Default::default()
    };
    let settings = LiveSettings::new(
        folder.path().to_path_buf(),
        options,
        MedhaLock::default(),
        sandbox::HomeScope::default(),
        sandbox::ApprovedRoots::default(),
        sandbox::NetworkGrant::default(),
        None,
    );
    let backend = settings.backend();
    let exec = settings.exec();
    let intent = ToolIntent {
        id: "read".into(),
        tool: "read".into(),
        args: serde_json::json!({"path":"README.md"}),
    };
    for _ in 0..8 {
        settings.authorize(AutonomyLevel::Careful, &intent, Some(BlastRadius::Read));
        assert_eq!(exec.label(), "host");
        let request = ExecRequest {
            program: "fixture-command-not-executed".into(),
            args: Vec::new(),
            cwd: folder.path().to_path_buf(),
            env: Vec::new(),
            clear_env: true,
            read_roots: Vec::new(),
            write_roots: Vec::new(),
        };
        assert_eq!(
            exec.build_command(&request).unwrap().as_std().get_program(),
            "fixture-command-not-executed"
        );
        assert!(
            Arc::ptr_eq(&backend, &settings.backend()),
            "an authorize or command lookup must not rebuild the backend"
        );
    }
}
