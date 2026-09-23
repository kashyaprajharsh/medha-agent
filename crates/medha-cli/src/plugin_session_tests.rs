use super::*;

#[test]
fn server_ids_are_valid_tool_name_segments() {
    assert_eq!(server_id("dev.me.guard", "gh"), "dev-me-guard-gh");
    assert_eq!(server_id("dev.me_x", "a-b"), "dev-me-x-a-b");
    assert!(!server_id("dev..x", "y").contains("--"));
}

#[test]
fn enabling_and_disabling_changes_skills_in_the_running_session() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source = root.join("source");
    std::fs::create_dir_all(source.join("skills/tidy")).unwrap();
    std::fs::write(
        source.join("plugin.toml"),
        "schema_version = 1\nid = \"dev.me.kit\"\nname = \"Kit\"\nversion = \"0.1.0\"\n\
         medha = \">=0.1.0\"\n\n[[components]]\nkind = \"skill\"\nid = \"tidy\"\n\
         path = \"skills/tidy\"\n",
    )
    .unwrap();
    std::fs::write(
        source.join("skills/tidy/SKILL.md"),
        "---\nname: tidy\ndescription: Tidy the imports\n---\n\nsteps",
    )
    .unwrap();
    let store = Store::new(
        root.join("home/plugins"),
        root.join("workspace/.medha/plugins"),
        root.join("home/plugins.toml"),
        root.join("state/plugins.toml"),
        env!("CARGO_PKG_VERSION"),
    );
    store.install(&source).unwrap();
    let skills = std::sync::Arc::new(tools::SkillStore::new(root.join("project-skills"), None));
    let live = LivePlugins::new(
        store.clone(),
        skills.clone(),
        None,
        Box::new(Vec::new),
        HashSet::new(),
        HashSet::new(),
    );
    let known: HashSet<String> = ["skill".to_string()].into();
    let listed = || skills.manifest(&known, None);

    assert!(live.apply().is_empty());
    assert!(!listed().contains("Tidy the imports"));

    let grant = store
        .inspect("dev.me.kit", None)
        .unwrap()
        .requested_grant()
        .unwrap();
    store.enable("dev.me.kit", None, &grant).unwrap();
    assert!(live.apply().is_empty());
    assert!(listed().contains("Tidy the imports"));

    store.disable("dev.me.kit", None).unwrap();
    live.apply();
    assert!(!listed().contains("Tidy the imports"));
}
