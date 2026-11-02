//! Regression tests for bugs found in real use, each recreating the scenario it was found in.
use super::*;
use kernel::EventLog;

/// The chat's events as the backend logged them.
async fn logged(world: &World, chat: &str) -> Vec<kernel::Event> {
    let state = std::fs::read_dir(world.home().join("projects"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join("events.db").is_file())
        .unwrap();
    let log = store::SqliteLog::open(state.join("events.db")).unwrap();
    log.events(chat.parse().unwrap()).await
}

/// A sub-agent transcript of local reads is not labelled as web content.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_only_sub_agent_transcript_does_not_mark_the_chat_as_web() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("inventory");
    std::fs::write(folder.join("notes.txt"), "local only\n").unwrap();
    let chat = client.open(&folder).await;
    let objective = format!("SCRIPT\nread {}", json!({"path": "notes.txt"}));
    let spawn = json!({"name": "inv", "objective": objective, "wait": true});
    let transcript = json!({"action": "transcript", "agent": "inv"});
    scripted(
        &mut client,
        &world,
        &chat,
        &[
            format!("agent_spawn {spawn}"),
            format!("agent {transcript}"),
        ],
    )
    .await;
    let events = logged(&world, &chat).await;
    let read_back = events
        .iter()
        .find(|event| {
            event.kind == kernel::EventKind::ToolObs
                && event.payload["payload"]["agent"] == "inv"
                && event.payload["payload"]["steps"].is_array()
        })
        .unwrap_or_else(|| panic!("no transcript result: {events:?}"));
    assert_ne!(
        read_back.trust,
        kernel::TrustLabel::Web,
        "a transcript of local reads is not web content: {:?}",
        read_back.payload
    );
}
