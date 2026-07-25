//! End-to-end and adversarial coverage against a hermetic stdio MCP server (a
//! small dependency-free Python script whose behaviour is selected by argv).
//! Exercises the real spawn + sandbox + rmcp transport + manager path. Skips
//! when python3 is unavailable.

use std::{path::Path, time::Duration};

use mcp::{Config, Error, McpManager, ServerConfig, ServerState, ToolFilter, Transport};
use serde_json::json;

/// Modes: `normal`, `hostile` (malformed tool names), `churn` (announces
/// tools/list_changed), `flaky <marker>` (exits once, then behaves).
const FAKE_SERVER: &str = r#"
import sys, json, os, time, subprocess

mode = sys.argv[1] if len(sys.argv) > 1 else "normal"
marker = sys.argv[2] if len(sys.argv) > 2 else None
die_after_call = mode == "flaky" and marker and not os.path.exists(marker)
if die_after_call:
    open(marker, "w").close()
grew = False

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n"); sys.stdout.flush()

def spec(name, required=None):
    schema = {"type": "object", "properties": {"text": {"type": "string"}}}
    if required: schema["required"] = required
    return {"name": name, "description": "d", "inputSchema": schema}

def catalog():
    if mode == "hostile":
        return [spec("bad__name"), spec("new\nline"), spec("x" * 200), spec("fine")]
    tools = [spec("echo", ["text"]), spec("slow"), spec("leak"), spec("big"), spec("spawn")]
    if mode == "churn":
        tools.append(spec("grow"))
        if grew: tools.append(spec("sprouted"))
    return tools

