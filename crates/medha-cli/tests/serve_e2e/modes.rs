//! The mode that runs without asking is Autopilot; the old name yolo still reaches it.
use super::*;

/// An older client that sets yolo gets Autopilot back, and the new name works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn yolo_from_an_older_client_is_autopilot() {
    let world = World::new();
    let folder = world.folder("modes");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    let set = client
        .ask("session.configure", Some(&chat), json!({"mode": "yolo"}))
        .await;
    assert!(
        set.get("error").is_none(),
        "an older client's yolo was refused: {set}"
    );
    let settings = client.ask("session.settings", Some(&chat), json!({})).await;
    assert_eq!(settings["result"]["mode"], "autopilot", "{settings}");
    client
        .ask("session.configure", Some(&chat), json!({"mode": "careful"}))
        .await;
    let set = client
        .ask(
            "session.configure",
            Some(&chat),
            json!({"mode": "autopilot"}),
        )
        .await;
    assert!(set.get("error").is_none(), "autopilot was refused: {set}");
    let settings = client.ask("session.settings", Some(&chat), json!({})).await;
    assert_eq!(settings["result"]["mode"], "autopilot", "{settings}");
}
