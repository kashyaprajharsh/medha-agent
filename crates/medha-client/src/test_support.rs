use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

pub fn address() -> String {
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
pub fn scripted_backend(
    address: &str,
    token: &'static str,
    late: Duration,
    events: usize,
) -> Arc<Mutex<Vec<Value>>> {
    let this = json!({ "build": wire::BUILD_ID, "capabilities": wire::CAPABILITIES });
    scripted(address, token, late, events, this)
}

/// One that says of itself what `identity` says, and has work in hand: asked
/// to give way to another build, it refuses.
pub fn scripted(
    address: &str,
    token: &'static str,
    late: Duration,
    events: usize,
    mut identity: Value,
) -> Arc<Mutex<Vec<Value>>> {
    identity["protocol"] = json!(PROTOCOL);
    let asked: Arc<Mutex<Vec<Value>>> = Arc::default();
    let listener = reading().block_on(async { wire::bind(address) }).unwrap();
    let heard = Arc::clone(&asked);
    reading().spawn(listener.serve(move |stream| {
        let (heard, identity) = (Arc::clone(&heard), identity.clone());
        tokio::spawn(async move {
            let (reading, mut writing) = tokio::io::split(stream);
            let mut reader = BufReader::new(reading);
            let Some(id) = wire::admit(&mut reader, &mut writing, token, ROLES).await else {
                return;
            };
            let welcome = json!({ "id": id, "result": identity });
            wire::write_frame(&mut writing, &welcome).await;
            while let Some(frame) = wire::read_frame(&mut reader).await {
                heard.lock().unwrap().push(frame.clone());
                if frame["method"] == "backend.prepare_upgrade" {
                    let busy = json!({ "id": frame["id"], "error": { "message": "in use" } });
                    wire::write_frame(&mut writing, &busy).await;
                }
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

pub fn exhaust_ordinary_replies(connection: &Arc<Connection>) {
    let (session, opened) = {
        let routes = connection.routes();
        let (session, (opened, _)) = routes.chats.iter().next().unwrap();
        (session.clone(), *opened)
    };
    for id in 0..UNANSWERED {
        connection
            .tell(
                &Chat {
                    session: session.clone(),
                    opened,
                },
                json!({"id": id, "method": "session.settings"}),
            )
            .unwrap();
    }
}

pub fn disconnect(connection: &Arc<Connection>) {
    connection.lost();
}

pub fn forget(address: &str) {
    if let Some(folder) = Path::new(address).parent().filter(|_| cfg!(unix)) {
        let _ = std::fs::remove_dir_all(folder);
    }
}

/// Waits until the backend has taken everything it was sent but `left` bytes.
pub fn taken_but(connection: &Connection, left: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while connection.unsent.load(Ordering::Relaxed) != left {
        assert!(Instant::now() < deadline, "the backend stopped reading");
        std::thread::sleep(Duration::from_millis(5));
    }
}
