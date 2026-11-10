//! A pin is kept by Medha, not by one app.
use super::*;

/// Whether the chat is pinned in the folder's list as the desktop reads it, and as the terminal does.
async fn pinned(client: &mut Client, folder: &Path, chat: &str) -> [Value; 2] {
    let desktop = client
        .about(folder, json!({"method": "sessions.list"}))
        .await
        .unwrap();
    let sessions = json!({"method": "application.resources", "params": {"resource": "sessions"}});
    let terminal = client.about(folder, sessions).await.unwrap();
    let row = |rows: &Value| {
        rows.as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == chat))
            .map(|row| row["pinned"].clone())
            .unwrap_or(Value::Null)
    };
    [row(&desktop), row(&terminal["data"])]
}

/// A chat pinned from one app is pinned in every app's list, and unpinned the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pin_is_shared_by_every_app_that_lists_the_chat() {
    let world = World::new();
    let folder = world.folder("pins");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    client.send(&chat, "a chat to pin").await;
    client.until(|frame| kind(frame, "turn.done")).await;
    assert_eq!(
        pinned(&mut client, &folder, &chat).await,
        [json!(false), json!(false)]
    );

    let pin = |on: bool| json!({"method": "chats.pin", "params": {"session": chat, "pinned": on}});
    client.about(&folder, pin(true)).await.unwrap();
    assert_eq!(
        pinned(&mut client, &folder, &chat).await,
        [json!(true), json!(true)]
    );
    client.about(&folder, pin(false)).await.unwrap();
    assert_eq!(
        pinned(&mut client, &folder, &chat).await,
        [json!(false), json!(false)]
    );
}
