use super::*;

#[cfg(unix)]
#[tokio::test]
async fn bundled_example_mcp_server_handshakes_and_answers() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-plugin");
    let store = Store::new(
        root.join("home/plugins"),
        root.join("workspace/.medha/plugins"),
        root.join("home/plugins.toml"),
        root.join("state/plugins.toml"),
        env!("CARGO_PKG_VERSION"),
    );
    store.install(&source).unwrap();
    store
        .enable("dev.medha.hello", None, &extensions::Grant::default())
        .unwrap();
    let mut session = SessionPlugins::discover(store);
    let servers = session.mcp_servers(&HashSet::new());
    assert_eq!(servers.len(), 1, "{:?}", session.warnings());
    let manager = mcp::McpManager::new(
        root.join("workspace"),
        mcp::Config {
            enabled: true,
            servers,
            ..mcp::Config::default()
        },
    );
    manager.connect_startup().await;
    let statuses = manager.status().await;
    assert_eq!(statuses[0].state, mcp::ServerState::Ready, "{statuses:?}");
    let result = manager
        .call(
            "mcp__dev-medha-hello-greetings__hello",
            &serde_json::json!({"name":"Ada"}),
        )
        .await
        .unwrap();
    assert!(result.text.contains("Hello, Ada!"), "{}", result.text);
    manager.shutdown().await;
}

#[test]
fn server_ids_are_valid_tool_name_segments() {
    assert_eq!(server_id("dev.me.guard", "gh"), "dev-me-guard-gh");
    assert_eq!(server_id("dev.me_x", "a-b"), "dev-me-x-a-b");
    assert!(!server_id("dev..x", "y").contains("--"));
}

#[test]
fn remote_mcp_cannot_connect_to_an_undeclared_host() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join("source")).unwrap();
    std::fs::write(
        root.join("source/plugin.toml"),
        "schema_version = 1\nid = \"dev.me.remote\"\nname = \"Remote\"\nversion = \"0.1.0\"\n\
         medha = \">=0.1.0\"\n[permissions]\nnetwork_hosts = [\"allowed.example.com\"]\n\
         [[components]]\nkind = \"mcp\"\nid = \"server\"\nurl = \"https://denied.example.com/mcp\"\n",
    )
    .unwrap();
    let store = Store::new(
        root.join("home/plugins"),
        root.join("workspace/.medha/plugins"),
        root.join("home/plugins.toml"),
        root.join("state/plugins.toml"),
        env!("CARGO_PKG_VERSION"),
    );
    store.install(root.join("source")).unwrap();
    let grant = store
        .inspect("dev.me.remote", None)
        .unwrap()
        .requested_grant()
        .unwrap();
    store.enable("dev.me.remote", None, &grant).unwrap();
    let mut session = SessionPlugins::discover(store);
    assert!(session.mcp_servers(&HashSet::new()).is_empty());
    assert!(
        session
            .warnings()
            .iter()
            .any(|warning| warning.contains("not listed in permissions.network_hosts"))
    );
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
