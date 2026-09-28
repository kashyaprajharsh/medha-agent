use super::*;
use kernel::{EventKind, InMemoryLog, TrustLabel};
use memory::{ConfidenceRung, MemoryEntry, MemoryKind, MemoryOp, MemoryProjection, Scope};

fn entry(learned_in: &Session, from: ulid::Ulid) -> MemoryEntry {
    MemoryEntry {
        name: "test-runner".into(),
        claim: "Tests run with cargo nextest".into(),
        description: "How this project runs its tests".into(),
        kind: MemoryKind::Project,
        scope: Scope::Project,
        trust: TrustLabel::User,
        confidence: ConfidenceRung::UserStated,
        provenance: vec![from],
        sessions: vec![learned_in.id],
        version: 1,
        pinned: false,
        links: vec![],
        created: 1.0,
        updated: 1.0,
    }
}

#[tokio::test]
async fn pin_and_forget_are_logged_before_the_memory_changes() {
    let dir = std::env::temp_dir().join(format!("medha-desktop-memory-{}", ulid::Ulid::new()));
    let store = Arc::new(MemoryProjection::open(dir.join("p.db"), dir.join("u.db")).unwrap());
    let log = InMemoryLog::new();
    let session = Session::new();
    let said = log
        .append(kernel::Event::user_message(
            &session,
            "we run tests with cargo nextest",
        ))
        .await
        .unwrap();
    store
        .apply_async(&MemoryOp::Write {
            entry: entry(&session, said.id),
        })
        .await
        .unwrap();

    let listed = list(Some(&store)).await.unwrap();
    assert_eq!(
        listed["memories"][0]["claim"],
        "Tests run with cargo nextest"
    );

    let target = json!({"scope": "project", "name": "test-runner", "pinned": true});
    let pinned = change(&log, &session, Some(&store), "memory.pin", &target)
        .await
        .unwrap();
    assert_eq!(pinned["memories"][0]["pinned"], true);

    let source = provenance(&log, Some(&store), &target).await.unwrap();
    assert_eq!(source["excerpt"], "we run tests with cargo nextest");

    let forgotten = change(&log, &session, Some(&store), "memory.forget", &target)
        .await
        .unwrap();
    assert!(forgotten["memories"].as_array().unwrap().is_empty());
    let writes = log
        .events(session.id)
        .await
        .into_iter()
        .filter(|event| event.kind == EventKind::MemoryWrite)
        .count();
    assert_eq!(writes, 2);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn memory_that_is_off_says_so() {
    assert_eq!(
        list(None).await.unwrap_err(),
        "Memory is off in this workspace"
    );
}

#[tokio::test]
async fn forgetting_something_never_remembered_changes_nothing() {
    let dir = std::env::temp_dir().join(format!("medha-desktop-memory-none-{}", ulid::Ulid::new()));
    let store = Arc::new(MemoryProjection::open(dir.join("p.db"), dir.join("u.db")).unwrap());
    let (log, session) = (InMemoryLog::new(), Session::new());
    let target = json!({"scope": "project", "name": "never"});
    let error = change(&log, &session, Some(&store), "memory.forget", &target)
        .await
        .unwrap_err();
    assert_eq!(error, "That memory no longer exists");
    assert!(log.events(session.id).await.is_empty());
    std::fs::remove_dir_all(dir).ok();
}
