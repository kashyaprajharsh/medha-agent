//! Budget the actual CLI request, including the system prompt and wire schema.
//! Byte bounds are portable regression guards, not tokenizer-independent tokens.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

fn captured_request(preset: &str) -> serde_json::Value {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let skill = workspace.join(".medha/skills/test-procedure");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"),
        "---\nname: test-procedure\ndescription: test installed skill\nrequired_tools: [shell.exec]\n---\nRun a command.\n").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "no request reached the mock provider"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let (body_start, length) = loop {
                let mut chunk = [0u8; 8192];
                let read = stream.read(&mut chunk).unwrap();
                assert!(read > 0, "incomplete HTTP request");
                request.extend_from_slice(&chunk[..read]);
                assert!(request.len() < 1_000_000);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")?
                                .trim()
                                .parse::<usize>()
                                .ok()
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break (end + 4, length);
                    }
                }
            };
            let chat = request.starts_with(b"POST /v1/chat/completions");
            let (kind, body) = if chat {
                (
                    "text/event-stream",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Done.\"}}]}\n\ndata: [DONE]\n\n",
                )
            } else {
                (
                    "application/json",
                    "{\"data\":[{\"id\":\"test-model\",\"context_length\":128000}]}",
                )
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            if chat {
                return serde_json::from_slice(&request[body_start..body_start + length]).unwrap();
            }
        }
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_medha"));
    // No developer profile, credentials, or prompt override may affect the fixture.
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("MEDHA_")) {
        command.env_remove(key);
    }
    let output = command
        .arg("hii")
        .current_dir(&workspace)
        .env("MEDHA_HOME", &home)
        .env("MEDHA_BASE_URL", format!("http://{address}/v1"))
        .env("MEDHA_MODEL", "test-model")
        .env("MEDHA_AUTH", "none")
        .env("MEDHA_PROTOCOL", "open-ai-chat")
        .env("MEDHA_MAX_CTX", "128000")
        .env("MEDHA_MAX_TURNS", "1")
        .env("MEDHA_TOOLS", preset)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap()
}

#[test]
fn full_and_minimal_wire_requests_obey_their_capabilities_and_budgets() {
    let full = captured_request("full");
    let minimal = captured_request("minimal");
    for (request, count, max_bytes) in [(&full, 24, 45_000), (&minimal, 5, 14_000)] {
        assert_eq!(request["tools"].as_array().unwrap().len(), count);
        let bytes = serde_json::to_vec(request).unwrap().len();
        assert!(
            bytes <= max_bytes,
            "assembled request is {bytes} bytes, limit {max_bytes}"
        );
    }
    let system = |request: &serde_json::Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "system")
            .filter_map(|message| message["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let full_system = system(&full);
    let minimal_system = system(&minimal);
    assert!(full_system.contains("test-procedure"));
    assert!(full_system.contains("`update_plan`"));
    for absent in [
        "test-procedure",
        "`update_plan`",
        "`clarify`",
        "`agent.spawn`",
        "`skill`",
        "`memory`",
        "`lsp`",
    ] {
        assert!(
            !minimal_system.contains(absent),
            "minimal prompt refers to {absent}"
        );
    }
    let mut names: Vec<_> = minimal["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["edit", "glob", "grep", "read", "shell_exec"]);
    assert!(minimal_system.contains("trust boundary"));
}
