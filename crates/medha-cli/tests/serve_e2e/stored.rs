//! Stored results read back by path: the store folder is open read-only to the model's tools and commands.
use super::*;

/// A 1×1 PNG, so a stored image can be told apart from text by its bytes alone.
const PNG: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// The project's store folder, where large results and images are kept by hash.
fn store(world: &World) -> PathBuf {
    std::fs::read_dir(world.home().join("projects"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join("events.db").is_file())
        .unwrap()
        .join("artifacts")
}

/// Runs a scripted turn, approving whatever it asks, and says how many approvals it asked for.
async fn asked_while(client: &mut Client, chat: &str, lines: &[String]) -> usize {
    client
        .send(chat, &format!("SCRIPT\n{}", lines.join("\n")))
        .await;
    let mut asked = 0;
    loop {
        let (_, frame) = client
            .until(|frame| frame["method"] == "approval" || kind(frame, "turn.done"))
            .await;
        if frame["method"] != "approval" {
            return asked;
        }
        asked += 1;
        let gate = json!({"gate_id": frame["params"]["gate_id"], "approve": true});
        client.ask("approval.respond", Some(chat), gate).await;
    }
}

/// The chat's tool result `back` places from its last one, so 0 is the newest.
async fn result(world: &World, chat: &str, back: usize) -> Value {
    let state = store(world).parent().unwrap().to_path_buf();
    let log = store::SqliteLog::open(state.join("events.db")).unwrap();
    kernel::EventLog::events(&log, chat.parse().unwrap())
        .await
        .into_iter()
        .filter(|event| event.kind == kernel::EventKind::ToolObs)
        .rev()
        .nth(back)
        .unwrap_or_else(|| panic!("no result {back} from the end"))
        .payload
}

/// A large command result is read back by its store path, by `read` and by a sandboxed command, with no
/// approval; writing into the store from a command is refused.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stored_result_is_read_by_path_and_the_store_cannot_be_written() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("project");
    let chat = client.open(&folder).await;
    let big = json!({"command": "seq 1 60000", "network": false});
    scripted(&mut client, &world, &chat, &[format!("shell_exec {big}")]).await;
    let kept = std::fs::read_dir(store(&world))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| std::fs::read_to_string(path).is_ok_and(|text| text.contains("59999")))
        .expect("the large result is in the store");
    let path = kept.display().to_string();
    let read = format!(
        "read {}",
        json!({"path": path, "offset": 59990, "limit": 5})
    );
    assert_eq!(
        asked_while(&mut client, &chat, &[read]).await,
        0,
        "no approval to read the store"
    );
    let read = result(&world, &chat, 0).await;
    assert!(
        read["payload"]["content"].to_string().contains("59991"),
        "{read}"
    );
    let commands = [
        format!(
            "shell_exec {}",
            json!({"command": format!("tail -n 1 {path}"), "network": false})
        ),
        format!(
            "shell_exec {}",
            json!({"command": format!("echo x > {}/planted", store(&world).display()), "network": false})
        ),
    ];
    scripted(&mut client, &world, &chat, &commands).await;
    let tail = result(&world, &chat, 1).await;
    assert!(tail["model_text"].to_string().contains("60000"), "{tail}");
    assert!(
        !store(&world).join("planted").exists(),
        "the store is read-only to commands"
    );
    // Even an approved write from a file tool must not overwrite a stored result.
    let overwrite = json!({"path": kept, "content": "replaced\n"});
    let planted = json!({"path": store(&world).join("planted"), "content": "x\n"});
    let lines = [format!("write {overwrite}"), format!("write {planted}")];
    // The approval card's diff of a 60,000-line file against one line once held this turn for 37 seconds.
    let started = std::time::Instant::now();
    scripted(&mut client, &world, &chat, &lines).await;
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_secs(5),
        "the approval waited on its diff: {took:?}"
    );
    assert!(
        read_to_string(&kept).contains("59999"),
        "the stored result is unchanged"
    );
    assert!(
        !store(&world).join("planted").exists(),
        "the store is read-only to file tools"
    );
    let refused = result(&world, &chat, 0).await.to_string();
    assert!(
        refused.contains("Medha's own project records"),
        "says why, not that it holds keys: {refused}"
    );
}

