#![cfg(unix)]

use runtime::project_trust::{ProjectTrust, register_lsp, revoke_live};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct ServerCleanup(PathBuf);
impl Drop for ServerCleanup {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|text| text.parse::<i32>().ok())
        {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_process_stops_its_server_at_next_use_without_interrupting_its_chat() {
    let files = tempfile::tempdir().unwrap();
    let directory = files.path().canonicalize().unwrap();
    let workspace = directory.join("checkout");
    let state = directory.join("state");
    std::fs::create_dir_all(workspace.join(".git")).unwrap();
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    unsafe {
        std::env::set_var("MEDHA_HOME", &state);
    }
    let script = workspace.join("server.py");
    std::fs::write(&script, r#"import json, os, sys
open('server.pid', 'w').write(str(os.getpid()))
while True:
    size = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line.lower().startswith(b'content-length:'): size = int(line.split(b':')[1])
        if line in (b'\r\n', b'\n'): break
    request = json.loads(sys.stdin.buffer.read(size))
    if 'id' not in request: continue
    result = {'capabilities': {'hoverProvider': True}} if request.get('method') == 'initialize' else {'contents': 'ready'}
    reply = json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}).encode()
    sys.stdout.buffer.write(b'Content-Length: ' + str(len(reply)).encode() + b'\r\n\r\n' + reply)
    sys.stdout.buffer.flush()
"#).unwrap();
    std::fs::write(workspace.join("medha.lock"), format!(
        "[[lsp.servers]]\nid = 'fixture'\ntrust = 'workspace'\ncommand = ['python3', '{}']\nlanguages = ['g7']\nroot_markers = ['.git']\n", script.display()
    )).unwrap();
    std::fs::write(workspace.join("src/code.g7"), "fixture").unwrap();
    let trust = ProjectTrust::for_folder(&workspace).unwrap().unwrap();
    trust.accept(&trust.review(&workspace).unwrap()).unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "child_lsp_owner", "--nocapture"])
        .env("MEDHA_HOME", &state)
        .env("MEDHA_TEST_WORKSPACE", &workspace)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let cleanup = ServerCleanup(workspace.join("server.pid"));
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let line = output
                .next_line()
                .await
                .unwrap()
                .expect("owner exited before starting");
            if line == "CONTROL_READY" {
                break;
            }
        }
    })
    .await
    .unwrap();
    let pid = std::fs::read_to_string(&cleanup.0)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert!(!state.join("lsp_controls").exists());
    let started = Instant::now();
    revoke_live(&workspace.join("src")).await.unwrap();
    eprintln!("local revocation: {} us", started.elapsed().as_micros());
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "an idle foreign process does not participate in revocation"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "the open chat stays alive"
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"continue\n")
        .await
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "the foreign server stopped before its next query returned"
    );
    assert!(!state.join("lsp_controls").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "fixture invoked in a separate process by the revocation test"]
async fn child_lsp_owner() {
    let workspace = PathBuf::from(std::env::var_os("MEDHA_TEST_WORKSPACE").unwrap());
    let base = lsp::Config::default();
    let source = workspace.clone();
    let manager = Arc::new(
        lsp::LspManager::new(workspace.clone(), base.clone()).with_project_config(Arc::new(
            move |folder| runtime::project_session::lsp_config(&source, folder, &base),
        )),
    );
    register_lsp(&manager);
    let code = workspace.join("src/code.g7");
    assert!(matches!(
        manager
            .hover(
                &code,
                lsp::Position {
                    line: 0,
                    character: 0
                }
            )
            .await,
        lsp::QueryReport::Ready { .. }
    ));
    println!("CONTROL_READY");
    std::io::stdout().flush().unwrap();
    let mut signal = String::new();
    std::io::stdin().read_line(&mut signal).unwrap();
    assert!(matches!(
        manager
            .hover(
                &code,
                lsp::Position {
                    line: 0,
                    character: 0
                }
            )
            .await,
        lsp::QueryReport::Unavailable { .. }
    ));
    assert!(manager.status().await.is_empty());
}
