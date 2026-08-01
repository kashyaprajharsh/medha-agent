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

/// A provider failure after the kernel session has started must reach the
/// process exit status. Printing an error and returning `Ok(())` made CI accept
/// failed agent runs as successful.
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
