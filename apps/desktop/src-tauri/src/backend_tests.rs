use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

pub(crate) fn address() -> String {
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

/// A backend that admits a client and answers only what starts a chat: its
/// start, after `late`, and its being attached to, followed by `events` frames
/// the chat is made to have said. It keeps what it was asked.
pub(crate) fn scripted_backend(
    address: &str,
    token: &'static str,
    late: Duration,
    events: usize,
) -> Arc<Mutex<Vec<Value>>> {
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
                if frame["method"] == "session.attach" && frame.get("id").is_some() {
                    let attached = json!({ "id": frame["id"], "result": { "gap": false } });
                    wire::write_frame(&mut writing, &attached).await;
                    for seq in 1..=events {
                        let said = json!({ "method": "note", "params": { "n": seq } });
                        let event = json!({ "method": "session.event",
                            "params": { "session": "late", "seq": seq, "frame": said } });
                        wire::write_frame(&mut writing, &event).await;
                    }
                }
            }
        });
    }));
    asked
}

fn joined(address: &str) -> Arc<Connection> {
    let joining = join(address.to_owned(), "token".into());
    let Ok(connection) = reading().block_on(joining) else {
        panic!("the backend did not admit the client");
    };
    connection
}

fn forget(address: &str) {
    if let Some(folder) = Path::new(address).parent().filter(|_| cfg!(unix)) {
        let _ = std::fs::remove_dir_all(folder);
    }
}

/// Waits until the backend has taken everything it was sent but `left` bytes.
fn taken_but(connection: &Connection, left: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while connection.unsent.load(Ordering::Relaxed) != left {
        assert!(Instant::now() < deadline, "the backend stopped reading");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_wait_that_ended_and_a_chat_that_ended_hold_no_place_among_the_unanswered() {
    let address = address();
    scripted_backend(&address, "token", Duration::ZERO, 0);
    let connection = joined(&address);
    let open = || {
        connection
            .open_chat(Path::new("."), None, false, None, Arc::new(|_| {}))
            .expect("a request with nobody waiting for it still held a place")
    };
    let settings = |id: usize| json!({ "id": id, "method": "session.settings" });

    // Nothing here answers these, and every wait for one runs out.
    for _ in 0..UNANSWERED {
        let unanswered = connection.ask(json!({ "method": "session.list" }), Duration::ZERO);
        assert_eq!(unanswered, Err(QUIET.to_string()));
    }
    let chat = open();

    for id in 0..UNANSWERED {
        connection.tell(&chat, settings(id)).unwrap();
    }
    let full = connection.tell(&chat, settings(0));
    assert_eq!(full, Err(NOT_TAKING.to_string()));
    let ended = json!({ "method": "session.ended", "params": {} });
    connection.heard(json!({ "method": "session.event",
        "params": { "session": "late", "seq": 1, "frame": ended } }));
    let again = open();

    // A request whose answer never comes gives its place up once it has waited as long as any may.
    for id in 0..UNANSWERED {
        connection.tell(&again, settings(id)).unwrap();
    }
    assert!(!connection.routes().is_full(UNANSWERED, Duration::ZERO));
    forget(&address);
}

#[test]
fn what_stops_a_chat_has_room_of_its_own_and_only_so_much() {
    let address = address();
    scripted_backend(&address, "token", Duration::ZERO, 0);
    let connection = joined(&address);
    let chat = connection
        .open_chat(Path::new("."), None, false, None, Arc::new(|_| {}))
        .unwrap();
    let refused = Err(NOT_TAKING.to_string());
    let cancel = || json!({ "method": "cancel" });

    // As it is when the backend has stopped taking what it is sent.
    taken_but(&connection, 0);
    connection.unsent.store(UNSENT_BYTES, Ordering::Relaxed);
    let settings = json!({ "method": "session.settings" });
    assert_eq!(connection.tell(&chat, settings), refused);
    connection.tell(&chat, cancel()).unwrap();
    taken_but(&connection, UNSENT_BYTES);
    // One made large is no longer something that only stops a chat.
    let padded = json!({ "method": "cancel", "params": { "why": "x".repeat(STOP_BYTES) } });
    assert_eq!(connection.tell(&chat, padded), refused);
    connection
        .unsent
        .store(UNSENT_BYTES + STOPS_UNSENT, Ordering::Relaxed);
    assert_eq!(connection.tell(&chat, cancel()), refused);
    forget(&address);
}

#[test]
fn a_wait_on_a_quiet_backend_gives_up_and_a_chat_that_starts_late_is_told_to_stop() {
    let address = address();
    let asked = scripted_backend(&address, "token", Duration::from_millis(600), 0);
    let connection = joined(&address);

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
    forget(&address);
}
