use super::*;
use kernel::{ArtifactStore, Event, InMemoryLog};
use std::sync::Arc;

#[tokio::test]
async fn unicode_fragments_keep_a_snapshot_and_expire_abandoned_storage() {
    let root = tempfile::tempdir().unwrap();
    let log = store::SqliteLog::open(root.path().join("events.db")).unwrap();
    let artifacts: Arc<dyn ArtifactStore> =
        Arc::new(store::FileArtifactStore::open(root.path().join("artifacts")).unwrap());
    let session = Session::default();
    let foreign = Session::default();
    let content = "你好 🪕 \"\\\n".repeat(60_000);
    log.append(Event::model_text(&session, &content))
        .await
        .unwrap();
    log.append(Event::model_text(&foreign, "not this conversation"))
        .await
        .unwrap();
    let mut reader = Reader::default();
    let mut page = reader
        .read(&log, &session, protocol::ReadHistory::default(), &artifacts)
        .await
        .unwrap();
    assert!(!page.event_finished);
    assert!(reader.has_cached());
    let mut data = page.data.clone();
    let head = page.through;
    log.append(Event::model_text(&session, "appended after snapshot"))
        .await
        .unwrap();
    while !page.finished {
        assert!(page.data.len() <= 256 * 1024);
        page = reader
            .read(
                &log,
                &session,
                protocol::ReadHistory {
                    after: page.after,
                    through: Some(head),
                    offset: page.offset,
                    conversation: Some(session.id.to_string()),
                },
                &artifacts,
            )
            .await
            .unwrap();
        data.push_str(&page.data);
    }
    let record: protocol::HistoryRecord = serde_json::from_str(&data).unwrap();
    assert_eq!(record.payload["text"], content);
    assert!(!reader.has_cached());
    let first = reader
        .read(&log, &session, protocol::ReadHistory::default(), &artifacts)
        .await
        .unwrap();
    assert!(!first.event_finished);
    reader.expire(std::time::Instant::now() + std::time::Duration::from_secs(61));
    assert!(!reader.has_cached());
    assert!(
        reader
            .read(
                &log,
                &foreign,
                protocol::ReadHistory {
                    conversation: Some(session.id.to_string()),
                    ..Default::default()
                },
                &artifacts
            )
            .await
            .is_err()
    );
    assert!(
        reader
            .read(
                &log,
                &session,
                protocol::ReadHistory {
                    after: u64::MAX,
                    through: Some(head),
                    conversation: Some(session.id.to_string()),
                    offset: 0
                },
                &artifacts
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn history_hides_provider_replay_state_and_resolves_saved_image_bytes() {
    let root = tempfile::tempdir().unwrap();
    let artifacts: Arc<dyn ArtifactStore> =
        Arc::new(store::FileArtifactStore::open(root.path().join("artifacts")).unwrap());
    let hash = artifacts.put(b"saved bytes").unwrap();
    let log = InMemoryLog::default();
    let session = Session::default();
    let mut user = Event::user_message(&session, "an image");
    user.payload["attachments"] = json!([{"mime_type":"image/png", "source":{"kind":"artifact","value":hash},
        "provider_state":[{"protocol":"test","data":"private replay state"}]}]);
    log.append(user).await.unwrap();
    let mut reader = Reader::default();
    let first = reader
        .read(&log, &session, protocol::ReadHistory::default(), &artifacts)
        .await
        .unwrap();
    assert!(!first.data.contains("private replay state"));
    let record: protocol::HistoryRecord = serde_json::from_str(&first.data).unwrap();
    assert_eq!(record.payload["attachments"][0]["source"]["kind"], "base64");
    assert_eq!(
        record.payload["attachments"][0]["source"]["value"],
        "c2F2ZWQgYnl0ZXM="
    );
    let mut canonical = Event::model_text(&session, "unused");
    canonical.kind = EventKind::ModelMessage;
    canonical.payload = json!({"role":"assistant", "private":"private replay state",
        "parts":[{"type":"text","text":"visible","provider_state":[{"data":"private replay state"}]},
            {"type":"reasoning","provider_state":[{"data":"private replay state"}]}]});
    log.append(canonical).await.unwrap();
    let page = reader
        .read(&log, &session, protocol::ReadHistory::default(), &artifacts)
        .await
        .unwrap();
    let page = reader
        .read(
            &log,
            &session,
            protocol::ReadHistory {
                after: page.after,
                through: None,
                offset: 0,
                conversation: None,
            },
            &artifacts,
        )
        .await;
    assert!(
        page.is_err(),
        "a nonzero read must carry its established snapshot"
    );
    let (_, event) = log
        .checked_history_record(session.id, 1, None, false)
        .await
        .unwrap();
    assert_eq!(event.unwrap().1.kind, EventKind::ModelMessage);
    let page = reader
        .read(
            &log,
            &session,
            protocol::ReadHistory {
                after: 1,
                through: Some(2),
                offset: 0,
                conversation: Some(session.id.to_string()),
            },
            &artifacts,
        )
        .await
        .unwrap();
    assert!(!page.data.contains("private replay state"));
    assert!(page.data.contains("visible"));
}
