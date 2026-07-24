//! End-to-end: connect to a hermetic stdio MCP server (a ~30-line Python echo
//! server, no third-party deps), list its tools, and call one — exercising the
//! real spawn + rmcp transport + manager path. Skips if python3 is absent.

use std::time::Duration;

use mcp::{Config, McpManager, ServerConfig};

const FAKE_SERVER: &str = r#"
import sys, json
def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    msg = json.loads(line); mid = msg.get("id"); method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion": msg["params"].get("protocolVersion","2025-06-18"),
            "capabilities":{"tools":{}},
            "serverInfo":{"name":"fake","version":"0.0.1"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[
            {"name":"echo","description":"Echo text back",
             "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}})
    elif method == "tools/call":
        args = msg.get("params",{}).get("arguments",{})
        send({"jsonrpc":"2.0","id":mid,"result":{
            "content":[{"type":"text","text":"echo: "+str(args.get("text",""))}],"isError":False}})
    elif mid is not None:
        send({"jsonrpc":"2.0","id":mid,"result":{}})
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_server_end_to_end() {
    if !sandbox::program_on_path("python3") {
        eprintln!("skipping MCP echo E2E: python3 is unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("echo_server.py");
    std::fs::write(&script, FAKE_SERVER).unwrap();

    let manager = McpManager::new(
        dir.path().to_path_buf(),
        Config {
            enabled: true,
            servers: vec![ServerConfig {
                id: "fake".into(),
                command: vec!["python3".into(), script.to_string_lossy().into_owned()],
                env: Vec::new(),
                requires_approval: false,
            }],
            startup_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(10),
            max_text_chars: 16_000,
            allow_network: true,
        },
    );

    tokio::time::timeout(Duration::from_secs(20), manager.connect_startup())
        .await
        .expect("MCP startup timed out");

    let specs = manager.tool_specs();
    assert!(
        specs.iter().any(|s| s.name == "mcp__fake__echo"),
        "expected the echo tool to be projected, got {:?}",
        specs.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    let out = manager
        .call("mcp__fake__echo", &serde_json::json!({ "text": "hi" }))
        .await
        .expect("echo call failed");
    assert!(!out.is_error);
    assert!(out.text.contains("echo: hi"), "unexpected output: {}", out.text);

    let status = manager.status().await;
    assert!(status.iter().any(|s| s.server == "fake" && s.tools == 1));

    manager.shutdown().await;
}
