use super::*;

fn state(text: &str) -> GuardedFileState {
    GuardedFileState::Present {
        sha256: Sha256::digest(text.as_bytes()).into(),
        len: text.len() as u64,
        identity: "test-file".into(),
    }
}

#[test]
fn all_current_pages_are_needed_and_mixed_revisions_do_not_bless_an_overwrite() {
    let ledger = Revisions::default();
    let path = Path::new("source.rs");
    assert!(!ledger.permits(path, &state("one\ntwo\n")));
    ledger.page(path, 1, 1, "one\n", true);
    assert!(!ledger.permits(path, &state("one\ntwo\n")));
    ledger.page(path, 2, 2, "two\n", false);
    assert!(ledger.permits(path, &state("one\ntwo\n")));
    assert!(!ledger.permits(path, &state("changed\ntwo\n")));
    ledger.page(path, 1, 1, "changed\n", true);
    assert!(ledger.permits(path, &state("changed\ntwo\n")));
    assert!(!Revisions::default().permits(path, &state("changed\ntwo\n")));
}

#[test]
fn own_verified_write_advances_knowledge_and_eviction_only_requires_a_reread() {
    let ledger = Revisions::default();
    assert!(ledger.permits(Path::new("new"), &GuardedFileState::Missing));
    ledger.written(Path::new("first"), "saved");
    assert!(ledger.permits(Path::new("first"), &state("saved")));
    for index in 0..FILES {
        ledger.written(Path::new(&index.to_string()), "saved");
    }
    assert!(!ledger.permits(Path::new("first"), &state("saved")));
    ledger.page(Path::new("first"), 1, 1, "saved", false);
    assert!(ledger.permits(Path::new("first"), &state("saved")));
}

/// One chat reading many files cannot push another chat's knowledge out.
#[tokio::test]
async fn another_chats_reads_do_not_evict_this_chats_record() {
    let ledger = Revisions::default();
    let (mine, other) = (ulid::Ulid::new(), ulid::Ulid::new());
    kernel::egress::session_scope(mine, async { ledger.written(Path::new("kept"), "saved") }).await;
    kernel::egress::session_scope(other, async {
        for index in 0..FILES + 10 {
            ledger.written(Path::new(&index.to_string()), "x");
        }
    })
    .await;
    let known = kernel::egress::session_scope(mine, async {
        ledger.permits(Path::new("kept"), &state("saved"))
    })
    .await;
    assert!(known);
}

/// Write, edit, write: the chat knew the whole file, so its own edit keeps it known.
/// An edit made without knowing the file still needs a read before a whole replacement.
#[tokio::test]
async fn an_edit_of_a_known_file_keeps_it_known_and_a_blind_edit_does_not() {
    use crate::ToolRegistry;
    use kernel::{Executor, ObsStatus, ToolIntent};
    use serde_json::json;
    let root = test_support::scratch("edit-keeps-known");
    std::fs::write(root.join("unseen.txt"), "one\ntwo\n").unwrap();
    let workspace = std::sync::Arc::new(sandbox::WorkspaceSandbox::new_jailed(&root).unwrap());
    let artifacts =
        std::sync::Arc::new(store::FileArtifactStore::open(root.join("artifacts")).unwrap());
    let registry = ToolRegistry::with_workspace(workspace, artifacts);
    let call = |tool: &str, args: serde_json::Value| ToolIntent {
        id: tool.into(),
        tool: tool.into(),
        args,
    };
    for (path, status) in [
        ("filter.txt", ObsStatus::Ok),
        ("unseen.txt", ObsStatus::Error),
    ] {
        if path == "filter.txt" {
            let first = call("write", json!({"path": path, "content": "# note\na\n"}));
            assert_eq!(registry.execute(&first).await.status, ObsStatus::Ok);
        }
        let edit = call(
            "edit",
            json!({"path": path, "old_string": "a", "new_string": "b"}),
        );
        let edit = if path == "unseen.txt" {
            call(
                "edit",
                json!({"path": path, "old_string": "one", "new_string": "uno"}),
            )
        } else {
            edit
        };
        assert_eq!(registry.execute(&edit).await.status, ObsStatus::Ok);
        let replace = call("write", json!({"path": path, "content": "rewritten\n"}));
        assert_eq!(registry.execute(&replace).await.status, status, "{path}");
    }
}

