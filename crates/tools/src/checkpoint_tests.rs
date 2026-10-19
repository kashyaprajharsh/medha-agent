use super::*;

fn git(root: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn backups_include_untracked_exclude_ignored_and_preserve_later_human_work() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(root.join("tracked"), "original").unwrap();
    git(&root, &["add", "--", "tracked", ".gitignore"]);
    std::fs::write(root.join("untracked"), "unsaved work").unwrap();
    std::fs::create_dir(root.join("target")).unwrap();
    std::fs::write(root.join("target/cache"), "ignored").unwrap();
    let sandbox = Arc::new(
        WorkspaceSandbox::new_jailed(&root)
            .unwrap()
            .with_snapshots_dir(
                scratch
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("state/snapshots"),
            ),
    );
    let guard = Arc::new(kernel::egress::Guard::default());
    let (ignored, _) = kernel::egress::scope(
        guard.clone(),
        true,
        prepare(
            &sandbox,
            &json!({"command":"rm -rf target","network":false}),
        ),
    )
    .await;
    assert!(ignored.unwrap().is_null());
    for command in ["cat < tracked", "cargo test 2>/dev/null", "rm --version"] {
        let (plain, _) = kernel::egress::scope(
            guard.clone(),
            false,
            prepare(&sandbox, &json!({"command":command,"network":false})),
        )
        .await;
        assert!(
            plain.unwrap().is_null(),
            "{command} creates no needed backup"
        );
    }
    assert!(!sandbox.snapshots_dir().exists());
    assert!(guard.mutations.notices().is_empty());
    let (prepared, _) = kernel::egress::scope(
        guard.clone(),
        true,
        prepare(
            &sandbox,
            &json!({"command":"rm tracked untracked","network":false}),
        ),
    )
    .await;
    let prepared = prepared.unwrap();
    assert_eq!(prepared["_checkpoint_files"].as_array().unwrap().len(), 3);
    let registry = orchestrator::NarrowedExecutor::new(
        Arc::new(ToolRegistry::with_workspace(
            sandbox.clone(),
            Arc::new(store::FileArtifactStore::open(scratch.path().join("artifacts")).unwrap()),
        )),
        None,
    );
    let ignored = ToolIntent {
        id: "ignored-after-checkpoint".into(),
        tool: "shell.exec".into(),
        args: json!({"command":"rm -rf target","network":false}),
    };
    let (ignored_result, _) = kernel::egress::scope(guard.clone(), false, async {
        let preparation = registry.prepare_effect(&ignored).await.unwrap();
        assert!(preparation.is_null());
        let mut observation = registry.execute(&ignored).await;
        registry
            .finish_effect(&ignored, &preparation, &mut observation)
            .await;
        observation
    })
    .await;
    assert_eq!(ignored_result.payload["exit_code"], 0);
    assert!(ignored_result.payload.get("checkpoint").is_none());
    assert!(!root.join("target").exists());
    assert!(guard.mutations.notices().is_empty());
    std::fs::remove_file(root.join("tracked")).unwrap();
    std::fs::remove_file(root.join("untracked")).unwrap();
    std::fs::write(root.join("created"), "command output").unwrap();
    let mut observation = Observation::ok(
        "command",
        json!({"command":"rm tracked untracked","exit_code":0}),
    );
    let destructive = ToolIntent {
        id: "command".into(),
        tool: "shell.exec".into(),
        args: json!({"command":"rm tracked untracked","network":false}),
    };
    kernel::egress::scope(guard.clone(), true, async {
        let child_preparation = registry.prepare_effect(&destructive).await.unwrap();
        assert_eq!(child_preparation["checkpoint"], prepared["checkpoint"]);
        registry
            .finish_effect(&destructive, &child_preparation, &mut observation)
            .await;
    })
    .await;
    let rows = observation.payload["_checkpoint_files"].as_array().unwrap();
    let files: Vec<_> = rows
        .iter()
        .filter(|row| {
            row["path"].as_str().unwrap().ends_with("tracked")
                || row["path"].as_str().unwrap().ends_with("created")
        })
        .map(|row| kernel::events::FileRollback {
            path: row["path"].as_str().unwrap().into(),
            snapshot: row["snapshot"].as_str().map(str::to_owned),
            expected: row["expected"].as_str().map(str::to_owned),
        })
        .collect();
    sandbox.restore_plan(&files).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("tracked")).unwrap(),
        "original"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("untracked")).unwrap(),
        "unsaved work"
    );
    assert!(!root.join("created").exists());
    std::fs::write(root.join("tracked"), "human later").unwrap();
    assert!(
        sandbox
            .restore_revision(
                files
                    .iter()
                    .find(|file| Path::new(&file.path).ends_with("tracked"))
                    .unwrap()
            )
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("tracked")).unwrap(),
        "human later"
    );
    let (again, _) = kernel::egress::scope(
        guard.clone(),
        false,
        prepare(&sandbox, &json!({"command":"rm untracked","network":false})),
    )
    .await;
    assert_eq!(again.unwrap()["checkpoint"], prepared["checkpoint"]);
}

