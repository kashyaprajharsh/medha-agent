use super::*;

#[test]
fn only_trusted_remote_servers_are_shared_so_an_approval_never_spreads() {
    let remote = |trust: &str| config::McpServer {
        url: "https://mcp.example/mcp".into(),
        trust: trust.into(),
        ..Default::default()
    };
    assert!(is_shared(&remote("trusted")));
    assert!(
        !is_shared(&remote("workspace")),
        "approved per chat, never for all"
    );
    assert!(!is_shared(&remote("")), "the default needs approval");
    let local = config::McpServer {
        command: vec!["uvx".into(), "server".into()],
        trust: "trusted".into(),
        ..Default::default()
    };
    assert!(!is_shared(&local), "a command stays jailed in its own chat");
    let off = config::McpServer {
        disabled: true,
        ..remote("trusted")
    };
    assert!(
        is_shared(&off),
        "a disconnected server is still the host's to show as off"
    );
}
