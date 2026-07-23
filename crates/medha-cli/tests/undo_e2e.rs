//! `medha undo`: log events → rollback_plan → sandbox.restore, same pipeline
//! `rewind_e2e.rs` proves, targeted at a write event instead of a prompt.

use kernel::{Event, EventLog, Observation, Session, TrustLabel};
use sandbox::WorkspaceSandbox;
use serde_json::json;
use ulid::Ulid;

async fn log_write(
    log: &store::SqliteLog,
    sbx: &WorkspaceSandbox,
    s: &Session,
    path: &str,
    content: &str,
) -> kernel::Event {
    let snapshot = sbx.write(path, content).await.unwrap();
    let payload = json!({ "path": path, "written": true, "snapshot": snapshot });
    let obs = Observation::ok(Ulid::new().to_string(), payload);
    log.append(Event::tool_obs(s, &obs, TrustLabel::Tool))
        .await
        .unwrap()
}

#[tokio::test]
async fn undo_reverts_only_the_single_most_recent_write() {
    let dir = std::env::temp_dir().join(format!("medha-undo-e2e-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();
    let log = store::SqliteLog::open(dir.join(".medha/events.db")).unwrap();
    let s = Session::new();

    log_write(&log, &sbx, &s, "a.txt", "v1").await;
    let last = log_write(&log, &sbx, &s, "a.txt", "v2").await;
    assert_eq!(sbx.read("a.txt").await.unwrap(), "v2");

    // `medha undo` (no --event): target = the single most recent write-family
    // observation across the log — found here by scanning newest-first, the
    // same walk `recent_writes` does in main.rs.
    let events = log.events(s.id).await;
    let plan = kernel::rollback_plan(&events, last.id);
    assert_eq!(plan.len(), 1, "only a.txt's second write is undone");
    for fr in &plan {
        sbx.restore(&fr.path, fr.snapshot.as_deref()).await.unwrap();
    }
    assert_eq!(
        sbx.read("a.txt").await.unwrap(),
        "v1",
        "reverted to the pre-second-write snapshot"
    );

    log.verify().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn undo_by_event_id_reverts_everything_from_that_point_forward() {
    let dir = std::env::temp_dir().join(format!("medha-undo-e2e-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();
    let log = store::SqliteLog::open(dir.join(".medha/events.db")).unwrap();
    let s = Session::new();

    log_write(&log, &sbx, &s, "lib.rs", "v1").await;
    let target = log_write(&log, &sbx, &s, "lib.rs", "v2").await;
    log_write(&log, &sbx, &s, "main.rs", "fn main() {}").await;

    assert_eq!(sbx.read("lib.rs").await.unwrap(), "v2");
    assert_eq!(sbx.read("main.rs").await.unwrap(), "fn main() {}");

    // `medha undo --event <target>`: undoes lib.rs's second write AND the
    // later main.rs creation — everything at/after the given event.
    let events = log.events(s.id).await;
    let plan = kernel::rollback_plan(&events, target.id);
    assert_eq!(plan.len(), 2, "both post-target writes are in the plan");
    for fr in &plan {
        sbx.restore(&fr.path, fr.snapshot.as_deref()).await.unwrap();
    }
    assert_eq!(
        sbx.read("lib.rs").await.unwrap(),
        "v1",
        "lib.rs rolled back to pre-target"
    );
    assert!(
        sbx.read("main.rs").await.is_err(),
        "main.rs (created after target) removed"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn undo_at_an_event_with_no_writes_after_it_is_a_no_op() {
    let dir = std::env::temp_dir().join(format!("medha-undo-e2e-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();
    let log = store::SqliteLog::open(dir.join(".medha/events.db")).unwrap();
    let s = Session::new();

    log_write(&log, &sbx, &s, "a.txt", "v1").await;
    let last = log
        .append(Event::user_message(&s, "just chatting, no more writes"))
        .await
        .unwrap();

    let events = log.events(s.id).await;
    let plan = kernel::rollback_plan(&events, last.id);
    assert!(plan.is_empty(), "nothing to undo after the last write");

    std::fs::remove_dir_all(&dir).ok();
}
