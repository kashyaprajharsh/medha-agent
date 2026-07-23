use kernel::{Event, EventLog, Session, TrustLabel};
use memory::{ConfidenceRung, MemoryEntry, MemoryKind, MemoryOp, MemoryProjection, Scope};
use std::process::Command;
use ulid::Ulid;

fn encoded_workspace(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|ch| {
            if matches!(ch, '/' | '\\' | ':') {
                '-'
            } else {
                ch
            }
        })
        .collect()
}

fn run_memory(workspace: &std::path::Path, home: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_medha"))
        .arg("memory")
        .args(args)
        .current_dir(workspace)
        .env("MEDHA_HOME", home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test]
async fn write_recall_show_and_fork_excludes_post_cut_memory() {
    let root = std::env::temp_dir().join(format!("medha-memory-e2e-{}", Ulid::new()));
    let workspace = root.join("quoted-project");
    let home = root.join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let state = home.join("projects").join(encoded_workspace(&workspace));
    std::fs::create_dir_all(&state).unwrap();

    let log = store::SqliteLog::open(state.join("events.db")).unwrap();
    let projection =
        MemoryProjection::open(state.join("memory.db"), home.join("memory.db")).unwrap();
    let session = Session::new();
    let evidence = log
        .append(Event::user_message(
            &session,
            "Remember the 'quoted-hyphen' cache decision.",
        ))
        .await
        .unwrap();
    let entry = MemoryEntry {
        name: "cache-decision".into(),
        claim: "Use the 'quoted-hyphen' cache key.".into(),
        description: "cache-key decision with quotes".into(),
        kind: MemoryKind::Decision,
        scope: Scope::Project,
        trust: TrustLabel::User,
        confidence: ConfidenceRung::UserStated,
        provenance: vec![evidence.id],
        sessions: vec![session.id],
        version: 1,
        pinned: false,
        links: vec![],
        created: evidence.ts,
        updated: evidence.ts,
    };
    let op = MemoryOp::Write {
        entry: entry.clone(),
    };
    let write = log
        .append(Event::memory_write(
            &session,
            serde_json::to_value(&op).unwrap(),
        ))
        .await
        .unwrap();
    projection.apply(&op).unwrap();

    let fresh = MemoryProjection::open(state.join("memory.db"), home.join("memory.db")).unwrap();
    let k3 = memory::recall::compile_k3(&fresh, 1_200, evidence.ts).unwrap();
    assert!(k3.contains("cache-decision"));

    let list = run_memory(&workspace, &home, &["list"]);
    assert!(list.contains("cache-decision"));
    let search = run_memory(&workspace, &home, &["search", "quoted-hyphen"]);
    assert!(search.contains("Use the 'quoted-hyphen' cache key."));
    let show = run_memory(&workspace, &home, &["show", "cache-decision"]);
    assert!(show.contains(&evidence.id.to_string()));
    assert!(show.contains(&session.id.to_string()));
    run_memory(&workspace, &home, &["pin", "cache-decision"]);
    let pinned = run_memory(&workspace, &home, &["show", "cache-decision"]);
    assert!(pinned.contains("pinned: true"));

    let pending_id = Ulid::new();
    let pending_dir = state.join("memory-pending");
    std::fs::create_dir_all(&pending_dir).unwrap();
    std::fs::write(
        pending_dir.join(format!("{pending_id}.json")),
        serde_json::to_vec(&MemoryOp::Pin {
            scope: Scope::Project,
            name: "cache-decision".into(),
            pinned: false,
        })
        .unwrap(),
    )
    .unwrap();
    assert!(run_memory(&workspace, &home, &["pending"]).contains(&pending_id.to_string()));
    run_memory(&workspace, &home, &["approve", &pending_id.to_string()]);
    assert!(!pending_dir.join(format!("{pending_id}.json")).exists());
    let unpinned = run_memory(&workspace, &home, &["show", "cache-decision"]);
    assert!(unpinned.contains("pinned: false"));

    let branch_id = log.fork(session.id, write.id).await.unwrap();
    let branch_events = log.events(branch_id).await;
    let branch =
        MemoryProjection::open(state.join("branch-memory.db"), home.join("memory.db")).unwrap();
    branch.rebuild_project(branch_events.into_iter()).unwrap();
    let branch_k3 = memory::recall::compile_k3(&branch, 1_200, evidence.ts).unwrap();
    assert!(!branch_k3.contains("cache-decision"));

    let after_write = log
        .append(Event::model_text(&session, "after the memory write"))
        .await
        .unwrap();
    let copied_branch = log.fork(session.id, after_write.id).await.unwrap();
    assert!(
        log.events(copied_branch)
            .await
            .iter()
            .any(|event| event.kind == kernel::EventKind::MemoryWrite)
    );
    run_memory(&workspace, &home, &["forget", "cache-decision"]);
    assert!(!run_memory(&workspace, &home, &["list"]).contains("cache-decision"));
}

#[cfg(unix)]
#[tokio::test]
async fn cli_edit_appends_a_user_trust_update_before_projection() {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!("medha-memory-edit-e2e-{}", Ulid::new()));
    let workspace = root.join("workspace");
    let home = root.join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let state = home.join("projects").join(encoded_workspace(&workspace));
    std::fs::create_dir_all(&state).unwrap();
    let log = store::SqliteLog::open(state.join("events.db")).unwrap();
    let projection =
        MemoryProjection::open(state.join("memory.db"), home.join("memory.db")).unwrap();
    let session = Session::new();
    let entry = MemoryEntry {
        name: "editable-fact".into(),
        claim: "old claim".into(),
        description: "editable hook".into(),
        kind: MemoryKind::Project,
        scope: Scope::Project,
        trust: TrustLabel::Tool,
        confidence: ConfidenceRung::Candidate,
        provenance: vec![],
        sessions: vec![session.id],
        version: 1,
        pinned: false,
        links: vec![],
        created: 1.0,
        updated: 1.0,
    };
    let op = MemoryOp::Write { entry };
    log.append(Event::memory_write(
        &session,
        serde_json::to_value(&op).unwrap(),
    ))
    .await
    .unwrap();
    projection.apply(&op).unwrap();

    let editor = root.join("editor.sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\nprintf \"edited claim with 'quotes'\" > \"$1\"\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&editor).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&editor, permissions).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_medha"))
        .args(["memory", "edit", "editable-fact"])
        .current_dir(&workspace)
        .env("MEDHA_HOME", &home)
        .env("EDITOR", &editor)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reopened_log = store::SqliteLog::open(state.join("events.db")).unwrap();
    let events = reopened_log.all_events().unwrap();
    let last = events
        .iter()
        .rev()
        .find(|event| event.kind == kernel::EventKind::MemoryWrite)
        .unwrap();
    let MemoryOp::Update { entry } = serde_json::from_value(last.payload.clone()).unwrap() else {
        panic!("edit must append an update event");
    };
    assert_eq!(entry.claim, "edited claim with 'quotes'");
    assert_eq!(entry.trust, TrustLabel::User);
    assert_eq!(entry.confidence, ConfidenceRung::UserStated);
    assert_eq!(entry.version, 2);
}
