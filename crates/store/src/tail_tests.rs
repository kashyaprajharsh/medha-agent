//! A turn reads only what was logged since its last read, and gets exactly what a full read would.
use super::*;

/// The tail after a known event is the rest of a full read: nothing missed, nothing twice, another chat's rows left out.
#[tokio::test]
async fn the_tail_after_an_event_is_the_rest_of_a_full_read() {
    let dir = std::env::temp_dir().join(format!("medha-tail-{}", Ulid::new()));
    let log = SqliteLog::open(dir.join("events.db")).unwrap();
    let (chat, other) = (kernel::Session::new(), kernel::Session::new());
    for text in ["one", "two"] {
        log.append(Event::model_text(&chat, text)).await.unwrap();
    }
    let read = log.checked_events(chat.id).await.unwrap();
    log.append(Event::model_text(&other, "elsewhere"))
        .await
        .unwrap();
    for text in ["three", "four"] {
        log.append(Event::model_text(&chat, text)).await.unwrap();
    }

    let tail = log
        .checked_events_after(chat.id, read.last().map(|event| event.id))
        .await
        .unwrap();
    let mut joined = read.clone();
    joined.extend(tail);
    let full = log.checked_events(chat.id).await.unwrap();
    assert_eq!(
        joined.iter().map(|event| event.id).collect::<Vec<_>>(),
        full.iter().map(|event| event.id).collect::<Vec<_>>()
    );
    assert_eq!(
        log.checked_events_after(chat.id, None).await.unwrap().len(),
        4
    );
    assert!(
        log.checked_events_after(chat.id, Some(Ulid::new()))
            .await
            .is_err(),
        "an unknown starting event read as the whole history"
    );
    std::fs::remove_dir_all(dir).unwrap();
}
