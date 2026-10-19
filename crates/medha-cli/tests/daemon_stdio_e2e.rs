use serde_json::json;
use std::{
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

#[test]
fn trust_cli_closes_its_output_while_the_backend_stays_alive() {
    let files = tempfile::tempdir().unwrap();
    let home = files.path().join("home");
    let folder = files.path().join("project");
    std::fs::create_dir_all(&folder).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_medha"))
        .args(["trust", "--yes"])
        .current_dir(&folder)
        .env("MEDHA_HOME", &home)
        .env("MEDHA_CRED_STORE", "file")
        .env("MEDHA_CATALOGUE_OFFLINE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (finished, result) = mpsc::channel();
    let reader = std::thread::spawn(move || finished.send(child.wait_with_output()).ok());
    let output = result.recv_timeout(Duration::from_secs(20));
    // Join the still-live daemon and retire it even if leaked handles delayed EOF.
    // The result above must have arrived before its shutdown, not because of it.
    let read = |name| std::fs::read_to_string(home.join("serve").join(name)).unwrap();
    // Join only: silently starting another daemon would not prove it survived.
    let service = medha_client::Backend::joined_to(read("address"), &read("token"));
    let connection = service.connection().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(connection.request(json!({"method":"backend.status"})))
        .unwrap();
    // Shutdown can close the transport before its acknowledgement is delivered.
    let _ =
        runtime.block_on(connection.request(
            json!({"method":"backend.prepare_upgrade", "params":{"build":wire::BUILD_ID}}),
        ));
    reader.join().unwrap();
    let output = output
        .expect("trust output waited for the shared backend to exit")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Checkout trusted"));
}
