//! The burst test runs in a binary of its own: 128 commands at once slow every
//! process on the machine, and beside tests that hold a clock they fail those.
#![cfg(unix)]

use sandbox::run_shell_bounded;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_shell_success_reap_survives_high_contention() {
    let dir = std::env::temp_dir().join(format!("medha-successpg-stress-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    // A helper acts only once every command has returned, so a slow kernel
    // cannot fail this: any helper still alive then leaves its marker. It
    // gives up after a minute so a killed test run leaves nothing looping.
    let gate = dir.join("every-command-returned");
    let mut runs = Vec::new();
    for n in 0..128 {
        let cwd = dir.clone();
        let marker = dir.join(format!("survived-{n}.txt"));
        let gate = gate.clone();
        runs.push(tokio::spawn(async move {
            let gate = gate.display();
            let script = format!(
                "(n=0; while [ ! -e {gate} ] && [ $n -lt 300 ]; do sleep 0.2; n=$((n+1)); \
                 done; [ -e {gate} ] && touch {}) >/dev/null 2>&1 &",
                marker.display()
            );
            run_shell_bounded(
                &script,
                &cwd,
                std::time::Duration::from_secs(30),
                1024,
                None,
            )
            .await
            .unwrap()
        }));
    }
    for run in runs {
        let outcome = run.await.unwrap();
        // The invariant under test is group reaping, not scheduler
        // throughput: under full-suite load a leader can overrun its bound
        // and be killed, and that killed group must be reaped exactly like
        // a completed one — the survivor count below is the real check.
        // Anything besides clean completion or the bounded kill is a
        // genuine failure.
        assert!(
            outcome.passed() || outcome.timed_out,
            "run neither completed nor timed out: status={:?} cancelled={}",
            outcome.status,
            outcome.cancelled
        );
    }
    std::fs::write(&gate, b"").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let survivors = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("survived-"))
        .count();
    assert_eq!(survivors, 0, "redirected helpers escaped under load");
    std::fs::remove_dir_all(&dir).ok();
}
