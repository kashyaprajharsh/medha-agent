use super::*;

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn installer_jail_writes_only_its_prefix_and_does_not_inherit_provider_keys() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().canonicalize().unwrap();
    let prefix = root.join("prefix");
    std::fs::create_dir(&prefix).unwrap();
    let backend = installer_backend(&prefix).unwrap();
    assert_eq!(backend.label(), "native");
    let script = "import os,pathlib,sys\np=pathlib.Path(sys.argv[1])\np.joinpath('inside').write_text('ok')\ntry:\n pathlib.Path(sys.argv[2]).write_text('escaped')\nexcept PermissionError:\n pass\nelse:\n raise SystemExit('outside write allowed')\nassert not any(k in os.environ for k in ['OPENAI_API_KEY','MEDHA_API_KEY','AWS_SECRET_ACCESS_KEY'])\nassert pathlib.Path(os.environ['HOME']).resolve()!=pathlib.Path(sys.argv[3]).resolve()\n";
    let request = ExecRequest {
        program: "python3".into(),
        args: vec![
            "-c".into(),
            script.into(),
            prefix.to_string_lossy().into_owned(),
            root.join("outside").to_string_lossy().into_owned(),
            std::env::var("HOME").unwrap(),
        ],
        cwd: prefix.clone(),
        env: Vec::new(),
        clear_env: true,
        read_roots: Vec::new(),
        write_roots: Vec::new(),
    };
    let result = sandbox::exec::run_request_bounded(
        backend.as_ref(),
        &request,
        Duration::from_secs(10),
        4096,
        None,
    )
    .await
    .unwrap();
    assert!(result.passed(), "{}", result.output);
    assert!(prefix.join("inside").exists());
    assert!(!root.join("outside").exists());
}

#[test]
fn installer_recipes_pin_every_requested_package() {
    for server in [
        "typescript-language-server",
        "pyright",
        "bash-language-server",
        "yaml-language-server",
        "intelephense",
        "gopls",
    ] {
        let recipe = install_recipe(server).unwrap();
        assert!(recipe[1..].iter().all(|package| {
            package
                .rsplit_once('@')
                .is_some_and(|(_, version)| !version.is_empty() && version != "latest")
        }));
    }
}
