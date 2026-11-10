//! Deleting one chat: hidden at once, undone for a while, then gone, and nothing else with it.
use super::*;

/// The folder's chats as the desktop lists them, as the terminal lists another folder's, and as an open chat lists its own.
async fn listed(client: &mut Client, folder: &Path, open: &str) -> [String; 3] {
    let sessions = json!({"resource": "sessions"});
    let desktop = client
        .about(folder, json!({"method": "sessions.list"}))
        .await
        .unwrap();
    let elsewhere = client
        .about(
            folder,
            json!({"method": "application.resources", "params": sessions}),
        )
        .await
        .unwrap();
    let here = client.ask("session.resources", Some(open), sessions).await;
    [
        desktop.to_string(),
        elsewhere.to_string(),
        here["result"].to_string(),
    ]
}

/// A chat is deleted only when closed everywhere, can be undone, and then goes while the folder's other chat works on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_chat_can_be_undone_and_then_goes_alone() {
    let world = World::new();
    let folder = world.folder("deleting");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let gone = client.open(&folder).await;
    client.send(&gone, "the chat to delete").await;
    client.until(|frame| kind(frame, "turn.done")).await;
    let kept = client.open(&folder).await;
    client.send(&kept, "the chat to keep").await;
    client.until(|frame| kind(frame, "turn.done")).await;
    let delete = json!({"method": "chats.delete", "params": {"session": gone}});
    let refused = client.about(&folder, delete.clone()).await.unwrap_err();
    assert!(refused.contains("open"), "{refused}");

    client.ask("session.close", Some(&gone), json!({})).await;
    client
        .until(|frame| frame["method"] == "session.ended")
        .await;
    assert!(
        listed(&mut client, &folder, &kept)
            .await
            .iter()
            .all(|list| list.contains(&gone))
    );
    client.about(&folder, delete.clone()).await.unwrap();
    for list in listed(&mut client, &folder, &kept).await {
        assert!(
            !list.contains(&gone),
            "a deleted chat is still listed: {list}"
        );
    }
    let reopened = client.create(&folder, Some(&gone)).await;
    assert!(
        reopened.get("error").is_some(),
        "a chat being deleted was opened: {reopened}"
    );
    let undo = json!({"method": "chats.restore", "params": {"session": gone}});
    client.about(&folder, undo).await.unwrap();
    for list in listed(&mut client, &folder, &kept).await {
        assert!(list.contains(&gone), "undo did not bring it back: {list}");
    }

    client.about(&folder, delete).await.unwrap();
    let events = json!({"method": "sessions.events", "session_id": gone});
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match client.about(&folder, events.clone()).await {
            Err(error) if error.contains("not found") => break,
            left => assert!(
                Instant::now() < deadline,
                "the chat was never deleted: {left:?}"
            ),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(listed(&mut client, &folder, &kept).await[0].contains(&kept));
    client.send(&kept, "still here").await;
    client.until(|frame| kind(frame, "turn.done")).await;
}