#[tokio::test]
async fn previews_redacted_results_and_another_chat_cannot_authorize_a_whole_replacement() {
    use crate::{ToolRegistry, model_view};
    use kernel::{Executor, ObsStatus, ToolIntent};
    use serde_json::json;
    let root = test_support::scratch("read-before-write");
    std::fs::write(root.join("source.txt"), "first\nsecond\n").unwrap();
    let workspace = std::sync::Arc::new(sandbox::WorkspaceSandbox::new_jailed(&root).unwrap());
    let artifacts =
        std::sync::Arc::new(store::FileArtifactStore::open(root.join("artifacts")).unwrap());
    let registry = ToolRegistry::with_workspace(workspace.clone(), artifacts.clone());
    let write = ToolIntent {
        id: "overwrite".into(),
        tool: "write".into(),
        args: json!({"path":"source.txt","content":"replacement"}),
    };
    assert!(registry.preview(&write).await.is_some());
    assert_eq!(registry.execute(&write).await.status, ObsStatus::Error);
    let read = ToolIntent {
        id: "read".into(),
        tool: "read".into(),
        args: json!({"path":"source.txt"}),
    };
    let shown = registry.execute(&read).await;
    assert_eq!(shown.status, ObsStatus::Ok);
    let mut redacted = shown.clone();
    redacted.model_text = Some("private content removed".into());
    registry.observation_delivered("read", &redacted);
    assert_eq!(registry.execute(&write).await.status, ObsStatus::Error);
    let mut complete = shown;
    complete.model_text = model_view::file_page(&complete.payload);
    registry.observation_delivered("read", &complete);
    let other = ToolRegistry::with_workspace(workspace, artifacts);
    assert_eq!(other.execute(&write).await.status, ObsStatus::Error);
    assert_eq!(registry.execute(&write).await.status, ObsStatus::Ok);
    std::fs::write(root.join("source.txt"), "human later").unwrap();
    assert_eq!(registry.execute(&write).await.status, ObsStatus::Error);
    assert_eq!(
        std::fs::read_to_string(root.join("source.txt")).unwrap(),
        "human later"
    );
}

#[tokio::test]
async fn reading_every_page_of_a_crlf_file_without_a_final_newline_permits_its_overwrite() {
    use crate::{ToolRegistry, model_view};
    use kernel::{Executor, ObsStatus, ToolIntent};
    use serde_json::json;
    let root = test_support::scratch("read-every-page");
    let text = (1..=4_500)
        .map(|line| format!("line {line} with some padding text"))
        .collect::<Vec<_>>()
        .join("\r\n");
    std::fs::write(root.join("big.txt"), &text).unwrap();
    let workspace = std::sync::Arc::new(sandbox::WorkspaceSandbox::new_jailed(&root).unwrap());
    let artifacts =
        std::sync::Arc::new(store::FileArtifactStore::open(root.join("artifacts")).unwrap());
    let registry = ToolRegistry::with_workspace(workspace, artifacts);
    let mut offset = 1;
    let mut pages = 0;
    loop {
        let read = ToolIntent {
            id: format!("read-{pages}"),
            tool: "read".into(),
            args: json!({"path":"big.txt","offset":offset}),
        };
        let mut shown = registry.execute(&read).await;
        assert_eq!(shown.status, ObsStatus::Ok);
        shown.model_text = model_view::file_page(&shown.payload);
        registry.observation_delivered("read", &shown);
        pages += 1;
        if shown.payload["has_more"] != true {
            break;
        }
        offset = shown.payload["end_line"].as_u64().unwrap() + 1;
    }
    assert!(pages > 1, "the fixture must span several pages");
    let write = ToolIntent {
        id: "overwrite".into(),
        tool: "write".into(),
        args: json!({"path":"big.txt","content":"replacement\n"}),
    };
    let written = registry.execute(&write).await;
    assert_eq!(written.status, ObsStatus::Ok, "{}", written.payload);
}

#[tokio::test]
async fn a_shared_readonly_child_cannot_authorize_the_parents_whole_file_write() {
    use crate::{ToolRegistry, model_view};
    use kernel::{Executor, ObsStatus, ToolIntent};
    use serde_json::json;
    let scratch = test_support::scratch("read-child-scope");
    std::fs::write(scratch.join("source.txt"), "original\n").unwrap();
    let registry = Arc::new(ToolRegistry::with_workspace(
        Arc::new(sandbox::WorkspaceSandbox::new_jailed(&scratch).unwrap()),
        Arc::new(store::FileArtifactStore::open(scratch.join("artifacts")).unwrap()),
    ));
    let child = orchestrator::NarrowedExecutor::new(registry.clone(), None).read_only();
    let read = ToolIntent {
        id: "read".into(),
        tool: "read".into(),
        args: json!({"path":"source.txt"}),
    };
    let write = ToolIntent {
        id: "write".into(),
        tool: "write".into(),
        args: json!({"path":"source.txt","content":"replacement"}),
    };
    let parent = ulid::Ulid::new();
    let child_id = ulid::Ulid::new();
    kernel::egress::session_scope(child_id, async {
        let mut observed = child.execute(&read).await;
        observed.model_text = model_view::file_page(&observed.payload);
        child.observation_delivered("read", &observed);
        assert!(child.prepare_effect(&write).await.is_err());
    })
    .await;
    kernel::egress::session_scope(parent, async {
        assert_eq!(registry.execute(&write).await.status, ObsStatus::Error);
        let writer = orchestrator::NarrowedExecutor::new(registry.clone(), None);
        let mut observed = writer.execute(&read).await;
        observed.model_text = model_view::file_page(&observed.payload);
        writer.observation_delivered("read", &observed);
        assert_eq!(writer.execute(&write).await.status, ObsStatus::Ok);
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(scratch.join("source.txt")).unwrap(),
        "replacement"
    );
}
