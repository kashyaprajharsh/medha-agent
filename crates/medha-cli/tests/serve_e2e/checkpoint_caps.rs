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
