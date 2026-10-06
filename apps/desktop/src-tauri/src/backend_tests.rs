use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

fn address() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "mq-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    if cfg!(windows) {
        format!(r"\\.\pipe\{name}")
    } else {
        // Listening makes the folder private, so the socket gets a folder of its own.
        let folder = std::env::temp_dir().join(name);
        folder.join("s").display().to_string()
    }
}

/// A backend that admits a client and then answers nothing but a chat's start, and that late.
fn slow_backend(address: &str, token: &'static str, late: Duration) -> Arc<Mutex<Vec<Value>>> {
    let asked: Arc<Mutex<Vec<Value>>> = Arc::default();
    let listener = reading().block_on(async { wire::bind(address) }).unwrap();
    let heard = Arc::clone(&asked);
    reading().spawn(listener.serve(move |stream| {
        let heard = Arc::clone(&heard);
        tokio::spawn(async move {
            let (reading, mut writing) = tokio::io::split(stream);
            let mut reader = BufReader::new(reading);
            let Some(id) = wire::admit(&mut reader, &mut writing, token, ROLES).await else {
                return;
            };
            let welcome = json!({ "id": id, "result": { "protocol": PROTOCOL } });
            wire::write_frame(&mut writing, &welcome).await;
            while let Some(frame) = wire::read_frame(&mut reader).await {
                heard.lock().unwrap().push(frame.clone());
                if frame["method"] == "session.create" {
                    tokio::time::sleep(late).await;
                    let started = json!({ "id": frame["id"], "result": { "session": "late" } });
                    wire::write_frame(&mut writing, &started).await;
                }
            }
        });
    }));
    asked
}

#[test]
fn a_wait_on_a_quiet_backend_gives_up_and_a_chat_that_starts_late_is_told_to_stop() {
    let address = address();
    let asked = slow_backend(&address, "token", Duration::from_millis(600));
    let joining = join(address.clone(), "token".into());
    let Ok(connection) = reading().block_on(joining) else {
        panic!("the backend did not admit the client");
    };

    let unanswered = connection.ask(
        json!({ "method": "session.list" }),
        Duration::from_millis(200),
    );
    assert_eq!(unanswered, Err(QUIET.to_string()));
    let create = json!({ "method": "session.create", "params": {} });
    let gave_up = connection.ask(create, Duration::from_millis(200));
    assert_eq!(gave_up, Err(QUIET.to_string()));

    let deadline = Instant::now() + Duration::from_secs(10);
    let stopped = |asked: &[Value]| {
        asked
            .iter()
            .any(|frame| frame["method"] == "session.close" && frame["session"] == "late")
    };
    while !stopped(&asked.lock().unwrap()) {
        assert!(
            Instant::now() < deadline,
            "a chat nobody waits for was left running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(folder) = Path::new(&address).parent().filter(|_| cfg!(unix)) {
        let _ = std::fs::remove_dir_all(folder);
    }
}
