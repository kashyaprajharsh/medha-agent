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

fn repo(scratch: &tempfile::TempDir) -> (PathBuf, Arc<WorkspaceSandbox>) {
    let base = scratch.path().canonicalize().unwrap();
    let root = base.join("repo");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    let sandbox = Arc::new(
        WorkspaceSandbox::new_jailed(&root)
            .unwrap()
            .with_snapshots_dir(base.join("state/snapshots"))
            .with_command_snapshots_dir(base.join("state/commands")),
    );
    (root, sandbox)
}

async fn prepared(
    guard: &Arc<kernel::egress::Guard>,
    sandbox: &Arc<WorkspaceSandbox>,
    command: &str,
) -> Value {
    let (result, _) = kernel::egress::scope(
        guard.clone(),
        false,
        prepare(sandbox, &json!({"command":command,"network":false})),
    )
    .await;
    result.expect("a backup never refuses the command")
}

fn rows(guard: &kernel::egress::Guard, sandbox: &WorkspaceSandbox) -> Vec<Value> {
    guard.mutations.checkpoint(sandbox.root()).unwrap()["_checkpoint_files"]
        .as_array()
        .unwrap()
        .clone()
}

/// A backup covers tracked and untracked work, skips ignored files, and undo restores
/// it without overwriting what the person changed afterwards.
#[tokio::test]
async fn backups_include_untracked_exclude_ignored_and_preserve_later_human_work() {
    let scratch = tempfile::tempdir().unwrap();
    let (root, sandbox) = repo(&scratch);
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(root.join("tracked"), "original").unwrap();
    git(&root, &["add", "--", "tracked", ".gitignore"]);
    std::fs::write(root.join("untracked"), "unsaved work").unwrap();
    std::fs::create_dir(root.join("target")).unwrap();
    std::fs::write(root.join("target/cache"), "ignored").unwrap();
    let guard = Arc::new(kernel::egress::Guard::default());
    for read in ["cat < tracked", "rg -n x tracked 2>/dev/null", "git status"] {
        assert!(
            prepared(&guard, &sandbox, read).await.is_null(),
            "{read} only reads"
        );
    }
    let first = prepared(&guard, &sandbox, "rm tracked untracked").await;
    assert!(first["checkpoint"].is_string());
    assert_eq!(
        rows(&guard, &sandbox).len(),
        3,
        "ignored files are not copied"
    );
    let registry = orchestrator::NarrowedExecutor::new(
        Arc::new(ToolRegistry::with_workspace(
            sandbox.clone(),
            Arc::new(store::FileArtifactStore::open(scratch.path().join("artifacts")).unwrap()),
        )),
        None,
    );
    std::fs::remove_file(root.join("tracked")).unwrap();
    std::fs::remove_file(root.join("untracked")).unwrap();
    std::fs::write(root.join("created"), "command output").unwrap();
    let mut observation = Observation::ok("command", json!({"exit_code":0}));
    let command = ToolIntent {
        id: "command".into(),
        tool: "shell.exec".into(),
        args: json!({"command":"rm tracked untracked","network":false}),
    };
    kernel::egress::scope(guard.clone(), true, async {
        let again = registry.prepare_effect(&command).await.unwrap();
        assert_eq!(
            again["checkpoint"], first["checkpoint"],
            "one backup per message"
        );
        registry
            .finish_effect(&command, &again, &mut observation)
            .await;
    })
    .await;
    let changed = observation.payload["_checkpoint_files"].as_array().unwrap();
    assert_eq!(
        changed.len(),
        3,
        "only what changed is recorded: {changed:?}"
    );
    let files: Vec<_> = changed
        .iter()
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
    let tracked = files
        .iter()
        .find(|file| file.path.ends_with("/tracked"))
        .unwrap();
    assert!(sandbox.restore_revision(tracked).await.is_err());
    assert_eq!(
        std::fs::read_to_string(root.join("tracked")).unwrap(),
        "human later"
    );
}

/// The commands the 2026-10-20 chat was refused for now run with a backup.
#[tokio::test]
async fn commands_once_refused_run_with_a_backup() {
    let scratch = tempfile::tempdir().unwrap();
    let (root, sandbox) = repo(&scratch);
    std::fs::create_dir(root.join("work")).unwrap();
    std::fs::write(root.join("work/f.txt"), "filter").unwrap();
    for command in [
        "cd work && ffmpeg -y -i a.gif m.mp4 2>err.log; echo \"exit=$?\"; head -40 err.log",
        "for f in *.png; do cp \"$f\" out/; rm -rf \"$f\"; done",
        "git -Csub reset --hard",
        "printf x > work/f.txt && ffmpeg -y out.mp4 2>&1 | tail -6; echo \"${PIPESTATUS[0]}\"",
    ] {
        let guard = Arc::new(kernel::egress::Guard::default());
        let result = prepared(&guard, &sandbox, command).await;
        assert!(result["checkpoint"].is_string(), "{command}: {result}");
        assert!(
            result.get("_checkpoint_note").is_none(),
            "{command}: {result}"
        );
    }
}

