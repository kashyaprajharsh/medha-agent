//! What project memory is rebuilt from: every memory save, in order, and nothing else.
use super::*;
use serde_json::json;

/// The rebuild reads only memory saves, as written and in order, a branch's copies left out, and still checked.
#[tokio::test]
async fn memory_saves_are_read_alone_in_order_and_still_checked() {
    let dir = std::env::temp_dir().join(format!("medha-memory-events-{}", Ulid::new()));
    let db = dir.join("events.db");
    let log = SqliteLog::open(&db).unwrap();
    let chat = kernel::Session::new();
    let save = |name: &str, source: &str| Event {
        kind: EventKind::MemoryWrite,
        payload: json!({"op": "write", "name": name}),
        provenance: Provenance {
            source: source.into(),
        },
        ..Event::model_text(&chat, "")
    };
    log.append(Event::user_message(&chat, "remember things"))
        .await
        .unwrap();
    log.append(save("first", "kernel")).await.unwrap();
    log.append(Event::model_text(&chat, "between"))
        .await
        .unwrap();
    log.append(save("copied", "fork")).await.unwrap();
    log.append(save("second", "kernel")).await.unwrap();

    let wanted: Vec<Event> = log
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::MemoryWrite && event.provenance.source != "fork")
        .collect();
    let read = log.memory_events().unwrap();
    assert_eq!(read.len(), 2);
    assert_eq!(
        read.iter().map(|event| event.id).collect::<Vec<_>>(),
        wanted.iter().map(|event| event.id).collect::<Vec<_>>()
    );
    assert_eq!(read[0].payload["name"], "first");

    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE events SET payload = '{\"op\":\"write\",\"name\":\"forged\"}' WHERE kind = 'memory.write' AND rowid = 2",
            [],
        )
        .unwrap();
    assert!(
        SqliteLog::open(&db).unwrap().memory_events().is_err(),
        "a changed memory save was trusted"
    );
    drop(log);
    std::fs::remove_dir_all(dir).unwrap();
}