#[cfg(unix)]
#[tokio::test]
async fn git_pathspec_magic_cannot_hide_a_literal_destructive_target() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join(":(exclude)*"), "must be backed up").unwrap();
    git(&root, &["--literal-pathspecs", "add", "--", ":(exclude)*"]);
    let sandbox = Arc::new(
        WorkspaceSandbox::new_jailed(&root)
            .unwrap()
            .with_snapshots_dir(scratch.path().canonicalize().unwrap().join("snapshots")),
    );
    let guard = Arc::new(kernel::egress::Guard::default());
    let (prepared, _) = kernel::egress::scope(
        guard,
        false,
        prepare(
            &sandbox,
            &json!({"command":"rm -- ':(exclude)*'","network":false}),
        ),
    )
    .await;
    let prepared = prepared.unwrap();
    assert!(prepared["checkpoint"].is_string());
    assert_eq!(
        prepared["_checkpoint_files"][0]["path"],
        root.canonicalize()
            .unwrap()
            .join(":(exclude)*")
            .to_string_lossy()
            .as_ref()
    );
    for flag in ["-t../outside", "-it../outside", "--target-directory=.."] {
        let guard = Arc::new(kernel::egress::Guard::default());
        let (refused, _) = kernel::egress::scope(
            guard.clone(),
            false,
            prepare(
                &sandbox,
                &json!({"command":format!("mv {flag} -- ':(exclude)*'"),"network":false}),
            ),
        )
        .await;
        assert!(refused.is_err(), "outside target {flag} has no backup");
        assert_eq!(guard.mutations.notices().len(), 1);
        assert!(root.join(":(exclude)*").exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn an_unbackable_link_blocks_the_command_with_one_quiet_notice() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(scratch.path().join("secret"), "private").unwrap();
    std::os::unix::fs::symlink(scratch.path().join("secret"), root.join("link")).unwrap();
    let sandbox = Arc::new(
        WorkspaceSandbox::new_jailed(&root)
            .unwrap()
            .with_snapshots_dir(scratch.path().canonicalize().unwrap().join("snapshots")),
    );
    let guard = Arc::new(kernel::egress::Guard::default());
    let (result, _) = kernel::egress::scope(
        guard.clone(),
        true,
        prepare(&sandbox, &json!({"command":"rm link","network":false})),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(guard.mutations.notices().len(), 1);
    assert!(guard.mutations.notices().is_empty());
    assert_eq!(
        std::fs::read_to_string(scratch.path().join("secret")).unwrap(),
        "private"
    );
}

#[tokio::test]
async fn checkout_store_retention_expires_old_backups_and_byte_failure_is_quiet() {
    let scratch = tempfile::tempdir().unwrap();
    let base = scratch.path().canonicalize().unwrap();
    let root = base.join("repo");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("note"), "first").unwrap();
    let sandbox = Arc::new(
        WorkspaceSandbox::new_jailed(&root)
            .unwrap()
            .with_snapshots_dir(base.join("legacy"))
            .with_command_snapshots_dir(base.join("checkout-store")),
    );
    let guard = Arc::new(kernel::egress::Guard::default());
    guard
        .mutations
        .configure(kernel::mutations::Settings {
            checkpoint_count: 1,
            checkpoint_bytes: 64,
            ..Default::default()
        })
        .unwrap();
    let (first, _) = kernel::egress::scope(
        guard.clone(),
        false,
        prepare(&sandbox, &json!({"command":"rm note","network":false})),
    )
    .await;
    let first = first.unwrap();
    let snapshot = first["_checkpoint_files"][0]["snapshot"].as_str().unwrap();
    assert!(snapshot.starts_with("command:"));
    guard.mutations.begin_message();
    std::fs::write(root.join("note"), "second").unwrap();
    let (second, _) = kernel::egress::scope(
        guard.clone(),
        false,
        prepare(&sandbox, &json!({"command":"rm note","network":false})),
    )
    .await;
    assert!(second.is_ok());
    assert_eq!(
        std::fs::read_dir(sandbox.command_snapshots_dir().join("commands"))
            .unwrap()
            .count(),
        1
    );
    assert!(
        sandbox
            .restore_plan(&[kernel::events::FileRollback {
                path: root.join("note").to_string_lossy().into_owned(),
                snapshot: Some(snapshot.into()),
                expected: None
            }])
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("note")).unwrap(),
        "second"
    );
    guard.mutations.begin_message();
    guard
        .mutations
        .configure(kernel::mutations::Settings {
            checkpoint_bytes: 1,
            ..Default::default()
        })
        .unwrap();
    let (failed, _) = kernel::egress::scope(
        guard.clone(),
        false,
        prepare(&sandbox, &json!({"command":"rm note","network":false})),
    )
    .await;
    assert!(failed.is_err());
    assert_eq!(guard.mutations.notices().len(), 1);
    assert!(root.join("note").exists());
}

