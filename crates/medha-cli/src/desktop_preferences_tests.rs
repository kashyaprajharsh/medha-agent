use super::*;

fn server() -> config::McpServer {
    config::McpServer {
        command: vec!["npx".into(), "server".into()],
        env: [("API_KEY".to_string(), "${key}".to_string())].into(),
        trust: "workspace".into(),
        ..Default::default()
    }
}

#[test]
fn an_update_changes_access_but_never_what_the_key_is_bound_to() {
    let mut server = server();
    apply_mcp_update(
        &mut server,
        &json!({
            "trust": "trusted", "network": false, "parallel": true,
            "deny_tools": ["delete_repo", " "], "command": ["rm"], "env": {"X": "1"}
        }),
    )
    .unwrap();
    assert_eq!(server.trust, "trusted");
    assert_eq!(server.network, Some(false));
    assert!(server.parallel_calls);
    assert_eq!(server.deny_tools, ["delete_repo"]);
    assert_eq!(server.command, ["npx", "server"]);
    assert_eq!(server.env.len(), 1);
}

#[test]
fn the_health_report_says_what_is_wrong_and_what_can_be_fixed() {
    let pulse = config::Pulse {
        medha_home: "/home/me/.medha".into(),
        config_path: "/home/me/.medha/config.toml".into(),
        config_exists: true,
        resolved: Err("MEDHA_PROTOCOL is not a protocol".into()),
        medha_env: vec!["MEDHA_PROTOCOL".into()],
        ignored_env: vec!["OPENAI_API_KEY".into()],
        project_lock: None,
        lock_executor: None,
        checks: vec![config::Check {
            health: config::Health::Warn,
            title: "default model".into(),
            detail: "'gone' is not a saved model".into(),
            auto_fixable: true,
        }],
    };
    let view = health_view(&pulse);
    assert_eq!(view["checks"][0]["health"], "warn");
    assert_eq!(view["checks"][0]["fixable"], true);
    assert_eq!(view["fixable"], true);
    assert_eq!(view["active"]["error"], "MEDHA_PROTOCOL is not a protocol");
    assert_eq!(view["ignored_env"][0], "OPENAI_API_KEY");
}

#[test]
fn an_update_rejects_values_it_does_not_understand() {
    let mut server = server();
    assert!(apply_mcp_update(&mut server, &json!({"trust": "root"})).is_err());
    assert!(apply_mcp_update(&mut server, &json!({"network": "yes"})).is_err());
    assert!(apply_mcp_update(&mut server, &json!({"deny_tools": "all"})).is_err());
    apply_mcp_update(&mut server, &json!({"network": null})).unwrap();
    assert_eq!(server.network, None);
}
