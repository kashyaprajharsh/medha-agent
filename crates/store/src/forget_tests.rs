//! Deleting a chat from a log every row of which is chained to the one before.
use super::*;
use serde_json::json;

fn event(session: &kernel::Session, kind: EventKind, payload: serde_json::Value) -> Event {
    Event {
        kind,
        payload,
        ..Event::model_text(session, "")
    }
}

/// A deleted chat takes its sub-agents and the outputs only it used; its memory saves, other chats and every open reader stay sound.
#[tokio::test]
async fn a_deleted_chat_leaves_a_log_every_open_reader_still_trusts() {
    let dir = std::env::temp_dir().join(format!("medha-forget-{}", Ulid::new()));
    let db = dir.join("events.db");
    let log = SqliteLog::open(&db).unwrap();
    let reader = SqliteLog::open(&db).unwrap();
    let (gone, agent, kept) = (
        kernel::Session::new(),
        kernel::Session::new(),
        kernel::Session::new(),
    );
    let (only_gone, shared) = ("a".repeat(64), "b".repeat(64));
    log.append(Event::user_message(&gone, "delete this chat"))
        .await
        .unwrap();
    let spawned = json!({"child": agent.id.to_string(), "agent": "helper"});
    log.append(event(&gone, EventKind::AgentSpawned, spawned))
        .await
        .unwrap();
    log.append(Event::user_message(&agent, "a sub-agent's task"))
        .await
        .unwrap();
    log.append(Event::model_text(
        &gone,
        &format!("saved {only_gone} and {shared}"),
    ))
    .await
    .unwrap();
    let saved = json!({"op": "write", "name": "kept", "scope": "project"});
    log.append(event(&gone, EventKind::MemoryWrite, saved))
        .await
        .unwrap();
    log.append(Event::user_message(&kept, "keep this chat"))
        .await
        .unwrap();
    log.append(Event::model_text(&kept, &format!("also {shared}")))
        .await
        .unwrap();
    reader.all_events().unwrap();

    let family = log.chat_with_agents(gone.id).unwrap();
    assert_eq!(family, vec![gone.id, agent.id]);
    assert!(
        log.forget_chat(gone.id, &[gone.id]).is_err(),
        "a sub-agent the caller did not lock was deleted"
    );
    log.set_pinned(gone.id, true).unwrap();
    let forgotten = log.forget_chat(gone.id, &family).unwrap();
    assert!(
        reader.pinned_chats().unwrap().is_empty(),
        "a deleted chat stayed pinned"
    );
    assert_eq!(forgotten.artifacts, vec![only_gone]);

    for log in [&log, &reader] {
        let listed: Vec<Ulid> = log.list_sessions().unwrap().iter().map(|s| s.id).collect();
        assert_eq!(listed, vec![kept.id], "a deleted chat is still listed");
        let events = log
            .all_events()
            .expect("an open reader stopped trusting the log");
        assert!(
            events.iter().any(|e| e.kind == EventKind::MemoryWrite),
            "the memory save went"
        );
        assert!(!events.iter().any(|e| e.session_id == agent.id));
    }
    assert!(
        reader.search("delete", 10).unwrap().is_empty(),
        "its words are still searchable"
    );
    log.append(Event::model_text(&kept, "later")).await.unwrap();
    reader.all_events().unwrap();
    SqliteLog::open(&db).unwrap().verify().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
