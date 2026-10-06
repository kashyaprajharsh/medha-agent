//! Opt-in real Docker/Podman proof, including an escaped double-fork helper.
use sandbox::exec::{CaptureLimits, ContainerBackend, ExecBackend, ExecRequest, NetPolicy};
use std::path::PathBuf;
use std::time::Duration;

const HELPER: &str = r#"
#include <unistd.h>
#include <stdlib.h>
#include <fcntl.h>
#include <sys/stat.h>
int main(int argc, char **argv) {
  pid_t child = fork(); if (child < 0) return 2;
  if (!child) {
    if (setsid() < 0) _exit(3);
    child = fork(); if (child < 0) _exit(4); if (child) _exit(0);
    int fd = open("heartbeat", O_WRONLY | O_CREAT | O_APPEND, 0600);
    int ready = open("ready", O_WRONLY | O_CREAT, 0600); close(ready);
    close(0); close(1); close(2);
    for (;;) { write(fd, ".", 1); usleep(10000); }
  }
  struct stat st;
  for (int n=0; n<1000 && stat("ready", &st); ++n) usleep(10000);
  if (argc > 1 && argv[1][0] == 'n') return 0;
  for (;;) pause();
}
"#;

struct Fixture {
    root: PathBuf,
    runtime: String,
    control: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only this test's exact control container is eligible for removal.
        let _ = std::process::Command::new(&self.runtime)
            .args(["rm", "-f", "-v", &self.control])
            .output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a running Docker/Podman daemon and MEDHA_TEST_CONTAINER_IMAGE with cc"]
async fn double_fork_helpers_stop_on_timeout_cancel_drop_and_normal_exit() {
    let image = std::env::var("MEDHA_TEST_CONTAINER_IMAGE")
        .expect("set MEDHA_TEST_CONTAINER_IMAGE to a cached image with sh and cc");
    let runtime = std::env::var("MEDHA_TEST_CONTAINER_RUNTIME").unwrap_or_else(|_| "docker".into());
    let root = std::env::temp_dir().join(format!("medha-container-cleanup-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let control = format!("medha-review-control-{}", ulid::Ulid::new());
    let fixture = Fixture {
        root,
        runtime: runtime.clone(),
        control: control.clone(),
    };
    let started = std::process::Command::new(&runtime)
        .args([
            "run",
            "-d",
            "--pull",
            "never",
            "--name",
            &control,
            "--network",
            "none",
            "--entrypoint",
            "sh",
            &image,
            "-c",
            "sleep 120",
        ])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let backend = ContainerBackend::new(
        runtime.clone(),
        image,
        NetPolicy::Deny,
        Some("256m".into()),
        Some(32),
    );
    for mode in ["timeout", "cancel", "drop", "normal"] {
        let dir = fixture.root.join(mode);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("escaped.c"), HELPER).unwrap();
        let command = format!("cc escaped.c -o escaped && exec ./escaped {mode}");
        if mode == "timeout" {
            let result = sandbox::run_shell_bounded_with(
                &backend,
                &command,
                &dir,
                Duration::from_secs(3),
                4096,
                None,
            )
            .await
            .unwrap();
            assert!(result.timed_out, "{result:?}");
        } else {
            let request = ExecRequest {
                program: "sh".into(),
                args: vec!["-c".into(), command],
                cwd: dir.clone(),
                env: vec![],
                clear_env: true,
                read_roots: vec![],
                write_roots: vec![],
            };
            let process = backend
                .spawn_owned(&request, None, CaptureLimits::standard())
                .unwrap();
            if mode != "normal" {
                tokio::time::timeout(Duration::from_secs(15), async {
                    while !dir.join("ready").exists() {
                        assert!(
                            process.is_running(),
                            "container ended before helper started: {:?}",
                            process.snapshot()
                        );
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("helper started");
            }
            if mode == "drop" {
                let done = process.done_receiver();
                drop(process);
                assert!(sandbox::exec::wait_done(done, Duration::from_secs(15)).await);
            } else {
                if mode == "cancel" {
                    process.kill();
                }
                assert!(
                    process.wait_until(Duration::from_secs(15)).await,
                    "container cleanup completed"
                );
                if mode == "normal" {
                    assert_eq!(process.exit_code(), Some(0), "{:?}", process.snapshot());
                }
            }
        }
        assert!(
            dir.join("ready").exists(),
            "escaped helper actually ran in {mode}"
        );
        let size = std::fs::metadata(dir.join("heartbeat")).unwrap().len();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            std::fs::metadata(dir.join("heartbeat")).unwrap().len(),
            size,
            "escaped helper survived {mode}"
        );
        let control_state = std::process::Command::new(&runtime)
            .args(["inspect", "--format", "{{.State.Running}}", &control])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&control_state.stdout).trim(),
            "true",
            "unrelated control container must survive {mode}"
        );
    }
}
