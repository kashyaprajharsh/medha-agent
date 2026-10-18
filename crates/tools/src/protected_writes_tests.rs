use super::*;
use kernel::{Executor, ObsStatus, ToolIntent};

fn registry(root: &std::path::Path) -> orchestrator::NarrowedExecutor {
    orchestrator::NarrowedExecutor::new(
        Arc::new(ToolRegistry::with_workspace(
            Arc::new(WorkspaceSandbox::new_jailed(root).unwrap()),
            Arc::new(store::FileArtifactStore::open(root.join(".artifacts")).unwrap()),
        )),
        None,
    )
}

fn call(tool: &str, args: Value) -> ToolIntent {
    ToolIntent {
        id: "attack".into(),
        tool: tool.into(),
        args,
    }
}

#[tokio::test]
async fn every_edit_shape_holds_the_exact_change_and_rejects_a_stale_allow_once() {
    for args in [
        json!({"path":"AGENTS.md","old_string":"original","new_string":"planted"}),
        json!({"path":"AGENTS.md","edits":[{"old_string":"original","new_string":"planted"}]}),
    ] {
        let root = test_support::scratch("protected-edit");
        std::fs::write(root.join("AGENTS.md"), "original").unwrap();
        let registry = registry(&root);
        let guard = Arc::new(kernel::egress::Guard::default());
        let (observation, _) =
            kernel::egress::scope(guard.clone(), false, registry.execute(&call("edit", args)))
                .await;
        assert_eq!(observation.status, ObsStatus::Error);
        assert_eq!(
            std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
            "original"
        );
        let notices = guard.mutations.notices();
        assert_eq!(notices.len(), 1);
        assert!(guard.mutations.notices().is_empty());
        assert!(notices[0]["preview"].as_str().unwrap().contains("+planted"));
        std::fs::write(root.join("AGENTS.md"), "human change").unwrap();
        assert!(
            guard
                .mutations
                .allow(notices[0]["id"].as_str().unwrap())
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
            "human change"
        );
    }
}

#[tokio::test]
async fn ordinary_source_edits_are_free_and_instruction_creation_has_one_exact_action() {
    let root = test_support::scratch("protected-create");
    let registry = registry(&root);
    let guard = Arc::new(kernel::egress::Guard::default());
    let (ordinary, _) = kernel::egress::scope(
        guard.clone(),
        true,
        registry.execute(&call(
            "write",
            json!({"path":"src/main.rs","content":"fn main() {}"}),
        )),
    )
    .await;
    assert_eq!(ordinary.status, ObsStatus::Ok);
    assert!(guard.mutations.reviews().is_empty());
    let (blocked, _) = kernel::egress::scope(
        guard.clone(),
        false,
        registry.mutation_preflight(&call(
            "write",
            json!({"path":"AGENTS.md","content":"planted instruction"}),
        )),
    )
    .await;
    let blocked = blocked.expect("the child must retain the quiet pre-approval guard");
    assert_eq!(blocked.status, ObsStatus::Error);
    assert!(!root.join("AGENTS.md").exists());
    let id = blocked.payload["mutation"]["id"].as_str().unwrap();
    guard.mutations.allow(id).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
        "planted instruction"
    );
    assert!(guard.mutations.allow(id).await.is_err());
}