/// A link is never copied or followed for undo; it is named and the command still runs.
#[cfg(unix)]
#[tokio::test]
async fn a_link_is_named_and_never_followed() {
    let scratch = tempfile::tempdir().unwrap();
    let (root, sandbox) = repo(&scratch);
    std::fs::write(scratch.path().join("secret"), "private").unwrap();
    std::os::unix::fs::symlink(scratch.path().join("secret"), root.join("link")).unwrap();
    std::fs::write(root.join("note"), "kept").unwrap();
    let guard = Arc::new(kernel::egress::Guard::default());
    let result = prepared(&guard, &sandbox, "rm link note").await;
    assert!(result["checkpoint"].is_string());
    assert!(
        result["_checkpoint_note"]
            .as_str()
            .unwrap()
            .contains("link")
    );
    let rows = rows(&guard, &sandbox);
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["path"].as_str().unwrap().ends_with("/note"));
    assert_eq!(
        std::fs::read_to_string(scratch.path().join("secret")).unwrap(),
        "private"
    );
}

/// A later backup reuses the copy of every file that did not change.
#[tokio::test]
async fn unchanged_files_reuse_their_last_copy() {
    let scratch = tempfile::tempdir().unwrap();
    let (root, sandbox) = repo(&scratch);
    std::fs::write(root.join("same"), "same").unwrap();
    std::fs::write(root.join("edited"), "before").unwrap();
    let guard = Arc::new(kernel::egress::Guard::default());
    prepared(&guard, &sandbox, "make").await;
    let first = rows(&guard, &sandbox);
    guard.mutations.begin_message();
    std::fs::write(root.join("edited"), "after, longer").unwrap();
    prepared(&guard, &sandbox, "make").await;
    let second = rows(&guard, &sandbox);
    let id = |rows: &[Value], name: &str| {
        rows.iter()
            .find(|row| row["path"].as_str().unwrap().ends_with(name))
            .map(|row| row["snapshot"].clone())
            .unwrap()
    };
    assert_eq!(id(&first, "/same"), id(&second, "/same"));
    assert_ne!(id(&first, "/edited"), id(&second, "/edited"));
    let copies = std::fs::read_dir(sandbox.command_snapshots_dir())
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_file())
        .filter(|entry| {
            ulid::Ulid::from_string(entry.as_ref().unwrap().file_name().to_str().unwrap()).is_ok()
        })
        .count();
    assert_eq!(
        copies, 3,
        "two first copies plus one new copy of the edited file"
    );
}

/// A backup still being made elsewhere is waited for, not a refusal.
#[tokio::test]
async fn a_backup_in_progress_is_waited_for() {
    let scratch = tempfile::tempdir().unwrap();
    let (root, sandbox) = repo(&scratch);
    std::fs::write(root.join("note"), "x").unwrap();
    std::fs::create_dir_all(sandbox.command_snapshots_dir()).unwrap();
    let mut other = lock(&sandbox).unwrap();
    let held = other.try_write().unwrap();
    let guard = Arc::new(kernel::egress::Guard::default());
    let waiting = {
        let (guard, sandbox) = (guard.clone(), sandbox.clone());
        tokio::spawn(async move { prepared(&guard, &sandbox, "rm note").await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!waiting.is_finished());
    drop(held);
    assert!(waiting.await.unwrap()["checkpoint"].is_string());
}

/// Retention drops old backups; a file past the byte allowance is named and the command runs.
#[tokio::test]
async fn retention_expires_old_backups_and_an_oversized_file_is_named() {
    let scratch = tempfile::tempdir().unwrap();
    let (root, sandbox) = repo(&scratch);
    std::fs::write(root.join("note"), "first").unwrap();
    let guard = Arc::new(kernel::egress::Guard::default());
    guard
        .mutations
        .configure(kernel::mutations::Settings {
            checkpoint_count: 1,
            checkpoint_bytes: 64,
            ..Default::default()
        })
        .unwrap();
    prepared(&guard, &sandbox, "rm note").await;
    let snapshot = rows(&guard, &sandbox)[0]["snapshot"]
        .as_str()
        .unwrap()
        .to_owned();
    guard.mutations.begin_message();
    std::fs::write(root.join("note"), "second").unwrap();
    prepared(&guard, &sandbox, "rm note").await;
    let kept = std::fs::read_dir(sandbox.command_snapshots_dir().join("commands")).unwrap();
    assert_eq!(kept.count(), 1);
    assert!(
        !sandbox.snapshot_exists(&snapshot),
        "an expired backup's copy is deleted"
    );
    guard.mutations.begin_message();
    guard
        .mutations
        .configure(kernel::mutations::Settings {
            checkpoint_bytes: 1,
            ..Default::default()
        })
        .unwrap();
    std::fs::write(root.join("note"), "third").unwrap();
    let oversized = prepared(&guard, &sandbox, "rm note").await;
    assert!(
        oversized["_checkpoint_note"]
            .as_str()
            .unwrap()
            .contains("note")
    );
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
    let state = tempfile::tempdir().unwrap();
    let sandbox = Arc::new(
        WorkspaceSandbox::new_jailed(&root)
            .unwrap()
            .with_snapshots_dir(state.path().canonicalize().unwrap().join("snapshots")),
    );
    for label in ["cold", "warm"] {
        let guard = Arc::new(kernel::egress::Guard::default());
        let started = std::time::Instant::now();
        let result = runtime.block_on(prepared(&guard, &sandbox, "git reset --hard"));
        let prepared_ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut observation = kernel::Observation::ok("measure", json!({}));
        let started = std::time::Instant::now();
        runtime.block_on(kernel::egress::scope(
            guard.clone(),
            false,
            finish(&sandbox, &mut observation),
        ));
        println!(
            "checkpoint {label}: files={}, prepare_ms={prepared_ms:.3}, finish_ms={:.3}, note={}",
            rows(&guard, &sandbox).len(),
            started.elapsed().as_secs_f64() * 1000.0,
            result["_checkpoint_note"]
        );
    }
}
