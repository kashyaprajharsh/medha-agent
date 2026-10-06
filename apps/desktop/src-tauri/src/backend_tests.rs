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
    let this = json!({ "build": wire::BUILD_ID, "capabilities": wire::CAPABILITIES });
    scripted(address, token, late, events, this)
}

/// One that says of itself what `identity` says, and has work in hand: asked
/// to give way to another build, it refuses.
fn scripted(
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

fn joined(address: &str) -> Arc<Connection> {
    let joining = join(address.to_owned(), "token".into(), false);
    let Ok(connection) = reading().block_on(joining) else {
        panic!("the backend did not admit the client");
    };
    connection
}

/// The terminal and the desktop are installed apart and are seldom one build.
/// Each must be able to use the backend the other started.
#[test]
fn a_backend_of_another_build_is_joined_when_it_can_do_all_this_one_needs() {
    let another =
        |capabilities: &[&str]| json!({ "build": "another", "capabilities": capabilities });
    let newer: Vec<&str> = wire::CAPABILITIES
        .iter()
        .copied()
        .chain(["later"])
        .collect();
    let older = &wire::CAPABILITIES[..1];
    assert_eq!(fit(&another(wire::CAPABILITIES), false), Fit::Join);
    assert_eq!(fit(&another(&newer), false), Fit::Join);
    assert_eq!(fit(&another(older), false), Fit::GiveWay { needed: true });
    assert_eq!(fit(&another(&[]), false), Fit::Legacy);
    // A build under development also asks one left over from before it to give way.
    let stale = fit(&another(wire::CAPABILITIES), true);
    assert_eq!(stale, Fit::GiveWay { needed: false });
    let same = json!({ "build": wire::BUILD_ID, "capabilities": wire::CAPABILITIES });
    assert_eq!(fit(&same, true), Fit::Join);

    // One with work in hand does not give way. It is joined if it can be used, and refused only if it cannot.
    let (usable, unusable) = (address(), address());
    scripted(
        &usable,
        "token",
        Duration::ZERO,
        0,
        another(wire::CAPABILITIES),
    );
    scripted(&unusable, "token", Duration::ZERO, 0, another(older));
    let connection = joined(&usable);
    connection
        .open_chat(Path::new("."), None, false, None, Arc::new(|_| {}))
        .expect("a backend that was joined did not answer");
    let refused = reading().block_on(join(unusable.clone(), "token".into(), false));
    assert!(matches!(refused, Err(Refused::Busy(why)) if why == "in use"));
    forget(&usable);
    forget(&unusable);
}

#[cfg(unix)]
#[test]
fn an_exited_starter_is_retried_after_the_singleton_is_released() {
    use std::os::unix::fs::PermissionsExt;
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/debug/medha");
    if !binary.is_file() {
        assert!(
            std::env::var_os("CI").is_none(),
            "build the backend before desktop tests"
        );
        return;
    }
    let address = address();
    let root = Path::new(&address).parent().unwrap();
    std::fs::create_dir_all(root).unwrap();
    let script = root.join("starter");
    std::fs::write(&script, "#!/bin/sh\nif mkdir \"$MEDHA_HOME/first-attempt\" 2>/dev/null; then exit 1; fi\nexec \"$MEDHA_TEST_BACKEND\" \"$@\"\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let backend = Backend::owned(
        script,
        vec![
            ("MEDHA_HOME".into(), root.display().to_string()),
            ("MEDHA_TEST_BACKEND".into(), binary.display().to_string()),
            ("MEDHA_CRED_STORE".into(), "file".into()),
        ],
    );
    let outcome = backend
        .connection()
        .and_then(|connection| connection.ask(json!({"method": "hello"}), SOON));
    backend.stop();
    std::fs::remove_dir_all(root).unwrap();
    assert!(outcome.is_ok(), "{outcome:?}");
}

pub(crate) fn exhaust_ordinary_replies(connection: &Arc<Connection>) {
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

pub(crate) fn disconnect(connection: &Arc<Connection>) {
    connection.lost();
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
fn leaving_an_old_incarnation_preserves_the_current_viewer_and_requests() {
    let (lines, _queued) = tokio::sync::mpsc::unbounded_channel();
    let (controls, _urgent) = tokio::sync::mpsc::unbounded_channel();
    let connection = Connection {
        lines,
        controls,
        unsent: Arc::default(),
        routes: Mutex::default(),
        stop: tokio::sync::watch::channel(false).0,
    };
    let current = Chat {
        session: "same".into(),
        opened: 2,
    };
    connection
        .routes()
        .chats
        .insert(current.session.clone(), (2, Arc::new(|_| {})));
    connection
        .tell(&current, json!({"id": 1, "method": "session.settings"}))
        .unwrap();
    connection.leave(&Chat {
        session: "same".into(),
        opened: 1,
    });
    assert!(connection.routes().hearing("same", Some(2)).is_some());
    assert_eq!(connection.routes().replies.len(), 1);
    connection
        .tell(&current, json!({"method": "cancel"}))
        .unwrap();
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
