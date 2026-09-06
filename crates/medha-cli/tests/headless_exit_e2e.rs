use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::Duration;

fn configured_medha(workspace: &std::path::Path, home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_medha"));
    command
        .arg("perform one test turn")
        .current_dir(workspace)
        .env("MEDHA_HOME", home)
        .env("MEDHA_BASE_URL", "http://127.0.0.1:1/v1")
        .env("MEDHA_MODEL", "test-model")
        .env("MEDHA_API_KEY", "test-key")
        .env("MEDHA_PROTOCOL", "open-ai-chat")
        .env("MEDHA_TOKEN_ACCOUNTING", "adaptive");
    command
}

#[test]
fn a_headless_provider_failure_exits_nonzero() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = [0u8; 8192];
                    let _ = stream.read(&mut request);
                    let body = br#"{"error":{"message":"deliberate provider rejection"}}"#;
                    write!(
                        stream,
                        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(body).unwrap();
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "Medha never contacted the fake provider"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fake provider failed: {error}"),
            }
        }
    });

    let output = configured_medha(&workspace, &home)
        .env("MEDHA_BASE_URL", format!("http://{address}/v1"))
        .output()
        .unwrap();
    server.join().unwrap();

    assert!(
        !output.status.success(),
        "headless provider failure incorrectly exited zero\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("headless run failed"),
        "failure was not propagated clearly: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_malformed_present_lockfile_fails_before_running_the_agent() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let lock = workspace.join("medha.lock");
    std::fs::write(&lock, "[sandbox\nnetwork = \"deny\"\n").unwrap();

    let output = configured_medha(&workspace, &home).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("could not parse medha.lock"), "{stderr}");
    assert!(stderr.contains(&lock.display().to_string()), "{stderr}");
}

#[test]
fn an_unreadable_present_lockfile_fails_instead_of_using_defaults() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let lock = workspace.join("medha.lock");
    // A directory is present at the exact configuration path and cannot be
    // read as TOML on every supported platform.
    std::fs::create_dir(&lock).unwrap();

    let output = configured_medha(&workspace, &home).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("could not read medha.lock"), "{stderr}");
    assert!(stderr.contains(&lock.display().to_string()), "{stderr}");
}

#[test]
fn malformed_budget_environment_fails_before_provider_access() {
    let root = tempfile::tempdir().unwrap();
    for (name, value) in [
        ("MEDHA_MAX_COST", "NaN"),
        ("MEDHA_MAX_COST", "inf"),
        ("MEDHA_MAX_COST", "-1"),
        ("MEDHA_MAX_TOKENS", "not-a-number"),
        ("MEDHA_MAX_WALL", "-1"),
        ("MEDHA_MAX_PARALLEL_TOOLS", "abc"),
    ] {
        let output = configured_medha(root.path(), &root.path().join("home"))
            .env(name, value)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(name), "{name}: {stderr}");
        assert!(
            !stderr.contains("headless run failed"),
            "contacted provider: {stderr}"
        );
    }
}

#[test]
fn required_verification_needs_a_command_before_provider_access() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("medha.lock"),
        "[verify]\nrequired = true\n",
    )
    .unwrap();
    let output = configured_medha(root.path(), &root.path().join("home"))
        .env_remove("MEDHA_VERIFY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("required verification needs"));
}

#[test]
fn invalid_execution_mode_fails_before_provider_access() {
    let root = tempfile::tempdir().unwrap();
    let output = configured_medha(root.path(), &root.path().join("home"))
        .env("MEDHA_MODE", "yoloo")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown mode"));
}

/// Exercise the real binary, HTTP adapter, command verifier, and exit status.
#[test]
fn required_completion_failure_exits_nonzero_but_plan_does_not_run_checks() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let server_done = done.clone();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !server_done.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let read = stream.read(&mut chunk).unwrap_or(0);
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .and_then(|n| n.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                assert!(request.len() < 2_000_000);
            }
            let is_chat =
                String::from_utf8_lossy(&request).starts_with("POST /v1/chat/completions");
            let (content_type, body) = if is_chat {
                (
                    "text/event-stream",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Done.\"}}]}\n\ndata: [DONE]\n\n",
                )
            } else {
                (
                    "application/json",
                    "{\"data\":[{\"id\":\"test-model\",\"context_length\":32000}]}",
                )
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let root = tempfile::tempdir().unwrap();
    for (mode, check, success) in [
        ("normal", "exit 7", false),
        ("normal", "exit 0", true),
        ("plan", "echo checked > should-not-exist", true),
    ] {
        let ws = root.path().join(format!("{mode}-{success}"));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("medha.lock"), format!("[verify]\ncommand = {check:?}\nrequired = true\ntimeout_s = 2\n[agents]\nenabled = false\n[lsp]\nenabled = false\n")).unwrap();
        let output = configured_medha(&ws, &ws.join("home"))
            .env("MEDHA_MODE", mode)
            .env_remove("MEDHA_VERIFY")
            .env("MEDHA_BASE_URL", format!("http://{address}/v1"))
            .env("MEDHA_MAX_CTX", "32000")
            .env("MEDHA_MAX_WALL", "5")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.success(),
            success,
            "{mode}: {stderr}\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        if !success {
            assert!(stderr.contains("completion blocked"), "{stderr}");
        }
        assert!(!ws.join("should-not-exist").exists());
    }
    done.store(true, Ordering::SeqCst);
    server.join().unwrap();
}
