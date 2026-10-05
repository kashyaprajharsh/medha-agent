use super::*;

#[tokio::test]
async fn a_server_switched_off_is_not_told_to_the_agent_and_is_still_there_for_settings() {
    let server = |id: &str, disabled| mcp::ServerConfig {
        id: id.into(),
        transport: mcp::Transport::Stdio {
            command: vec!["this-program-must-never-run".into()],
            env: Vec::new(),
        },
        requires_approval: true,
        disabled,
        ..Default::default()
    };
    let manager = Arc::new(mcp::McpManager::new(
        std::path::PathBuf::from("."),
        mcp::Config {
            enabled: true,
            servers: vec![server("kept-on", false), server("switched-off", true)],
            ..mcp::Config::default()
        },
    ));
    let tool = McpStatus {
        manager: Arc::clone(&manager),
    };

    let told = tool.execute(&json!({})).await.unwrap();
    let named: Vec<&str> = told["servers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|server| server["server"].as_str())
        .collect();
    assert_eq!(
        named,
        ["kept-on"],
        "the agent was told of a server that is off"
    );
    assert_eq!(manager.status().await.len(), 2);
}
