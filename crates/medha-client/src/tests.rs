use super::test_support::*;
use super::*;
use std::future::{Future, poll_fn};
use std::task::Poll;

fn joined(address: &str) -> Arc<Connection> {
    let joining = join(address.to_owned(), "token".into(), false);
    let Ok(connection) = reading().block_on(joining) else {
        panic!("the backend did not admit the client");
    };
    connection
}

#[test]
fn reply_saturation_fixture_counts_a_pending_bootstrap_request() {
    let (lines, _queued) = tokio::sync::mpsc::unbounded_channel();
    let (controls, _urgent) = tokio::sync::mpsc::unbounded_channel();
    let connection = Arc::new(Connection {
        lines,
        controls,
        unsent: Arc::default(),
        routes: Mutex::default(),
        stop: tokio::sync::watch::channel(false).0,
    });
    let chat = connection
        .listen("fixture".into(), Arc::new(|_| {}))
        .unwrap();
    connection
        .tell(
            &chat,
            json!({"id": "bootstrap", "method": "session.presentation"}),
        )
        .unwrap();
    exhaust_ordinary_replies(&connection);
    assert_eq!(connection.routes().replies.len(), UNANSWERED);
    assert!(!connection.has_room(128, "session.settings"));
    assert!(connection.has_room(128, "cancel"));
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
    // After an update, the backend an earlier release left running is asked to
    // give way. A later one is joined: the two never stop each other's in turn.
    let released = |version: &str| {
        json!({ "backend": version, "build": "another",
        "capabilities": wire::CAPABILITIES })
    };
    let earlier = fit(&released("0.1.9"), false);
    assert_eq!(earlier, Fit::GiveWay { needed: false });
    assert_eq!(fit(&released(env!("CARGO_PKG_VERSION")), false), Fit::Join);
    assert_eq!(fit(&released("999.0.0"), false), Fit::Join);

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

#[test]
fn each_surface_requires_its_own_features_without_requiring_an_identical_build() {
    let desktop = json!({"build": "different", "capabilities": wire::CLIENT_CAPABILITIES});
    assert_eq!(fit_for(&desktop, false, &[]), Fit::Join);
    assert_eq!(
        fit_for(&desktop, false, &["terminal-controls", "replay-stream"]),
        Fit::GiveWay { needed: true }
    );
    let terminal = json!({"build": "different", "capabilities": wire::CAPABILITIES});
    assert_eq!(
        fit_for(&terminal, false, &["terminal-controls", "replay-stream"]),
        Fit::Join
    );
}

#[tokio::test]
async fn an_old_chat_handle_cannot_send_an_async_request_to_its_replacement() {
    let (lines, mut queued) = tokio::sync::mpsc::unbounded_channel();
    let (controls, _urgent) = tokio::sync::mpsc::unbounded_channel();
    let connection = Connection {
        lines,
        controls,
        unsent: Arc::default(),
        routes: Mutex::default(),
        stop: tokio::sync::watch::channel(false).0,
    };
    let first = connection.listen("same".into(), Arc::new(|_| {})).unwrap();
    connection.routes().forget("same", first.opened);
    let _second = connection.listen("same".into(), Arc::new(|_| {})).unwrap();
    let refused = connection.call_chat(&first, &protocol::Cancel {}).await;
    assert_eq!(refused.unwrap_err(), STOPPED);
    assert!(queued.try_recv().is_err());
    assert!(connection.routes().replies.is_empty());
    assert_eq!(connection.unsent.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn cancelling_after_create_while_attach_waits_detaches_and_abandons_in_order() {
    let (lines, mut queued) = tokio::sync::mpsc::unbounded_channel();
    let (controls, mut urgent) = tokio::sync::mpsc::unbounded_channel();
    let connection = Connection {
        lines,
        controls,
        unsent: Arc::default(),
        routes: Mutex::default(),
        stop: tokio::sync::watch::channel(false).0,
    };
    let params = protocol::CreateSession {
        folder: PathBuf::from("."),
        ends_with_client: true,
        resume: None,
        model: None,
        mode: None,
        reasoning: None,
        settings: None,
        startup: None,
    };
    let mut opening = Box::pin(connection.create_chat(&params, Arc::new(|_| {})));
    poll_fn(|cx| {
        assert!(opening.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let create: Value = serde_json::from_str(&queued.try_recv().unwrap()).unwrap();
    connection.heard(json!({"id": create["id"], "result": {"session": "new", "stream": "first"}}));
    poll_fn(|cx| {
        assert!(opening.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let attach: Value = serde_json::from_str(&urgent.try_recv().unwrap()).unwrap();
    assert_eq!(attach["method"], "session.attach");
    assert_eq!(connection.routes().chats.len(), 1);
    drop(opening);
    assert!(connection.routes().replies.is_empty());
    assert!(connection.routes().chats.is_empty());
    let detach: Value = serde_json::from_str(&urgent.try_recv().unwrap()).unwrap();
    let abandon: Value = serde_json::from_str(&urgent.try_recv().unwrap()).unwrap();
    assert_eq!(detach["method"], "session.detach");
    assert_eq!(abandon["method"], "session.abandon");
    assert!(urgent.try_recv().is_err());
}

#[cfg(unix)]
#[test]
fn an_exited_starter_is_retried_after_the_singleton_is_released() {
    use std::os::unix::fs::PermissionsExt;
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/medha");
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
            .any(|frame| frame["method"] == "session.abandon" && frame["session"] == "late")
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

async fn wait_for_close(asked: &Mutex<Vec<Value>>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let methods: Vec<String> = asked
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| frame["session"] == "late")
            .filter_map(|frame| frame["method"].as_str().map(str::to_owned))
            .collect();
        if methods.iter().any(|method| method == "session.abandon") {
            assert_eq!(methods, ["session.abandon"]);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "an abandoned chat was left running"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn async_peer(late: Duration) -> (String, Arc<Mutex<Vec<Value>>>, Arc<Connection>) {
    tokio::task::spawn_blocking(move || {
        let address = address();
        let asked = scripted_backend(&address, "token", late, 0);
        let connection = joined(&address);
        (address, asked, connection)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn cancelling_an_async_start_frees_its_slot_and_retires_the_late_chat() {
    let (address, asked, connection) = async_peer(Duration::from_millis(100)).await;
    let mut request = Box::pin(connection.request(json!({"method": "session.create"})));
    poll_fn(|cx| {
        assert!(request.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(connection.routes().replies.len(), 1);
    drop(request);
    assert!(connection.routes().replies.is_empty());
    wait_for_close(&asked).await;
    connection.lost();
    forget(&address);
}

#[tokio::test]
async fn cancelling_after_a_start_reply_arrives_still_retires_the_chat() {
    let (address, asked, connection) = async_peer(Duration::from_millis(50)).await;
    let mut request = Box::pin(connection.request(json!({"method": "session.create"})));
    poll_fn(|cx| {
        assert!(request.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    // Leave the request unpolled while the reader delivers the reply to its
    // oneshot. Cancellation must handle this ownership-transfer boundary too.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !connection.routes().replies.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    drop(request);
    wait_for_close(&asked).await;
    connection.lost();
    forget(&address);
}

#[tokio::test]
async fn an_async_deadline_frees_the_slot_and_retires_a_late_start() {
    let (address, asked, connection) = async_peer(Duration::from_millis(100)).await;
    let outcome = connection
        .ask_async(
            json!({"method": "session.create"}),
            Duration::from_millis(10),
        )
        .await;
    assert_eq!(outcome, Err(QUIET.to_owned()));
    assert!(connection.routes().replies.is_empty());
    wait_for_close(&asked).await;
    connection.lost();
    forget(&address);
}

#[tokio::test]
async fn cancelled_async_reads_do_not_exhaust_the_request_limit() {
    let (address, _, connection) = async_peer(Duration::ZERO).await;
    for _ in 0..UNANSWERED + 1 {
        let mut request = Box::pin(connection.request(json!({"method": "session.list"})));
        poll_fn(|cx| {
            assert!(request.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(request);
    }
    assert!(connection.routes().replies.is_empty());
    assert!(connection.routes().late.is_empty());
    let outcome = connection
        .request(json!({"method": "session.create"}))
        .await;
    assert_eq!(outcome.unwrap()["session"], "late");
    connection.lost();
    forget(&address);
}

#[tokio::test]
async fn a_closed_writer_releases_the_bytes_and_reply_slot_before_returning() {
    let (lines, queued) = tokio::sync::mpsc::unbounded_channel();
    let (controls, urgent) = tokio::sync::mpsc::unbounded_channel();
    drop((queued, urgent));
    let connection = Connection {
        lines,
        controls,
        unsent: Arc::default(),
        routes: Mutex::default(),
        stop: tokio::sync::watch::channel(false).0,
    };
    for method in ["session.create", "session.close"] {
        let outcome = connection.request(json!({"method": method})).await;
        assert_eq!(outcome, Err(STOPPED.to_owned()));
        assert_eq!(connection.unsent.load(Ordering::Relaxed), 0);
        assert!(connection.routes().replies.is_empty());
    }
}

#[tokio::test]
async fn typed_async_commands_validate_scope_and_decode_the_backend_answer() {
    let (address, _, connection) = async_peer(Duration::ZERO).await;
    let wrong = connection
        .call_async(protocol::Scope::Service, &protocol::Cancel {})
        .await;
    assert_eq!(wrong.unwrap_err(), "the command has the wrong scope");
    assert_eq!(connection.unsent.load(Ordering::Relaxed), 0);
    assert!(connection.routes().replies.is_empty());

    let made = connection
        .call_async(
            protocol::Scope::Service,
            &protocol::CreateSession {
                folder: PathBuf::from("."),
                ends_with_client: true,
                resume: None,
                model: None,
                mode: None,
                reasoning: None,
                settings: None,
                startup: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(made.session, "late");
    let invalid = connection
        .decoded::<protocol::GetSettings>(json!({"secret": "never echo this"}))
        .unwrap_err();
    assert_eq!(
        invalid,
        "Medha returned an invalid answer to session.settings"
    );
    connection.lost();
    forget(&address);
}
