use super::*;

#[tokio::test]
async fn a_writer_registry_cannot_load_the_parent_checkouts_skills() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().canonicalize().unwrap();
    let parent = root.join("parent");
    let writer = root.join("writer");
    let personal = root.join("personal");
    for folder in [&parent, &writer] {
        std::fs::create_dir_all(folder.join(".git")).unwrap();
        std::fs::create_dir_all(folder.join(".medha/skills/task")).unwrap();
        std::fs::write(
            folder.join(".medha/skills/task/SKILL.md"),
            "---\nname: task\ndescription: Help with a task\n---\nPARENT_PROJECT_PROCEDURE",
        )
        .unwrap();
    }
    std::fs::create_dir_all(personal.join("personal")).unwrap();
    std::fs::write(
        personal.join("personal/SKILL.md"),
        "---\nname: personal\ndescription: A personal procedure\n---\nPERSONAL_PROCEDURE",
    )
    .unwrap();
    let authorized = parent.clone();
    let cache = root.join("cache");
    let store = Arc::new(
        SkillStore::new(parent.join(".medha/skills"), Some(personal)).with_project_source(
            Arc::new(move |_, path| path.starts_with(&authorized).then(|| cache.clone())),
        ),
    );
    let mut registry = ToolRegistry::with_workspace(
        Arc::new(WorkspaceSandbox::new_jailed(&parent).unwrap()),
        Arc::new(store::FileArtifactStore::open(root.join("blobs")).unwrap()),
    );
    registry.register_skills(store);
    let child = registry
        .rebase(Arc::new(WorkspaceSandbox::new_jailed(&writer).unwrap()))
        .unwrap();
    let call = |name: &str| ToolIntent {
        id: "skill".into(),
        tool: "skill".into(),
        args: json!({"name": name}),
    };
    assert_eq!(
        Executor::execute(&registry, &call("task")).await.status,
        kernel::ObsStatus::Ok
    );
    assert_ne!(
        Executor::execute(&child, &call("task")).await.status,
        kernel::ObsStatus::Ok
    );
    let user = Executor::execute(&child, &call("personal")).await;
    assert_eq!(user.status, kernel::ObsStatus::Ok, "{user:?}");
    assert_eq!(user.payload["procedure"], "PERSONAL_PROCEDURE");
    assert_eq!(
        child
            .specs()
            .iter()
            .map(|spec| &spec.name)
            .collect::<Vec<_>>(),
        registry
            .specs()
            .iter()
            .map(|spec| &spec.name)
            .collect::<Vec<_>>()
    );
}
