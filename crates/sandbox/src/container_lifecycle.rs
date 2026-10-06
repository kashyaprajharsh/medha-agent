//! Daemon-owned command lifetime. A killed Docker client is not containment.
use super::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const CREATE_TIMEOUT: Duration = Duration::from_secs(15);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

struct OwnedContainer {
    runtime: String,
    name: String,
    registered: bool,
    armed: bool,
}

impl OwnedContainer {
    async fn cleanup(&mut self) -> Result<(), String> {
        if !self.armed {
            return Ok(());
        }
        let mut last = String::new();
        for attempt in 0..3 {
            let mut command = tokio::process::Command::new(&self.runtime);
            // Remove anonymous volumes too. The exact generated name is the
            // only selector; no process, label, prefix or workspace matching.
            command.args(["rm", "-f", "-v", &self.name]);
            match run_command_bounded(command, CLEANUP_TIMEOUT, 4096, None).await {
                Ok(result)
                    if result.status == Some(0)
                        || (self.registered && reports_absent(&result.output)) =>
                {
                    self.armed = false;
                    return Ok(());
                }
                Ok(result) => last = result.output,
                Err(error) => last = error.to_string(),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await;
            }
        }
        Err(format!(
            "could not confirm removal of {}: {last}",
            self.name
        ))
    }
}

fn reports_absent(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("no such container") || text.contains("no container with name or id")
}

impl Drop for OwnedContainer {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Runtime teardown/panic backstop. Normal cleanup is asynchronous.
        // Killing the caller's future does not drop this independently owned
        // lifecycle; it requests cancellation and waits for removal instead.
        for _ in 0..3 {
            let mut command = std::process::Command::new(&self.runtime);
            command
                .args(["rm", "-f", "-v", &self.name])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let Ok(mut child) = command.spawn() else {
                continue;
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) if status.success() => {
                        self.armed = false;
                        return;
                    }
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        eprintln!("Medha could not confirm container cleanup: {}", self.name);
    }
}

pub(super) fn spawn(
    backend: ContainerBackend,
    request: ExecRequest,
    working_dir: Option<&Path>,
    limits: CaptureLimits,
) -> Result<BgProc, ExecError> {
    let root = request
        .cwd
        .canonicalize()
        .map_err(|e| ExecError::Spawn(e.to_string()))?;
    let dir = working_dir
        .unwrap_or(&request.cwd)
        .canonicalize()
        .map_err(|e| ExecError::Spawn(e.to_string()))?;
    let relative = dir.strip_prefix(&root).map_err(|_| {
        ExecError::Unavailable(
            "container working directory must be inside the mounted workspace".into(),
        )
    })?;
    let container_dir = format!(
        "/workspace/{}",
        relative.to_string_lossy().replace('\\', "/")
    );
    let name = format!("medha-command-{}", ulid::Ulid::new());
    let lease = OwnedContainer {
        runtime: backend.runtime.clone(),
        name,
        registered: false,
        armed: true,
    };
    let capture: SharedCapture = Arc::new(Mutex::new(CapturePair::new(
        limits.stdout,
        limits.stderr,
        limits.aggregate,
        backend.denies_network(&request),
    )));
    let code = Arc::new(Mutex::new(None));
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);
    let cancel = CancellationToken::new();
    let owned_cancel = cancel.clone();
    let held_capture = capture.clone();
    let held_code = code.clone();
    // Cleanup must not depend on the chat's executor: a synchronous credential
    // wait in that chat cannot prevent cancellation reaching the daemon.
    std::thread::Builder::new()
        .name("medha-container".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    if let Ok(mut target) = held_capture.lock() {
                        target.push(
                            CapturedStream::Stderr,
                            format!("container supervisor unavailable: {error}").as_bytes(),
                        );
                    }
                    if let Ok(mut target) = held_code.lock() {
                        *target = Some(-1);
                    }
                    let _ = done_tx.send(true);
                    return;
                }
            };
            runtime.block_on(async move {
                let mut lease = lease;
                let operation: Result<Option<i32>, String> = async {
                    if cancel.is_cancelled() {
                        lease.armed = false;
                        return Ok(None);
                    }
                    let create =
                        backend.build_owned_create_command(&request, &lease.name, &container_dir);
                    // create is inert. Settle registration before observing cancellation
                    // so no late start can race a successful removal.
                    let created = run_command_bounded(create, CREATE_TIMEOUT, 8192, None)
                        .await
                        .map_err(|error| error.to_string())?;
                    if created.status != Some(0) || created.timed_out {
                        return Err(format!("container was not started: {}", created.output));
                    }
                    lease.registered = true;
                    if cancel.is_cancelled() {
                        return Ok(None);
                    }
                    let mut start = tokio::process::Command::new(&backend.runtime);
                    start.args(["start", "-a", &lease.name]);
                    let process = spawn_background_captured(
                        start,
                        limits.stdout,
                        limits.stderr,
                        limits.aggregate,
                        false,
                        None,
                        Some(held_capture.clone()),
                    )
                    .map_err(|error| error.to_string())?;
                    tokio::select! {
                        _ = cancel.cancelled() => {
                            process.kill();
                            process.wait().await;
                            Ok(None)
                        }
                        _ = process.wait() => Ok(process.exit_code()),
                    }
                }
                .await;
                let cleanup = lease.cleanup().await;
                let result = match (operation, cleanup) {
                    (Ok(status), Ok(())) => status,
                    (operation, cleanup) => {
                        let error = format!(
                            "[container execution failed: {}; cleanup: {}]\n",
                            operation.err().unwrap_or_else(|| "command finished".into()),
                            cleanup.err().unwrap_or_else(|| "complete".into())
                        );
                        if let Ok(mut target) = held_capture.lock() {
                            target.push(CapturedStream::Stderr, error.as_bytes());
                        }
                        Some(-1)
                    }
                };
                if let Ok(mut target) = held_code.lock() {
                    *target = result;
                }
                // Completion includes confirmed cleanup; failure is explicitly nonzero.
                let _ = done_tx.send(true);
            });
        })
        .map_err(|error| ExecError::Spawn(format!("container supervisor unavailable: {error}")))?;
    Ok(BgProc {
        pid: None,
        capture,
        done_rx,
        code,
        net_flag: None,
        owned_cancel: Some(owned_cancel),
    })
}