for line in sys.stdin:
    line = line.strip()
    if not line: continue
    msg = json.loads(line); mid = msg.get("id"); method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion": msg["params"].get("protocolVersion","2025-06-18"),
            "capabilities":{"tools":{"listChanged":True}},
            "serverInfo":{"name":"fake","version":"0.0.1"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":catalog()}})
    elif method == "tools/call":
        params = msg.get("params",{}); name = params.get("name"); args = params.get("arguments",{})
        if name == "slow":
            time.sleep(30)
        if name == "grow":
            grew = True
            send({"jsonrpc":"2.0","method":"notifications/tools/list_changed"})
        if name == "spawn":
            child = subprocess.Popen(["sleep", "300"])
            text = str(child.pid)
        elif name == "leak":
            text = "\n".join(sorted("%s=%s" % kv for kv in os.environ.items()))
        elif name == "big":
            text = "A" * 50000
        else:
            text = "echo: " + str(args.get("text",""))
        send({"jsonrpc":"2.0","id":mid,"result":{
            "content":[{"type":"text","text":text}],"isError":False}})
        if die_after_call:
            os._exit(1)
    elif mid is not None:
        send({"jsonrpc":"2.0","id":mid,"result":{}})
"#;

struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new() -> Option<Self> {
        if !sandbox::program_on_path("python3") {
            eprintln!("skipping MCP e2e: python3 is unavailable");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.py"), FAKE_SERVER).unwrap();
        Some(Self { dir })
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn command(&self, mode: &str, extra: Option<&str>) -> Vec<String> {
        let mut command = vec![
            "python3".to_string(),
            self.dir
                .path()
                .join("server.py")
                .to_string_lossy()
                .into_owned(),
            mode.to_string(),
        ];
        command.extend(extra.map(str::to_string));
        command
    }
}

fn config(server: ServerConfig) -> Config {
    Config {
        enabled: true,
        servers: vec![server],
        startup_timeout: Duration::from_secs(20),
        request_timeout: Duration::from_secs(10),
        health_interval: Duration::from_millis(200),
        ..Config::default()
    }
}

fn server(id: &str, command: Vec<String>) -> ServerConfig {
    ServerConfig {
        id: id.into(),
        transport: Transport::Stdio {
            command,
            env: Vec::new(),
        },
        ..Default::default()
    }
}

/// Poll `check` until it holds or the budget runs out.
async fn wait_for(label: &str, mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..150 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {label}");
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connects_lists_and_calls() {
    let Some(fake) = Fake::new() else { return };
    let manager = McpManager::new(
        fake.path().to_path_buf(),
        config(server("fake", fake.command("normal", None))),
    );
    manager.connect_startup().await;

    let names: Vec<String> = manager.tool_specs().into_iter().map(|s| s.name).collect();
    assert!(
        names.contains(&"mcp__fake__echo".to_string()),
        "expected the echo tool, got {names:?}"
    );

    let out = manager
        .call("mcp__fake__echo", &json!({ "text": "hi" }))
        .await
        .expect("echo call failed");
    assert!(!out.is_error);
    assert!(out.text.contains("echo: hi"), "unexpected: {}", out.text);

    let status = &manager.status().await[0];
    assert_eq!(status.state, ServerState::Ready);
    assert_eq!(status.tools, 5);
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_keeps_unwanted_tools_out_of_context() {
    let Some(fake) = Fake::new() else { return };
    let mut definition = server("fake", fake.command("normal", None));
    definition.tools = ToolFilter {
        allow: vec!["echo".into(), "b*".into()],
        deny: vec!["big".into()],
    };
    let manager = McpManager::new(fake.path().to_path_buf(), config(definition));
    manager.connect_startup().await;

    let names: Vec<String> = manager.tool_specs().into_iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["mcp__fake__echo".to_string()]);
    let status = &manager.status().await[0];
    assert_eq!((status.tools, status.hidden), (1, 4));

    // A filtered tool is not addressable even when named exactly.
    assert!(matches!(
        manager.call("mcp__fake__big", &json!({})).await,
        Err(Error::UnknownTool { .. })
    ));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_tool_names_are_dropped() {
    let Some(fake) = Fake::new() else { return };
    let manager = McpManager::new(
        fake.path().to_path_buf(),
        config(server("fake", fake.command("hostile", None))),
    );
    manager.connect_startup().await;

    let names: Vec<String> = manager.tool_specs().into_iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["mcp__fake__fine".to_string()]);
    assert_eq!(manager.status().await[0].hidden, 3);
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arguments_are_validated_before_the_round_trip() {
    let Some(fake) = Fake::new() else { return };
    let manager = McpManager::new(
        fake.path().to_path_buf(),
        config(server("fake", fake.command("normal", None))),
    );
    manager.connect_startup().await;

    assert!(matches!(
        manager.call("mcp__fake__echo", &json!({})).await,
        Err(Error::BadArguments { .. })
    ));
    assert!(matches!(
        manager.call("mcp__fake__nope", &json!({})).await,
        Err(Error::UnknownTool { .. })
    ));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn medha_secrets_never_reach_the_server() {
    let Some(fake) = Fake::new() else { return };
    // SAFETY: set before any child spawn in this single-threaded test body.
    unsafe { std::env::set_var("MEDHA_TEST_SECRET", "do-not-leak") };
    let mut definition = server("fake", fake.command("normal", None));
    if let Transport::Stdio { env, .. } = &mut definition.transport {
        env.push(("SERVER_TOKEN".into(), "granted".into()));
    }
    let manager = McpManager::new(fake.path().to_path_buf(), config(definition));
    manager.connect_startup().await;

    let out = manager.call("mcp__fake__leak", &json!({})).await.unwrap();
    assert!(
        !out.text.contains("do-not-leak"),
        "secret leaked into the server env"
    );
    assert!(
        out.text.contains("SERVER_TOKEN=granted"),
        "explicit env missing"
    );
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_output_survives_intact_for_the_artifact_layer() {
    let Some(fake) = Fake::new() else { return };
    let manager = McpManager::new(
        fake.path().to_path_buf(),
        config(server("fake", fake.command("normal", None))),
    );
    manager.connect_startup().await;

    let out = manager.call("mcp__fake__big", &json!({})).await.unwrap();
    assert_eq!(out.text.len(), 50_000);
    assert!(manager.max_text_chars() < out.text.len());
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_calls_time_out_without_killing_the_session() {
    let Some(fake) = Fake::new() else { return };
    let mut cfg = config(server("fake", fake.command("normal", None)));
    cfg.request_timeout = Duration::from_millis(300);
    let manager = McpManager::new(fake.path().to_path_buf(), cfg);
    manager.connect_startup().await;

    assert!(matches!(
        manager.call("mcp__fake__slow", &json!({})).await,
        Err(Error::Timeout(_))
    ));
    assert_eq!(manager.status().await[0].state, ServerState::Ready);
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crashed_server_reconnects() {
    let Some(fake) = Fake::new() else { return };
    let marker = fake.path().join("crashed").to_string_lossy().into_owned();
    let manager = McpManager::new(
        fake.path().to_path_buf(),
        config(server("fake", fake.command("flaky", Some(&marker)))),
    );
    manager.connect_startup().await;
    manager
        .call("mcp__fake__echo", &json!({ "text": "x" }))
        .await
        .unwrap();

    // The supervisor notices the dead transport and reconnects on backoff.
    wait_for("degraded", async || {
        manager.status().await[0].state != ServerState::Ready
    })
    .await;
    wait_for("reconnect", async || {
        manager.status().await[0].state == ServerState::Ready
    })
    .await;
    manager
        .call("mcp__fake__echo", &json!({ "text": "again" }))
        .await
        .expect("call after reconnect failed");
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_failures_park_instead_of_hot_looping() {
    let Some(fake) = Fake::new() else { return };
    let mut cfg = config(server(
        "fake",
        vec!["python3".into(), "-c".into(), "raise SystemExit(1)".into()],
    ));
    cfg.max_reconnects = 2;
    cfg.startup_timeout = Duration::from_secs(5);
    let manager = McpManager::new(fake.path().to_path_buf(), cfg);
    manager.connect_startup().await;

    wait_for("parked", async || {
        manager.status().await[0].state == ServerState::Parked
    })
    .await;
    // Parked servers stay callable-as-errors, reporting why.
    let error = manager
        .call("mcp__fake__echo", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.server_state(), Some(ServerState::Parked));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_changed_refreshes_the_catalogue() {
    let Some(fake) = Fake::new() else { return };
    let manager = McpManager::new(
        fake.path().to_path_buf(),
        config(server("fake", fake.command("churn", None))),
    );
    manager.connect_startup().await;
    assert!(
        !manager
            .tool_specs()
            .iter()
            .any(|s| s.name.ends_with("sprouted"))
    );

    manager.call("mcp__fake__grow", &json!({})).await.unwrap();
    wait_for("catalogue refresh", async || {
        manager
            .tool_specs()
            .iter()
            .any(|s| s.name.ends_with("sprouted"))
    })
    .await;
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_calls_to_one_server_are_serialized() {
    let Some(fake) = Fake::new() else { return };
    let manager = McpManager::new(
        fake.path().to_path_buf(),
        config(server("fake", fake.command("normal", None))),
    );
    manager.connect_startup().await;

    let calls = (0..4).map(|i| {
        let manager = manager.clone();
        async move {
            manager
                .call("mcp__fake__echo", &json!({ "text": i.to_string() }))
                .await
        }
    });
    for result in futures::future::join_all(calls).await {
        assert!(result.is_ok(), "concurrent call failed: {result:?}");
    }
    manager.shutdown().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_reaps_the_whole_process_tree() {
    let Some(fake) = Fake::new() else { return };
    let manager = McpManager::new(
        fake.path().to_path_buf(),
        config(server("fake", fake.command("normal", None))),
    );
    manager.connect_startup().await;

    // The server forks a grandchild; only a process-group kill reaps it.
    let out = manager.call("mcp__fake__spawn", &json!({})).await.unwrap();
    let grandchild: u32 = out.text.trim().parse().expect("grandchild pid");
    assert!(alive(grandchild));

    manager.shutdown().await;
    wait_for("grandchild reap", async || !alive(grandchild)).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_manager_leaves_no_orphan() {
    let Some(fake) = Fake::new() else { return };
    let manager = McpManager::new(
        fake.path().to_path_buf(),
        config(server("fake", fake.command("normal", None))),
    );
    manager.connect_startup().await;
    let out = manager.call("mcp__fake__spawn", &json!({})).await.unwrap();
    let grandchild: u32 = out.text.trim().parse().expect("grandchild pid");

    drop(manager);
    wait_for("orphan reap", async || !alive(grandchild)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_server_is_never_connected() {
    let Some(fake) = Fake::new() else { return };
    let mut definition = server("fake", fake.command("normal", None));
    definition.disabled = true;
    let manager = McpManager::new(fake.path().to_path_buf(), config(definition));
    manager.connect_startup().await;

    // Kept in the config, but nothing spawned and nothing in the model's context.
    assert_eq!(manager.status().await[0].state, ServerState::Disabled);
    assert!(manager.tool_specs().is_empty());
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tool_browser_lists_filtered_tools_as_switched_off() {
    let Some(fake) = Fake::new() else { return };
    let mut definition = server("fake", fake.command("normal", None));
    definition.tools = ToolFilter {
        allow: Vec::new(),
        deny: vec!["big".into(), "slow".into()],
    };
    let manager = McpManager::new(fake.path().to_path_buf(), config(definition));
    manager.connect_startup().await;

    let tools = manager.server_tools("fake");
    // Every tool the server offers is listed; the filtered ones are just off.
    assert_eq!(tools.len(), 5);
    assert_eq!(
        tools
            .iter()
            .filter(|(_, on)| !on)
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["big", "slow"]
    );
    assert!(tools.iter().any(|(name, on)| name == "echo" && *on));
    manager.shutdown().await;
}
