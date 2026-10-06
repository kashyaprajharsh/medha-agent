use super::*;
use kernel::Event;

#[test]
fn every_file_in_the_folder_ships_and_parses() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("connectors");
    let mut on_disk: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let shipped: Vec<&str> = FILES.iter().map(|(id, _)| *id).collect();
    assert_eq!(on_disk, shipped, "add new files to `entries!`, sorted");
    for (file, text) in FILES {
        let connector: Connector =
            toml::from_str(text).unwrap_or_else(|error| panic!("{file}.toml: {error}"));
        assert_eq!(connector.id, *file);
    }
    assert_eq!(catalog().len(), FILES.len());
}

#[test]
fn every_connector_has_its_logo_in_the_desktop_app() {
    let logos =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/desktop/src/assets/connectors");
    for connector in catalog() {
        assert!(
            logos.join(format!("{}.png", connector.id)).is_file(),
            "{} has no logo",
            connector.id
        );
    }
}

#[test]
fn connectors_only_reach_vendors_over_https() {
    for connector in catalog() {
        for address in [&connector.url, &connector.source] {
            let url = url::Url::parse(address).unwrap();
            assert_eq!(url.scheme(), "https", "{}: {address}", connector.id);
            assert!(url.host_str().is_some_and(|host| host.contains('.')));
            assert!(url.username().is_empty() && url.password().is_none());
        }
        assert!(!connector.name.is_empty());
        assert!(!connector.description.is_empty() && !connector.description.contains('\n'));
    }
}

#[test]
fn a_connector_reconnects_at_launch_through_the_ordinary_path() {
    for (id, auth) in [("linear", "oauth"), ("deepwiki", "none")] {
        let server = find(id).unwrap().server();
        let resolved = crate::config::resolve_mcp_server(id, &server);
        assert!(
            !resolved.requires_approval,
            "{id} would wait at every launch"
        );
        let mcp::Transport::Remote {
            url,
            auth: resolved_auth,
        } = resolved.transport
        else {
            panic!("{id} is not remote");
        };
        assert_eq!(url, find(id).unwrap().url);
        match auth {
            "oauth" => assert!(matches!(resolved_auth, mcp::RemoteAuth::OAuth)),
            _ => assert!(matches!(resolved_auth, mcp::RemoteAuth::None)),
        }
    }
}

#[test]
fn connecting_reuses_a_server_added_by_hand_and_never_takes_a_name() {
    let linear = find("linear").unwrap();
    let mut servers = BTreeMap::from([(
        "my-linear".to_string(),
        McpServer {
            url: "https://MCP.linear.app/mcp/".into(),
            auth: "oauth".into(),
            ..Default::default()
        },
    )]);
    assert_eq!(linear.install(&mut servers), "my-linear");
    assert_eq!(servers.len(), 1);

    let mut servers = BTreeMap::from([(
        "linear".to_string(),
        McpServer {
            url: "https://linear.example.com/mcp".into(),
            ..Default::default()
        },
    )]);
    assert_eq!(linear.install(&mut servers), "linear-2");
    assert_eq!(servers["linear"].url, "https://linear.example.com/mcp");
    assert_eq!(servers["linear-2"].url, linear.url);
    assert_eq!(linear.install(&mut servers), "linear-2");
}

#[test]
fn detection_never_looks_outside_the_project() {
    for connector in catalog() {
        assert!(
            (2..=3).contains(&connector.asks.len()),
            "{} needs two or three asks",
            connector.id
        );
        for rule in &connector.detect {
            let path = rule.split_once(':').map_or(rule.as_str(), |(_, name)| name);
            assert!(
                !Path::new(path).is_absolute() && !path.split('/').any(|part| part == ".."),
                "{}: {rule}",
                connector.id
            );
        }
    }
}

#[test]
fn evidence_names_what_it_found_and_matches_whole_packages() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"dependencies":{"@neondatabase/serverless":"1"},"devDependencies":{"@sentry/nextjs":"8"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("requirements.txt"),
        "jira-extras==1\nsentry-sdk>=2\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("netlify.toml"), "").unwrap();
    let project = Project::scan(dir.path());
    let found = |id| project.evidence(find(id).unwrap());
    assert_eq!(
        found("neon").as_deref(),
        Some("Found @neondatabase/serverless in package.json")
    );
    assert_eq!(found("netlify").as_deref(), Some("Found netlify.toml"));
    assert_eq!(
        found("sentry").as_deref(),
        Some("Found @sentry/nextjs in package.json")
    );
    assert_eq!(found("atlassian"), None, "jira-extras is not jira");
}

#[test]
fn activity_counts_only_calls_that_ran_for_that_server_this_week() {
    let now = 10.0 * 86_400.0;
    let session = kernel::Session::new();
    let call = |tool: &str, days_ago: f64| {
        let intent = kernel::ToolIntent {
            id: "c".into(),
            tool: tool.into(),
            args: json!({}),
        };
        let mut event = Event::tool_effect_prepared(&session, &intent, "state:*");
        event.ts = now - days_ago * 86_400.0;
        event
    };
    let asked_but_never_ran = Event::model_intent(
        &session,
        &kernel::ToolIntent {
            id: "d".into(),
            tool: "mcp__linear__create_issue".into(),
            args: json!({}),
        },
    );
    let events = [
        call("mcp__linear__create_issue", 1.0),
        call("mcp__linear__list_issues", 2.0),
        call("mcp__linear__list_issues", 9.0),
        call("mcp__linear-2__list_issues", 1.0),
        call("shell.exec", 1.0),
        asked_but_never_ran,
    ];
    let mut tally = HashMap::new();
    activity(&kernel::events::tool_calls(&events), now, &mut tally);
    assert_eq!(tally["linear"].this_week, 2);
    assert_eq!(tally["linear"].last_used, now - 86_400.0);
    assert_eq!(tally["linear-2"].this_week, 1);
    assert_eq!(tally.len(), 2);
}
