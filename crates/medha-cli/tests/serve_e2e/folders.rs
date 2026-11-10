//! What the backend does when a folder leaves Medha with its chat history.
use super::*;

/// Kills a one-shot run left holding a chat, whatever the test does next.
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A folder's history is deleted only once no chat in it is open anywhere, and nothing else goes with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folders_history_is_deleted_only_when_no_chat_in_it_is_open_anywhere() {
    let world = World::new();
    let (leaving, staying) = (world.folder("leaving"), world.folder("staying"));
    std::fs::write(leaving.join("notes.txt"), "the user's own file").unwrap();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let kept = client.open(&staying).await;
    client.send(&kept, "a chat in another folder").await;
    client.until(|frame| kind(frame, "turn.done")).await;
    let chat = client.open(&leaving).await;
    client.send(&chat, "a chat in the folder").await;
    client.until(|frame| kind(frame, "turn.done")).await;

    let history = client
        .about(&leaving, json!({"method": "folder.history"}))
        .await
        .unwrap();
    assert_eq!(history["chats"], 1, "{history}");
    assert!(history["bytes"].as_u64().unwrap() > 0, "{history}");
    let delete = json!({"method": "folder.history.delete"});
    let refused = client.about(&leaving, delete.clone()).await.unwrap_err();
    assert!(refused.contains("open"), "{refused}");

    client.ask("session.close", Some(&chat), json!({})).await;
    client
        .until(|frame| frame["method"] == "session.ended")
        .await;
    let mut run = Running(
        medha(&world.home(), &world.provider)
            .arg("HOLD a one-shot run")
            .current_dir(&leaving)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    while !world.provider.asked().contains("HOLD a one-shot run") {}
    let refused = client.about(&leaving, delete.clone()).await.unwrap_err();
    assert!(refused.contains("open"), "{refused}");
    run.0.kill().unwrap();
    run.0.wait().unwrap();
    let _ = world.provider.release.send(());

    client.about(&leaving, delete).await.unwrap();
    let listed = client
        .about(&leaving, json!({"method": "sessions.list"}))
        .await
        .unwrap();
    assert!(!listed.to_string().contains(&chat), "{listed}");
    assert_eq!(
        std::fs::read_to_string(leaving.join("notes.txt")).unwrap(),
        "the user's own file"
    );
    let other = client
        .about(&staying, json!({"method": "sessions.list"}))
        .await
        .unwrap();
    assert!(other.to_string().contains(&kept), "{other}");
}
