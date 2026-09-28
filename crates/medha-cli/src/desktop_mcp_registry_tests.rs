use super::*;

fn page(server: Value, status: &str) -> Value {
    json!({
        "servers": [{"server": server, "_meta": {"io.modelcontextprotocol.registry/official": {"status": status}}}],
        "metadata": {"nextCursor": "next-page"}
    })
}

#[test]
fn a_package_becomes_a_command_with_its_variables() {
    let server = json!({
        "name": "com.pulsemcp/remote-filesystem",
        "description": "Cloud storage",
        "packages": [{
            "registryType": "npm", "identifier": "remote-filesystem-mcp-server", "version": "0.1.2",
            "transport": {"type": "stdio"},
            "environmentVariables": [
                {"name": "GCS_BUCKET", "isRequired": true},
                {"name": "GCS_PRIVATE_KEY", "isSecret": true}
            ]
        }]
    });
    let listed = listing(&page(server, "active"));
    let setup = &listed["servers"][0]["setups"][0];
    assert_eq!(
        setup["command"],
        json!(["npx", "-y", "remote-filesystem-mcp-server@0.1.2"])
    );
    assert_eq!(setup["variables"][0]["required"], true);
    assert_eq!(setup["variables"][1]["secret"], true);
    assert_eq!(listed["next"], "next-page");
}

#[test]
fn a_remote_asks_for_a_token_only_when_it_needs_a_bearer_header() {
    let server = json!({"name": "x", "remotes": [
        {"type": "streamable-http", "url": "https://a.example/mcp", "headers": [{"name": "Authorization", "value": "Bearer {key}"}]},
        {"type": "streamable-http", "url": "https://b.example/mcp"},
        {"type": "streamable-http", "url": "https://c.example/mcp", "headers": [{"name": "X-Api-Key", "value": "{key}"}]},
        {"type": "streamable-http", "url": "https://{tenant}.example/mcp"}
    ]});
    let setups = listing(&page(server, "active"))["servers"][0]["setups"].clone();
    assert_eq!(setups[0]["sign_in"], "token");
    assert_eq!(setups[1]["sign_in"], "detect");
    assert_eq!(setups[2]["unsupported"], "needs custom request headers");
    assert_eq!(setups[3]["unsupported"], "needs values filled into its URL");
}

#[test]
fn a_choice_becomes_an_add_command_with_the_cursor_where_input_is_needed() {
    assert_eq!(short_name("io.github.acme/postgres-mcp-server"), "postgres");
    assert_eq!(
        short_name("com.pulsemcp/remote-filesystem"),
        "remote-filesystem"
    );
    let local = json!({"kind": "local", "command": ["npx", "-y", "pg@1.0"], "variables": [
        {"name": "PG_URL", "required": true},
        {"name": "PG_TOKEN", "secret": true},
        {"name": "LOG", "default": "info"},
        {"name": "OPTIONAL"}
    ]});
    let (line, cursor) = add_command("io.github.acme/pg-mcp", &local).unwrap();
    assert_eq!(
        line,
        "/mcp add pg --env PG_URL= --env PG_TOKEN=${key} --env LOG=info --key  -- npx -y pg@1.0"
    );
    assert_eq!(
        &line[..cursor],
        "/mcp add pg --env PG_URL= --env PG_TOKEN=${key} --env LOG=info --key "
    );
    let remote = json!({"kind": "remote", "url": "https://x.example/mcp", "sign_in": "token"});
    let (line, cursor) = add_command("x", &remote).unwrap();
    assert_eq!(
        (line.as_str(), cursor),
        (
            "/mcp add x --url https://x.example/mcp --bearer ",
            line.len()
        )
    );
    assert!(
        add_command(
            "x",
            &json!({"kind": "remote", "unsupported": "needs custom request headers"})
        )
        .is_none()
    );
}

#[test]
fn a_filled_in_line_is_one_mcp_add_accepts() {
    let local = json!({"kind": "local", "command": ["npx", "-y", "pg@1.0"], "variables": [
        {"name": "PG_TOKEN", "secret": true}, {"name": "LOG", "default": "info"}
    ]});
    let (line, cursor) = add_command("io.github.acme/pg-mcp", &local).unwrap();
    let typed = format!("{}sk-typed{}", &line[..cursor], &line[cursor..]);
    let args = typed.strip_prefix("/mcp add ").unwrap().split_whitespace();
    let parsed = crate::config::parse_mcp_add_args(args).unwrap();
    assert_eq!(parsed.id, "pg");
    assert_eq!(parsed.key.as_deref(), Some("sk-typed"));
    assert_eq!(parsed.server.command, ["npx", "-y", "pg@1.0"]);
    assert_eq!(parsed.server.env["PG_TOKEN"], "${key}");
    assert_eq!(parsed.server.env["LOG"], "info");
}

#[test]
fn deprecated_servers_and_unknown_package_types_are_not_offered() {
    let deprecated = listing(&page(json!({"name": "old"}), "deprecated"));
    assert!(deprecated["servers"].as_array().unwrap().is_empty());
    let docker = json!({"name": "d", "packages": [{"registryType": "oci", "identifier": "img", "transport": {"type": "stdio"}}]});
    let setup = &listing(&page(docker, "active"))["servers"][0]["setups"][0];
    assert!(setup["unsupported"].as_str().unwrap().contains("oci"));
}