#[test]
#[ignore = "release measurement against the real checkout"]
fn release_checkpoint_cost() {
    assert!(!std::hint::black_box(cfg!(debug_assertions)));
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for label in ["cold", "warm"] {
        let state = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(
            WorkspaceSandbox::new_jailed(&root)
                .unwrap()
                .with_snapshots_dir(state.path().canonicalize().unwrap().join("snapshots")),
        );
        let guard = Arc::new(kernel::egress::Guard::default());
        let started = std::time::Instant::now();
        let (result, _) = runtime.block_on(kernel::egress::scope(
            guard.clone(),
            false,
            prepare(
                &sandbox,
                &json!({"command":"git reset --hard","network":false}),
            ),
        ));
        let prepared_ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut observation = kernel::Observation {
            intent_id: "measure".into(),
            status: kernel::ObsStatus::Ok,
            payload: json!({}),
            media: vec![],
            relayed_trust: None,
            net_denied: false,
            model_text: None,
        };
        let started = std::time::Instant::now();
        runtime.block_on(kernel::egress::scope(
            guard,
            false,
            finish(&sandbox, &mut observation),
        ));
        println!(
            "checkpoint finish {label}: elapsed_ms={:.3}",
            started.elapsed().as_secs_f64() * 1000.0
        );
        let prepared = result.unwrap();
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(
                sandbox
                    .snapshots_dir()
                    .join("commands")
                    .join(prepared["checkpoint"].as_str().unwrap())
                    .join("manifest.json"),
            )
            .unwrap(),
        )
        .unwrap();
        println!(
            "checkpoint {label}: files={}, bytes={}, elapsed_ms={:.3}",
            prepared["_checkpoint_files"].as_array().unwrap().len(),
            manifest["bytes"],
            prepared_ms
        );
    }
}
