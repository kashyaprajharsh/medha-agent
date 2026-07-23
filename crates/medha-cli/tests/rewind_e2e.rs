//! End-to-end check of the rewind (time-travel) mechanism (§18.4), exercising
//! the exact pieces the TUI `/rewind` task chains together, with a REAL SQLite
//! event log and a REAL workspace sandbox:
//!
//!   log events (with real snapshot ids) → fork → rollback_plan → sandbox.restore
//!
//! The unit tests cover each piece in isolation; this proves they compose — that
//! the snapshot id the sandbox returns on a write survives round-tripping through
//! the event log and drives a correct file rollback, while the branch preserves
//! the original session.

use kernel::{Event, EventLog, Observation, Session, TrustLabel};
use sandbox::WorkspaceSandbox;
use serde_json::json;
use ulid::Ulid;

/// Log a write exactly as the kernel does: the tool result (path + snapshot id)
/// becomes a `tool.observation` event, so `rollback_plan` can later read it back.
async fn log_write(
    log: &store::SqliteLog,
    sbx: &WorkspaceSandbox,
    s: &Session,
    path: &str,
    content: &str,
) {
    let snapshot = sbx.write(path, content).await.unwrap();
    let payload = json!({ "path": path, "written": true, "snapshot": snapshot });
    let obs = Observation::ok(Ulid::new().to_string(), payload);
    log.append(Event::tool_obs(s, &obs, TrustLabel::Tool))
        .await
        .unwrap();
}

#[tokio::test]
async fn rewind_branches_the_session_and_rolls_files_back_to_the_cut() {
    let dir = std::env::temp_dir().join(format!("medha-rewind-e2e-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let sbx = WorkspaceSandbox::new_jailed(&dir).unwrap();
    let log = store::SqliteLog::open(dir.join(".medha/events.db")).unwrap();
    let s = Session::new();

    // Turn 1: create lib.rs = v1.
    log.append(Event::user_message(&s, "create lib.rs"))
        .await
        .unwrap();
    log_write(&log, &sbx, &s, "lib.rs", "v1").await;

    // Turn 2 (the point we'll rewind to): edit lib.rs to v2, create main.rs.
    let cut = log
        .append(Event::user_message(&s, "add main.rs and bump lib"))
        .await
        .unwrap();
    log_write(&log, &sbx, &s, "lib.rs", "v2").await;
    log_write(&log, &sbx, &s, "main.rs", "fn main() {}").await;

    // Sanity: the workspace is at the post-turn-2 state.
    assert_eq!(sbx.read("lib.rs").await.unwrap(), "v2");
    assert_eq!(sbx.read("main.rs").await.unwrap(), "fn main() {}");

    // --- Reproduce the TUI /rewind task body verbatim ---
    let events = log.events(s.id).await;
    let idx = kernel::cut_index(&events, cut.id).expect("cut point present");
    let branch = log.fork(s.id, cut.id).await.unwrap();
    let plan = kernel::rollback_plan(&events, cut.id);
    let mut rolled = 0usize;
    for fr in &plan {
        if sbx.restore(&fr.path, fr.snapshot.as_deref()).await.is_ok() {
            rolled += 1;
        }
    }
    let msgs = kernel::project_messages(&events[..idx]);

    // Files rolled back to the turn-1 state: lib.rs back to v1, main.rs gone.
    assert_eq!(rolled, 2, "both post-cut files rolled back");
    assert_eq!(
        sbx.read("lib.rs").await.unwrap(),
        "v1",
        "lib.rs restored to pre-turn-2"
    );
    assert!(
        sbx.read("main.rs").await.is_err(),
        "main.rs (created after cut) removed"
    );

    // The branch keeps turn 1's conversation and nothing from turn 2 onward.
    assert_eq!(
        msgs.first().map(|m| m.content.as_str()),
        Some("create lib.rs")
    );
    assert!(
        !msgs.iter().any(|m| m.content.contains("add main.rs")),
        "the rewound-to turn and everything after it are excluded from the branch"
    );
    assert_ne!(branch, s.id);
    let branch_events = log.events(branch).await;
    assert_eq!(
        branch_events.len(),
        2,
        "branch = prefix before the cut (turn-1 user msg + its write)"
    );
    assert!(
        branch_events.iter().all(|e| e.session_id == branch),
        "events re-homed onto the branch"
    );
    assert_eq!(
        log.events(s.id).await.len(),
        5,
        "original session preserved intact"
    );

    // The tamper-evident chain still verifies after the fork appended events.
    log.verify().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}
