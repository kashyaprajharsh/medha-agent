use super::*;

#[tokio::test]
async fn malformed_or_unsupported_blocks_do_not_become_a_partial_prompt() {
    for value in [
        json!([{"type":"text","text":"keep"},{"type":"audio","data":"ignored"}]),
        json!([{"type":"image"}]),
        json!([]),
        json!({"text":"wrong"}),
    ] {
        assert!(content::prompt(value).await.is_err());
    }
    let message = content::prompt(json!([{"type":"text","text":"hello"},
        {"type":"resource","resource":{"text":"attached","uri":"file:///unused"}}]))
    .await
    .unwrap();
    assert_eq!(message.content, "hello\nattached");
    assert!(matches!(message.intent, Some(protocol::SendIntent::Start)));
}

#[test]
fn editor_mcp_rejects_remote_and_duplicate_environment_entries() {
    let command = std::env::current_exe().unwrap();
    let params: Open = serde_json::from_value(json!({"cwd":std::env::temp_dir(),
        "mcpServers":[{"name":"test","command":command,"args":[],"env":[
            {"name":"TOKEN","value":"one"},{"name":"TOKEN","value":"two"}]}]}))
    .unwrap();
    assert!(params.servers().is_err());
    let params: Open = serde_json::from_value(json!({"cwd":std::env::temp_dir(),
        "mcpServers":[{"type":"http","name":"test","command":command,"args":[],"env":[]}]}))
    .unwrap();
    assert!(params.servers().is_err());
}