#[cfg(unix)]
fn read_to_string(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// The note on a result too large to show names the file it is kept in, which `read` and `grep` open by path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_result_note_names_the_stored_file_by_path() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("project");
    let chat = client.open(&folder).await;
    // Over the 1 MB a folder search skips, so only searching the one named file finds it.
    let big = json!({"command": "seq 1 200000", "network": false});
    let seen = scripted(&mut client, &world, &chat, &[format!("shell_exec {big}")]).await;
    let kept = std::fs::read_dir(store(&world))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| std::fs::metadata(path).is_ok_and(|found| found.len() > 1_000_000))
        .expect("the large result is in the store");
    let name = kept.file_name().unwrap().to_string_lossy().into_owned();
    assert!(seen.contains("read path=\\\""), "{seen}");
    assert!(
        seen.contains(&format!("{name}\\\"")),
        "the note names the stored file: {seen}"
    );
    assert!(!seen.contains("read hash="), "no hash to read by: {seen}");
    let search = json!({"pattern": "^199999$", "path": kept});
    let page = json!({"path": kept, "offset": 199990});
    scripted(
        &mut client,
        &world,
        &chat,
        &[format!("grep {search}"), format!("read {page}")],
    )
    .await;
    let found = result(&world, &chat, 1).await;
    assert_eq!(found["payload"]["count"], 1, "{found}");
    // The surfaces name the page after the command that produced it, not the store's file.
    let events = json!({"method": "sessions.events", "session_id": chat, "limit": 200});
    let history = client.about(&folder, events).await.unwrap().to_string();
    assert!(
        history.contains("output of seq 1 200000 · from line 199,990"),
        "{history}"
    );
}

/// An image kept in the store has no extension; `read` by path still shows it as an image.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stored_image_read_by_path_comes_back_as_an_image() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("project");
    std::fs::write(folder.join("dot.png"), PNG).unwrap();
    let chat = client.open(&folder).await;
    scripted(
        &mut client,
        &world,
        &chat,
        &[format!("read {}", json!({"path": "dot.png"}))],
    )
    .await;
    let kept = std::fs::read_dir(store(&world))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| std::fs::read(path).is_ok_and(|bytes| bytes.starts_with(&PNG[..8])))
        .expect("the image is in the store");
    let read = json!({"path": kept});
    assert_eq!(
        asked_while(&mut client, &chat, &[format!("read {read}")]).await,
        0
    );
    let shown = result(&world, &chat, 0).await;
    assert_eq!(shown["status"], "ok", "{shown}");
    assert_eq!(shown["media"][0]["mime_type"], "image/png", "{shown}");
}

/// A chat from before notes named paths says `read hash="…"` with a byte offset; that call still reads the
/// result, though the schema no longer offers `hash`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_chats_read_by_hash_still_reads_the_result() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("project");
    let chat = client.open(&folder).await;
    let big = json!({"command": "seq 1 60000", "network": false});
    let seen = scripted(&mut client, &world, &chat, &[format!("shell_exec {big}")]).await;
    assert!(
        !seen.contains("\\\"hash\\\":{\\\"type"),
        "hash is not offered: {seen}"
    );
    let hash = std::fs::read_dir(store(&world))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .find(|name| name.len() == 64)
        .unwrap();
    let old = json!({"hash": hash, "offset": 0, "length": 8});
    scripted(&mut client, &world, &chat, &[format!("read {old}")]).await;
    let read = result(&world, &chat, 0).await;
    assert_eq!(read["status"], "ok", "{read}");
    assert_eq!(
        read["payload"]["content"], "$ seq 1 ",
        "the first 8 bytes: {read}"
    );
}
