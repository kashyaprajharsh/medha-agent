//! `medha gate` scoring real runs against a scripted model.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn request(stream: &mut TcpStream) -> (String, Value) {
    let mut buffer = Vec::new();
    let mut chunk = [0; 16384];
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            return (String::new(), Value::Null);
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(end) = buffer.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
        let length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0);
        if buffer.len() >= end + 4 + length {
            let body =
                serde_json::from_slice(&buffer[end + 4..end + 4 + length]).unwrap_or(Value::Null);
            return (head, body);
        }
    }
}

fn reply(stream: &mut TcpStream, delta: Value, finish: &str) {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"index": 0, "delta": delta}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}),
    );
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn call(name: &str, arguments: Value) -> Value {
    json!({"role": "assistant", "tool_calls": [{"index": 0, "id": format!("{name}-1"), "type": "function",
        "function": {"name": name, "arguments": arguments.to_string()}}]})
}

/// The task starts a sub-agent that reads a file; each finishes once it has a tool result.
fn scripted_model(asked: Arc<AtomicUsize>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let asked = asked.clone();
            std::thread::spawn(move || {
                let (head, body) = request(&mut stream);
                if !head.starts_with("POST /v1/chat/completions ") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
                asked.fetch_add(1, Ordering::SeqCst);
                let answered = body["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| message["role"] == "tool")
                });
                let child = body["messages"].to_string().contains("focused sub-agent");
                match (child, answered) {
                    (_, true) => reply(
                        &mut stream,
                        json!({"role": "assistant", "content": "done"}),
                        "stop",
                    ),
                    (true, false) => reply(
                        &mut stream,
                        call("read", json!({"path": "notes.txt"})),
                        "tool_calls",
                    ),
                    (false, false) => reply(
                        &mut stream,
                        call(
                            "agent_spawn",
                            json!({"objective": "say hello", "wait": true}),
                        ),
                        "tool_calls",
                    ),
                }
            });
        }
    });
    url
}

/// The gate's JSON report for one scenario with `checks`, run with `extra` arguments.
fn gate(checks: &[&str], extra: &[&str]) -> Value {
    let (output, _) = gate_run(checks, extra);
    let printed = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&printed)
        .unwrap_or_else(|_| panic!("{printed}\n{}", String::from_utf8_lossy(&output.stderr)))
}

/// The gate's raw output for one scenario with `checks`, and how many model requests it made.
fn gate_run(checks: &[&str], extra: &[&str]) -> (std::process::Output, usize) {
    let root = tempfile::tempdir().unwrap();
    let scenario = root.path().join("scenarios/delegates");
    std::fs::create_dir_all(scenario.join("fixture")).unwrap();
    std::fs::write(scenario.join("fixture/notes.txt"), "hello\n").unwrap();
    let listed: String = checks
        .iter()
        .map(|check| format!("  - {check}\n"))
        .collect();
    std::fs::write(
        scenario.join("scenario.yaml"),
        format!("id: delegates\ntask: Delegate a greeting.\ncontract:\n  mode: yolo\n  max_turns: 6\n  max_wall_s: 120\nchecks:\n{listed}"),
    )
    .unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let asked = Arc::new(AtomicUsize::new(0));
    let output = Command::new(env!("CARGO_BIN_EXE_medha"))
        .args(["gate", "scenarios", "--json"])
        .args(extra)
        .current_dir(root.path())
        .env("MEDHA_HOME", &home)
        .env("MEDHA_BASE_URL", scripted_model(asked.clone()))
        .env("MEDHA_MODEL", "test-model")
        .env("MEDHA_API_KEY", "test-key")
        .env("MEDHA_PROTOCOL", "open-ai-chat")
        .env("MEDHA_MAX_CTX", "32000")
        .output()
        .unwrap();
    (output, asked.load(Ordering::SeqCst))
}

/// The gate's checks for one scenario run, by label.
fn checks(checks: &[&str]) -> Vec<(String, bool, String)> {
    let report = gate(checks, &["--seeds", "1"]);
    report["scenarios"][0]["seeds"][0]["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("{report}"))
        .iter()
        .map(|check| {
            (
                check["label"].as_str().unwrap_or_default().to_owned(),
                check["passed"] == true,
                check["detail"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// A tool a sub-agent calls counts against a scenario that forbids it.
#[test]
fn a_sub_agents_calls_are_scored_with_the_task() {
    let scored = checks(&["tool_not_used: read"]);
    assert_eq!(scored.len(), 1, "{scored:?}");
    assert!(!scored[0].1, "the sub-agent's read was missed: {scored:?}");
}

/// One passing run cannot promote a scenario; the default run count can.
#[test]
fn a_single_passing_run_holds_until_enough_runs_pass() {
    let once = gate(&["tool_used: read"], &["--seeds", "1"]);
    let scenario = &once["scenarios"][0];
    assert!(scenario["seeds"][0]["passed"] == true, "{once}");
    assert_eq!(scenario["verdict"], "HOLD", "{once}");
    assert!(
        scenario["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("1 of 3")),
        "{once}"
    );
    let default = gate(&["tool_used: read"], &[]);
    assert_eq!(
        default["scenarios"][0]["seeds"].as_array().map(Vec::len),
        Some(3),
        "{default}"
    );
    assert_eq!(default["scenarios"][0]["verdict"], "PROMOTE", "{default}");
}

/// A command check the sandbox cannot run stops the gate before any model call, saying how to enable it.
#[test]
fn a_command_check_that_cannot_run_stops_before_the_model_is_asked() {
    let (output, asked) = gate_run(
        &["command: { run: \"true\", expect_exit: 0 }"],
        &["--seeds", "1"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert_eq!(
        asked, 0,
        "the model was asked before the gate refused: {stderr}"
    );
    assert!(
        stderr.contains("delegates") && stderr.contains("backend = \"container\""),
        "{stderr}"
    );
}

/// A check naming a tool the run never offered fails and says so, instead of passing unseen.
#[test]
fn a_check_naming_a_tool_that_does_not_exist_fails_loudly() {
    let scored = checks(&["tool_not_used: fs.write"]);
    assert!(
        !scored[0].1 && scored[0].2.contains("never offered"),
        "{scored:?}"
    );
}
