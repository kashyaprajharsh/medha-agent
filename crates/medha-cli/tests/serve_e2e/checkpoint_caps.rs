use super::*;

fn copies(home: &Path) -> Vec<PathBuf> {
    walk(home)
        .into_iter()
        .filter(|path| {
            path.is_file()
                && path.parent().is_some_and(|parent| {
                    parent
                        .file_name()
                        .is_some_and(|name| name == "command-snapshots")
                })
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| ulid::Ulid::from_string(name).is_ok())
        })
        .collect()
}

fn copied_bytes(home: &Path) -> u64 {
    copies(home)
        .iter()
        .map(|path| std::fs::metadata(path).unwrap().len())
        .sum()
}

fn backup_manifests(home: &Path) -> Vec<Value> {
    walk(home)
        .iter()
        .filter(|path| {
            path.file_name().is_some_and(|name| name == "manifest.json")
                && path
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == "commands")
        })
        .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
        .collect()
}

fn settings(world: &World, count: usize, bytes: usize) {
    std::fs::create_dir_all(world.home()).unwrap();
    std::fs::write(
        world.home().join("config.toml"),
        format!("[security]\ncheckpoint_count = {count}\ncheckpoint_bytes = {bytes}\n"),
    )
    .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_cap_backup_store_shrinks_and_the_newest_command_can_be_rewound() {
    let world = World::new();
    settings(&world, 2, 128);
    let folder = world.folder("backup-cap");
    std::fs::write(folder.join("stable.txt"), [b's'; 48]).unwrap();
    std::fs::write(folder.join("recent.txt"), "stage0---\n").unwrap();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    for stage in 1..=3 {
        let args =
            json!({"command":format!("printf 'stage{stage}---\\n' > recent.txt"),"network":false});
        let request = scripted(&mut client, &world, &chat, &[format!("shell_exec {args}")]).await;
        assert!(request.contains("exit code: 0"), "{request}");
    }
    assert_eq!(
        copied_bytes(&world.home()),
        68,
        "two backups share the stable copy"
    );
    drop(client);
    drop(backend);

    settings(&world, 2, 64);
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    let args = json!({"command":"printf 'stage4---\\n' > recent.txt","network":false});
    scripted(&mut client, &world, &chat, &[format!("shell_exec {args}")]).await;
    let kept = copied_bytes(&world.home());
    assert!(kept <= 64, "the 64-byte cap kept {kept} bytes of copies");

    let points = client
        .ask("session.rewind.points", Some(&chat), json!({}))
        .await;
    let rewound = client
        .ask(
            "session.rewind",
            Some(&chat),
            json!({"at":points["result"]["points"][0]["id"],"scope":"code"}),
        )
        .await;
    assert!(rewound.get("error").is_none(), "{rewound}");
    assert_eq!(
        std::fs::read(folder.join("recent.txt")).unwrap(),
        b"stage3---\n"
    );
    assert_eq!(
        std::fs::read(folder.join("stable.txt")).unwrap(),
        [b's'; 48]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_backup_counts_reused_copies_towards_its_allowance() {
    let world = World::new();
    settings(&world, 20, 64);
    let folder = world.folder("incoming-cap");
    std::fs::write(folder.join("stable.txt"), [b's'; 48]).unwrap();
    std::fs::write(folder.join("recent.txt"), "stage0---\n").unwrap();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    for content in ["1234567890123456789", "last"] {
        let args = json!({"command":format!("printf '{content}\\n' > recent.txt"),"network":false});
        scripted(&mut client, &world, &chat, &[format!("shell_exec {args}")]).await;
    }
    let kept = copied_bytes(&world.home());
    assert!(
        kept <= 64,
        "the newest backup alone exceeds the 64-byte cap: {kept}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_over_ten_megabytes_is_skipped_and_left_alone_by_rewind() {
    let world = World::new();
    let folder = world.folder("large-file");
    std::fs::File::create(folder.join("large.bin"))
        .unwrap()
        .set_len(10 * 1024 * 1024 + 1)
        .unwrap();
    std::fs::write(folder.join("note.txt"), "original\n").unwrap();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    let args = json!({"command":"printf 'changed\\n' > large.bin; printf 'changed\\n' > note.txt","network":false});
    let request = scripted(&mut client, &world, &chat, &[format!("shell_exec {args}")]).await;
    let manifests = backup_manifests(&world.home());
    let rows = manifests[0]["files"].as_array().unwrap();
    let row = rows
        .iter()
        .find(|row| row["path"] == json!(folder.join("large.bin")))
        .unwrap();
    assert_eq!(row["skipped"], true, "{row}");
    assert!(row.get("snapshot").is_none(), "{row}");
    assert!(copied_bytes(&world.home()) < 10 * 1024 * 1024);
    assert!(
        request.contains("Not backed up, so undo cannot restore them: large.bin."),
        "{request}"
    );
    let points = client
        .ask("session.rewind.points", Some(&chat), json!({}))
        .await;
    let rewound = client
        .ask(
            "session.rewind",
            Some(&chat),
            json!({"at":points["result"]["points"][0]["id"],"scope":"code"}),
        )
        .await;
    assert!(rewound.get("error").is_none(), "{rewound}");
    assert_eq!(
        std::fs::read(folder.join("note.txt")).unwrap(),
        b"original\n"
    );
    assert_eq!(
        std::fs::read(folder.join("large.bin")).unwrap(),
        b"changed\n"
    );
}

async fn above_file_limit(git: bool) {
    let world = World::new();
    settings(&world, 20, 64);
    let folder = world.folder("many-files");
    std::fs::write(folder.join("marker.txt"), "before\n").unwrap();
    for index in 0..50_000 {
        std::fs::write(folder.join(format!("f{index:05}")), b"x").unwrap();
    }
    if git {
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&folder)
                .status()
                .unwrap()
                .success()
        );
    }
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    let args = json!({"command":"printf 'after\\n' > marker.txt","network":false});
    let request = scripted(&mut client, &world, &chat, &[format!("shell_exec {args}")]).await;
    assert_eq!(
        std::fs::read(folder.join("marker.txt")).unwrap(),
        b"after\n"
    );
    assert!(
        request.contains("No backup was made")
            && (request.contains("50,000 files") || request.contains("50000 files")),
        "no file-count note: {request}"
    );
    assert!(
        backup_manifests(&world.home()).is_empty(),
        "an over-limit workspace was backed up"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_git_workspace_over_fifty_thousand_files_gets_no_backup() {
    above_file_limit(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_git_workspace_over_fifty_thousand_files_gets_no_backup() {
    above_file_limit(true).await;
}
