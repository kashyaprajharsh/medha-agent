//! What a backend clears when it opens a project: old stored results past their limit, and old edit backups.
use super::stored::store;
#[cfg(unix)]
use super::stored::{PNG, result};
use super::*;

/// Sets a file's modification time `days` back, as if it had been written then.
#[cfg(unix)]
fn age(path: &Path, days: u64) {
    let then = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(then)
        .unwrap();
}

/// Waits for a condition the backend meets in the background, failing after a while.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("{what} never happened");
}

/// Past its size limit, the store drops text results stored over 30 days ago, oldest first, and keeps recent
/// ones and every image; reading a dropped one says so.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_store_drops_old_text_results_and_keeps_recent_ones_and_images() {
    let world = World::new();
    std::fs::create_dir_all(world.home()).unwrap();
    std::fs::write(
        world.home().join("config.toml"),
        "[security]\nstored_bytes = 200000\n",
    )
    .unwrap();
    let folder = world.folder("project");
    std::fs::write(folder.join("dot.png"), PNG).unwrap();
    let (old, recent, image) = {
        let backend = world.backend();
        let mut client = backend.connect().await;
        let chat = client.open(&folder).await;
        let lines = [
            format!(
                "shell_exec {}",
                json!({"command": "seq 1 60000", "network": false})
            ),
            format!("read {}", json!({"path": "dot.png"})),
        ];
        scripted(&mut client, &world, &chat, &lines).await;
        let kept = |test: &dyn Fn(&[u8]) -> bool| {
            std::fs::read_dir(store(&world))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| std::fs::read(path).is_ok_and(|bytes| test(&bytes)))
                .unwrap()
        };
        let old = kept(&|bytes| bytes.starts_with(b"$ seq 1 60000"));
        let image = kept(&|bytes| bytes.starts_with(&PNG[..8]));
        let more = format!(
            "shell_exec {}",
            json!({"command": "seq 1 50000", "network": false})
        );
        scripted(&mut client, &world, &chat, &[more]).await;
        let recent = kept(&|bytes| bytes.starts_with(b"$ seq 1 50000"));
        (old, recent, image)
    };
    age(&old, 40);
    age(&image, 40);
    // A new backend sweeps the store once, when it first opens the project.
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    eventually("the old result being dropped", || !old.exists()).await;
    assert!(recent.exists(), "a result stored in the last 30 days stays");
    assert!(image.exists(), "an image is never dropped");
    let read = json!({"path": old});
    scripted(&mut client, &world, &chat, &[format!("read {read}")]).await;
    let said = result(&world, &chat, 0).await.to_string();
    assert!(
        said.contains("cleaned up") && said.contains("Run the command again"),
        "{said}"
    );
}

/// Edit backups past the backup allowance go oldest first, as command backups do.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edit_backups_past_their_allowance_go_oldest_first() {
    let world = World::new();
    std::fs::create_dir_all(world.home()).unwrap();
    std::fs::write(
        world.home().join("config.toml"),
        "[security]\ncheckpoint_bytes = 150000\n",
    )
    .unwrap();
    let folder = world.folder("project");
    let page = "x".repeat(100_000);
    std::fs::write(folder.join("notes.txt"), format!("{page}\n")).unwrap();
    let backups = |world: &World| -> Vec<PathBuf> {
        let folder = store(world).parent().unwrap().join("snapshots");
        let mut found: Vec<PathBuf> = std::fs::read_dir(folder)
            .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default();
        found.sort();
        found
    };
    let first = {
        let backend = world.backend();
        let mut client = backend.connect().await;
        let chat = client.open(&folder).await;
        let lines = [
            format!("read {}", json!({"path": "notes.txt"})),
            format!(
                "edit {}",
                json!({"path": "notes.txt", "old_string": "xxxx\n", "new_string": "xxxy\n"})
            ),
            format!(
                "edit {}",
                json!({"path": "notes.txt", "old_string": "xxxy\n", "new_string": "xxyy\n"})
            ),
        ];
        scripted(&mut client, &world, &chat, &lines).await;
        let found = backups(&world);
        assert_eq!(found.len(), 2, "one backup per edit: {found:?}");
        found[0].clone()
    };
    let backend = world.backend();
    let mut client = backend.connect().await;
    client.open(&folder).await;
    eventually("the oldest backup being dropped", || !first.exists()).await;
    assert_eq!(backups(&world).len(), 1, "the newest backup stays");
}
