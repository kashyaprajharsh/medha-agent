use super::*;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf, duplex, split};

/// A chat that says what it is told to, so routing can be checked without a model.
struct Stub {
    opened: AtomicU64,
    /// What a slow request waits for.
    released: tokio::sync::Notify,
}

async fn say(output: &mut DuplexStream, frame: Value) {
    wire::write_frame(output, &frame).await;
}

async fn chat(input: DuplexStream, mut output: DuplexStream) -> Result<(), String> {
    let mut input = BufReader::new(input);
    while let Some(frame) = wire::read_frame(&mut input).await {
        let (id, params) = (frame["id"].clone(), frame["params"].clone());
        match frame["method"].as_str() {
            Some("hello") => say(&mut output, json!({"id": id, "result": "the chat's own"})).await,
            Some("say") => {
                say(&mut output, json!({"method": "event", "params": params})).await;
                say(
                    &mut output,
                    json!({"id": id, "result": {"said": params["text"]}}),
                )
                .await;
            }
            Some("burst") => {
                let pad = "x".repeat(params["bytes"].as_u64().unwrap_or(0) as usize);
                for n in 0..params["count"].as_u64().unwrap() {
                    let event = json!({"method": "event", "params": {"n": n, "pad": pad}});
                    say(&mut output, event).await;
                }
                say(&mut output, json!({"id": id, "result": {}})).await;
            }
            Some("noise") => {
                output.write_all(b"not a frame\n").await.unwrap();
                say(&mut output, json!({"id": id, "result": {}})).await;
            }
            // Stops reading what it is asked, as a chat does while it waits on something else.
            Some("stall") => {
                say(&mut output, json!({"id": id, "result": "stalled"})).await;
                let long = params["ms"].as_u64().unwrap_or(3_600_000);
                tokio::time::sleep(Duration::from_millis(long)).await;
            }
            Some("cancel") => say(&mut output, json!({"id": id, "result": "cancelled"})).await,
            Some("huge") => {
                let answer = json!({"id": id, "result": "x".repeat(wire::MAX_FRAME)});
                say(&mut output, answer).await;
            }
            Some("huge_error") => {
                // serde emits error before id; the id is beyond the retained CHAT_FRAME prefix.
                let answer = json!({"id": id, "error": {"code": -32000, "message": "x".repeat(wire::MAX_FRAME - 512)}});
                assert!(answer.to_string().len() < wire::MAX_FRAME);
                say(&mut output, answer).await;
            }
            Some("break") => return Err("it broke".into()),
            Some("panic") => panic!("the chat panicked"),
            _ => {}
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl Chats for Stub {
    async fn open(&self, params: &Value) -> Result<Opened, String> {
        if let Some(reason) = params["refuse"].as_str() {
            return Err(reason.into());
        }
        if params["slow"] == true {
            self.released.notified().await;
        }
        let n = self.opened.fetch_add(1, Ordering::Relaxed);
        let session = match params["resume"].as_str() {
            Some(id) => id.to_string(),
            None => format!("chat-{n}"),
        };
        let (to_chat, chat_in) = duplex(64 * 1024);
        let (chat_out, from_chat) = duplex(64 * 1024);
        let task = tokio::spawn(chat(chat_in, chat_out));
        Ok(Opened {
            session,
            about: json!({"folder": params["folder"]}),
            input: Box::new(to_chat),
            output: Box::new(from_chat),
            done: Box::pin(async move {
                task.await
                    .unwrap_or_else(|_| Err("the chat stopped unexpectedly".into()))
            }),
            cancel: (params["out_of_band"] == true)
                .then(|| Arc::new(|| true) as Arc<dyn Fn() -> bool + Send + Sync>),
        })
    }

    async fn about_folder(&self, request: &Value) -> Result<Value, String> {
        match request["method"].as_str() {
            Some("history") => Ok(json!({"of": request["folder"]})),
            Some("everything") => Ok(json!("x".repeat(wire::MAX_FRAME))),
            Some("install") => {
                self.released.notified().await;
                Ok(json!({"installed": request["params"]["what"]}))
            }
            _ => Err("unknown method".into()),
        }
    }
}

fn backend() -> Arc<Backend<Stub>> {
    Backend::new(
        Stub {
            opened: AtomicU64::new(1),
            released: tokio::sync::Notify::new(),
        },
        "test",
    )
}

#[tokio::test]
async fn chat_capacity_counts_starts_and_reports_explicit_overload() {
    let backend = Backend::with_chat_limit(
        Stub {
            opened: AtomicU64::new(1),
            released: tokio::sync::Notify::new(),
        },
        "test",
        1,
    );
    let mut first = connect(&backend);
    let mut second = connect(&backend);
    let starting = first
        .post(json!({"method": "session.create", "params": {"slow": true}}))
        .await;
    until("start admission", || {
        backend.status()["starting_chats"] == 1
    })
    .await;
    let refused = second.ask("session.create", None, json!({})).await;
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("active-chat limit")
    );
    assert!(
        second
            .ask("hello", None, json!({}))
            .await
            .get("result")
            .is_some()
    );
    backend.chats.released.notify_one();
    let made = first.answer(starting).await;
    let id = made["result"]["session"].as_str().unwrap();
    assert_eq!(backend.live(), 1);
    first.attach(id, None).await;
    first.ask("session.close", Some(id), json!({})).await;
    until("capacity released", || backend.live() == 0).await;
    assert!(
        second
            .ask("session.create", None, json!({}))
            .await
            .get("result")
            .is_some()
    );
}

struct Tester {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    events: VecDeque<Value>,
    asked: u64,
}

fn connect_with(backend: &Arc<Backend<Stub>>, pipe: usize) -> Tester {
    let (mine, theirs) = duplex(pipe);
    let (reader, writer) = split(theirs);
    let backend = Arc::clone(backend);
    tokio::spawn(async move { backend.serve(reader, writer).await });
    let (reader, writer) = split(mine);
    Tester {
        reader: BufReader::new(reader),
        writer,
        events: VecDeque::new(),
        asked: 0,
    }
}

fn connect(backend: &Arc<Backend<Stub>>) -> Tester {
    connect_with(backend, 4 * 1024 * 1024)
}

impl Tester {
    async fn frame(&mut self) -> Option<Value> {
        tokio::time::timeout(Duration::from_secs(10), wire::read_frame(&mut self.reader))
            .await
            .expect("the backend went quiet")
    }

    /// The whole answer frame; events that arrive first are kept for `event`.
    async fn ask(&mut self, method: &str, session: Option<&str>, params: Value) -> Value {
        self.ask_as(None, method, session, params).await
    }

    async fn ask_as(
        &mut self,
        id: Option<u64>,
        method: &str,
        session: Option<&str>,
        params: Value,
    ) -> Value {
        self.asked += 1;
        let id = id.unwrap_or(self.asked);
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Some(session) = session {
            request["session"] = json!(session);
        }
        wire::write_frame(&mut self.writer, &request).await;
        self.answer(id).await
    }

    /// Sends a request without waiting for its answer.
    async fn post(&mut self, mut request: Value) -> u64 {
        self.asked += 1;
        request["id"] = json!(self.asked);
        wire::write_frame(&mut self.writer, &request).await;
        self.asked
    }

    async fn answer(&mut self, id: u64) -> Value {
        loop {
            let frame = self
                .frame()
                .await
                .expect("the backend closed the connection");
            if frame["id"] == json!(id) {
                return frame;
            }
            self.events.push_back(frame);
        }
    }

    /// The next event as `(seq, frame the chat wrote)`.
    async fn event(&mut self) -> (u64, Value) {
        let frame = match self.events.pop_front() {
            Some(frame) => frame,
            None => self
                .frame()
                .await
                .expect("the backend closed the connection"),
        };
        assert_eq!(frame["method"], "session.event", "{frame}");
        let params = &frame["params"];
        (params["seq"].as_u64().unwrap(), params["frame"].clone())
    }

    async fn open(&mut self) -> String {
        let made = self.ask("session.create", None, json!({})).await;
        let session = made["result"]["session"].as_str().unwrap().to_string();
        self.attach(&session, None).await;
        session
    }

    async fn attach(&mut self, session: &str, after: Option<u64>) -> Value {
        let params = after.map_or(json!({}), |after| json!({"after": after}));
        self.ask("session.attach", Some(session), params).await["result"].clone()
    }

    async fn say(&mut self, session: &str, text: &str) -> Value {
        self.ask("say", Some(session), json!({"text": text})).await
    }
}

async fn until(what: &str, mut holds: impl FnMut() -> bool) {
    for _ in 0..500 {
        if holds() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never happened: {what}");
}

#[tokio::test]
async fn two_clients_on_one_chat_see_one_stream_and_each_gets_only_its_own_answer() {
    let backend = backend();
    let (mut first, mut second) = (connect(&backend), connect(&backend));
    let session = first.open().await;
    second.attach(&session, None).await;

    // Both use request id 7; neither may receive the other's answer.
    let one = first
        .ask_as(Some(7), "say", Some(&session), json!({"text": "one"}))
        .await;
    let two = second
        .ask_as(Some(7), "say", Some(&session), json!({"text": "two"}))
        .await;
    assert_eq!(one["result"]["said"], "one");
    assert_eq!(two["result"]["said"], "two");

    for client in [&mut first, &mut second] {
        let (a, b) = (client.event().await, client.event().await);
        assert_eq!((a.0, &a.1["params"]["text"]), (1, &json!("one")));
        assert_eq!((b.0, &b.1["params"]["text"]), (2, &json!("two")));
    }
}

#[tokio::test]
async fn a_client_cannot_speak_to_a_chat_it_has_not_attached_to() {
    let backend = backend();
    let (mut owner, mut stranger) = (connect(&backend), connect(&backend));
    let session = owner.open().await;

    let refused = stranger.say(&session, "let me in").await;
    assert_eq!(refused["error"]["message"], "attach to the session first");
    let refused = stranger
        .ask("session.close", Some(&session), json!({}))
        .await;
    assert_eq!(refused["error"]["message"], "attach to the session first");
    assert_eq!(
        stranger.say("no-such-chat", "hello").await["error"]["message"],
        "no such session"
    );
    assert_eq!(
        stranger.ask("say", None, json!({})).await["error"]["code"],
        -32602
    );

    assert_eq!(
        owner.say(&session, "still mine").await["result"]["said"],
        "still mine"
    );
    assert_eq!(owner.event().await.1["params"]["text"], "still mine");
    assert!(
        owner.events.is_empty(),
        "the stranger's words reached the chat"
    );
}

#[tokio::test]
async fn attaching_to_a_chat_gives_no_way_into_a_later_chat_of_the_same_id() {
    let backend = backend();
    let (mut first, mut second) = (connect(&backend), connect(&backend));
    let resume = json!({"resume": "kept"});
    first.ask("session.create", None, resume.clone()).await;
    first.attach("kept", None).await;
    first.ask("session.close", Some("kept"), json!({})).await;
    until("the closed chat leaves the table", || backend.live() == 0).await;

    second.ask("session.create", None, resume).await;
    let refused = first.say("kept", "still me").await;
    assert_eq!(refused["error"]["message"], "attach to the session first");
    let refused = first.ask("session.close", Some("kept"), json!({})).await;
    assert_eq!(refused["error"]["message"], "attach to the session first");
}

#[tokio::test]
async fn a_client_that_comes_back_with_its_cursor_misses_nothing() {
    let backend = backend();
    let mut first = connect(&backend);
    let session = first.open().await;
    first.say(&session, "a").await;
    let (cursor, _) = first.event().await;
    drop(first);

    let mut other = connect(&backend);
    other.attach(&session, None).await;
    for text in ["b", "c", "d"] {
        other.say(&session, text).await;
    }

    let mut back = connect(&backend);
    let attached = back.attach(&session, Some(cursor)).await;
    assert_eq!(
        (attached["replayed"].as_u64(), attached["gap"].as_bool()),
        (Some(3), Some(false))
    );
    other.say(&session, "e").await;
    let mut seen = Vec::new();
    for _ in 0..4 {
        let (seq, frame) = back.event().await;
        seen.push((seq, frame["params"]["text"].as_str().unwrap().to_string()));
    }
    let expected =
        [(2, "b"), (3, "c"), (4, "d"), (5, "e")].map(|(seq, text)| (seq, text.to_string()));
    assert_eq!(seen, expected);
}

#[tokio::test]
async fn a_cursor_older_than_what_is_kept_is_told_so() {
    let backend = backend();
    let mut driver = connect(&backend);
    let session = driver.open().await;
    let count = session::KEPT_FRAMES as u64 + 10;
    driver
        .ask("burst", Some(&session), json!({"count": count}))
        .await;

    let mut late = connect(&backend);
    let attached = late.attach(&session, Some(0)).await;
    assert_eq!(attached["gap"], true);
    assert_eq!(attached["replayed"], session::KEPT_FRAMES);
    assert_eq!(
        late.event().await.0,
        11,
        "the oldest kept event comes first"
    );

    let mut current = connect(&backend);
    assert_eq!(current.attach(&session, Some(count)).await["gap"], false);
    let mut ahead = connect(&backend);
    assert_eq!(ahead.attach(&session, Some(count + 5)).await["gap"], true);
}

#[tokio::test]
async fn a_cursor_no_chat_could_have_reached_is_a_gap_and_harms_nobody() {
    let backend = backend();
    let mut client = connect(&backend);
    let session = client.open().await;
    client.say(&session, "one").await;

    for ahead in [u64::MAX, u64::MAX - 1, 2] {
        let attached = client.attach(&session, Some(ahead)).await;
        assert_eq!(attached["gap"], true, "after {ahead}: {attached}");
        assert_eq!(attached["replayed"], 0);
    }
    assert_eq!(client.attach(&session, Some(1)).await["gap"], false);
    assert_eq!(
        client.say(&session, "still served").await["result"]["said"],
        "still served"
    );
}

#[tokio::test]
async fn a_chat_that_dies_tells_its_clients_and_the_others_keep_running() {
    let backend = backend();
    let mut client = connect(&backend);
    let (doomed, broken, healthy) = (
        client.open().await,
        client.open().await,
        client.open().await,
    );

    let request = json!({"method": "panic", "session": doomed});
    wire::write_frame(&mut client.writer, &request).await;
    let (_, ended) = client.event().await;
    assert_eq!(ended["method"], "session.ended");
    assert_eq!(ended["params"]["error"], "the chat stopped unexpectedly");

    let request = json!({"method": "break", "session": broken});
    wire::write_frame(&mut client.writer, &request).await;
    assert_eq!(client.event().await.1["params"]["error"], "it broke");

    until("the dead chats leave the table", || backend.live() == 1).await;
    assert_eq!(
        client.say(&healthy, "alive").await["result"]["said"],
        "alive"
    );
    let gone = client.say(&doomed, "anyone?").await;
    assert_eq!(gone["error"]["message"], "no such session");
}

#[tokio::test]
async fn a_client_too_slow_to_keep_up_is_dropped_and_the_chat_goes_on() {
    let backend = backend();
    let mut driver = connect(&backend);
    let session = driver.open().await;
    let mut stalled = connect_with(&backend, 1024);
    stalled.attach(&session, None).await;

    driver
        .ask("burst", Some(&session), json!({"count": 8000}))
        .await;
    driver.events.clear();
    let listed = driver.ask("session.list", None, json!({})).await;
    assert_eq!(listed["result"]["sessions"][0]["clients"], 1, "{listed}");
    assert_eq!(
        driver.say(&session, "on we go").await["result"]["said"],
        "on we go"
    );

    // What was already on its way is still readable; then the connection ends.
    while stalled.frame().await.is_some() {}
}

#[tokio::test]
async fn a_client_holding_too_many_bytes_unread_is_dropped_and_the_chat_goes_on() {
    let backend = backend();
    let mut stalled = connect_with(&backend, 1024);
    let session = stalled.open().await;
    // Far fewer frames than a client may queue, and more bytes than it may.
    let frame = 2 * 1024 * 1024;
    let count = crate::client::QUEUED_BYTES / frame + 8;
    stalled
        .post(json!({"method": "burst", "session": session,
            "params": {"count": count, "bytes": frame}}))
        .await;

    let mut other = connect(&backend);
    for _ in 0..500 {
        let listed = other.ask("session.list", None, json!({})).await;
        if listed["result"]["sessions"][0]["clients"] == 0 {
            other.attach(&session, None).await;
            let said = other.say(&session, "on we go").await;
            assert_eq!(said["result"]["said"], "on we go");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("a client holding more than it may was never dropped");
}

#[tokio::test]
async fn an_answer_or_a_frame_too_large_to_read_breaks_no_connection_and_no_chat() {
    let backend = backend();
    let mut client = connect_with(&backend, 64 * 1024 * 1024);
    let refused = client
        .post(json!({"method": "everything", "folder": "/w"}))
        .await;
    let refused = client.answer(refused).await;
    assert_eq!(
        refused["error"]["message"],
        "the answer is too large to send at once"
    );

    let session = client.open().await;
    let burst = json!({"count": 1, "bytes": wire::MAX_FRAME});
    client.ask("burst", Some(&session), burst).await;
    // An answer of the chat's own that large is not left unanswered: whoever asked is told.
    let huge = client.ask("huge", Some(&session), json!({})).await;
    assert_eq!(
        huge["error"]["message"],
        "the answer is too large to send at once"
    );
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        client.ask("huge_error", Some(&session), json!({})),
    )
    .await
    .expect("an oversized error was left unanswered");
    assert_eq!(
        error["error"]["message"],
        "the answer is too large to send at once"
    );
    assert_eq!(
        client.say(&session, "after").await["result"]["said"],
        "after"
    );
    assert_eq!(client.event().await.1["params"]["text"], "after");
}

#[tokio::test]
async fn a_chat_that_stops_reading_holds_up_nothing_else_and_can_still_be_closed() {
    let backend = backend();
    let mut client = connect(&backend);
    let (stalled, healthy) = (client.open().await, client.open().await);
    assert_eq!(
        client.ask("stall", Some(&stalled), json!({})).await["result"],
        "stalled"
    );

    // More than the chat's pipe holds, so writing to it cannot finish.
    let large = json!({"text": "x".repeat(200 * 1024)});
    for _ in 0..3 {
        client
            .post(json!({"method": "say", "session": stalled, "params": large}))
            .await;
    }
    let hello = client.ask("hello", None, json!({})).await;
    assert_eq!(hello["result"]["protocol"], PROTOCOL);
    assert_eq!(
        client.say(&healthy, "unaffected").await["result"]["said"],
        "unaffected"
    );
    let history = client
        .post(json!({"method": "history", "folder": "/w"}))
        .await;
    assert_eq!(client.answer(history).await["result"], json!({"of": "/w"}));

    // What it has not read is bounded, and one request more is refused at once.
    for _ in 0..(session::INPUT_FRAMES + 8) {
        client
            .post(json!({"method": "say", "session": stalled, "params": {"text": "one more"}}))
            .await;
    }
    let refused = loop {
        let frame = client.frame().await.expect("the connection was dropped");
        if frame.get("error").is_some() {
            break frame;
        }
    };
    let why = refused["error"]["message"].as_str().unwrap();
    assert!(why.contains("has not read"), "{why}");

    // What stops a turn is taken all the same, and only so much of that.
    let stop = json!({"method": "cancel", "session": stalled});
    let first = client.post(stop.clone()).await;
    for _ in 0..session::URGENT_FRAMES {
        client.post(stop.clone()).await;
    }
    let refused = loop {
        let frame = client.frame().await.expect("the connection was dropped");
        if frame.get("error").is_some() && frame["id"].as_u64() >= Some(first) {
            break frame;
        }
    };
    assert_eq!(
        refused["id"].as_u64(),
        Some(first + session::URGENT_FRAMES as u64),
        "{refused}"
    );

    let closing = client.ask("session.close", Some(&stalled), json!({})).await;
    assert_eq!(closing["result"]["closing"], true);
    let after = client.say(&stalled, "after it was closed").await;
    assert_eq!(after["error"]["message"], "this chat has ended");
}

#[tokio::test]
async fn what_stops_a_turn_is_read_before_the_requests_that_were_waiting() {
    let backend = backend();
    let mut client = connect(&backend);
    let session = client.open().await;
    let paused = client
        .ask("stall", Some(&session), json!({"ms": 500}))
        .await;
    assert_eq!(paused["result"], "stalled");

    // Each is more than the chat's pipe holds, so at most the first is part written
    // by the time the chat reads again; the rest wait behind it.
    let large = json!({"text": "x".repeat(200 * 1024)});
    let mut waiting = Vec::new();
    for _ in 0..4 {
        let say = json!({"method": "say", "session": session, "params": large});
        waiting.push(client.post(say).await);
    }
    let cancel = client
        .post(json!({"method": "cancel", "session": session}))
        .await;

    let mut answered = Vec::new();
    while answered.len() < 5 {
        let frame = client.frame().await.expect("the connection was dropped");
        if frame.get("method").is_none() {
            assert!(frame.get("result").is_some(), "{frame}");
            answered.push(frame["id"].as_u64().unwrap());
        }
    }
    let at = answered.iter().position(|id| *id == cancel).unwrap();
    assert!(
        at <= 1,
        "sent {waiting:?} then {cancel}, read as {answered:?}"
    );
    answered.remove(at);
    assert_eq!(answered, waiting, "what was waiting lost its order");
}

#[tokio::test]
async fn a_chat_is_resumed_once_and_closed_by_whoever_is_attached() {
    let backend = backend();
    let mut client = connect(&backend);
    let made = client
        .ask(
            "session.create",
            None,
            json!({"resume": "kept", "folder": "/w"}),
        )
        .await;
    assert_eq!(made["result"]["session"], "kept");
    assert_eq!(made["result"]["about"]["folder"], "/w");
    let again = client
        .ask("session.create", None, json!({"resume": "kept"}))
        .await;
    assert_eq!(
        again["error"]["message"],
        "this chat is already live; attach to it"
    );
    let refused = client
        .ask("session.create", None, json!({"refuse": "no model"}))
        .await;
    assert_eq!(refused["error"]["message"], "no model");

    client.attach("kept", None).await;
    // A line that is not a frame does not stop the chat being read.
    client.ask("noise", Some("kept"), json!({})).await;
    assert_eq!(client.say("kept", "after").await["result"]["said"], "after");
    assert_eq!(client.event().await.1["params"]["text"], "after");
    // A request that names a chat reaches the chat, even one the backend also answers.
    let greeted = client.ask("hello", Some("kept"), json!({})).await;
    assert_eq!(greeted["result"], "the chat's own");

    assert_eq!(
        client.ask("session.close", Some("kept"), json!({})).await["result"]["closing"],
        true
    );
    let (_, ended) = client.event().await;
    assert_eq!(
        (&ended["method"], &ended["params"]["error"]),
        (&json!("session.ended"), &Value::Null)
    );
    until("the closed chat leaves the table", || backend.live() == 0).await;
    let hello = client.ask("hello", None, json!({})).await;
    assert_eq!(hello["result"], backend.identity());
}

#[tokio::test]
async fn a_slow_request_does_not_hold_up_the_client_that_sent_it() {
    let backend = backend();
    let mut client = connect(&backend);
    let installing = client
        .post(json!({"method": "install", "folder": "/w", "params": {"what": "a plugin"}}))
        .await;
    let creating = client
        .post(json!({"method": "session.create", "params": {"slow": true}}))
        .await;

    let history = client
        .post(json!({"method": "history", "folder": "/w"}))
        .await;
    assert_eq!(client.answer(history).await["result"], json!({"of": "/w"}));
    let chat = client.open().await;
    assert_eq!(
        client.say(&chat, "meanwhile").await["result"]["said"],
        "meanwhile"
    );
    let unknown = client
        .post(json!({"method": "nonsense", "folder": "/w"}))
        .await;
    assert_eq!(
        client.answer(unknown).await["error"]["message"],
        "unknown method"
    );

    backend.chats.released.notify_waiters();
    assert_eq!(
        client.answer(installing).await["result"]["installed"],
        "a plugin"
    );
    assert!(client.answer(creating).await["result"]["session"].is_string());
}

#[tokio::test]
async fn cancellation_reaches_a_turn_without_waiting_for_its_unread_input_pipe() {
    let backend = backend();
    let mut client = connect(&backend);
    let created = client
        .ask("session.create", None, json!({"out_of_band": true}))
        .await;
    let session = created["result"]["session"].as_str().unwrap().to_owned();
    let not_attached = client.ask("cancel", Some(&session), json!({})).await;
    assert!(not_attached.get("error").is_some());
    client
        .ask("session.attach", Some(&session), json!({}))
        .await;
    client
        .ask("stall", Some(&session), json!({"ms": 1000}))
        .await;
    client
        .post(json!({"session": session, "method": "say", "params": {"text": "x".repeat(100_000)}}))
        .await;
    let cancelled = tokio::time::timeout(
        Duration::from_millis(300),
        client.ask("cancel", Some(&session), json!({})),
    )
    .await
    .unwrap();
    assert_eq!(cancelled["result"]["cancelled"], true);
    assert!(
        client
            .ask("hello", None, json!({}))
            .await
            .get("result")
            .is_some()
    );
}

#[tokio::test]
async fn a_tied_chat_ends_with_the_last_client_watching_it_and_any_other_goes_on() {
    let backend = backend();
    let mut starter = connect(&backend);
    let mut watching = connect(&backend);
    let tied = starter
        .ask("session.create", None, json!({"ends_with_client": true}))
        .await["result"]["session"]
        .as_str()
        .unwrap()
        .to_string();
    starter.attach(&tied, None).await;
    let kept = starter.open().await;
    watching.attach(&tied, None).await;

    // Whoever started it going does not end it while another client watches.
    drop(starter);
    assert_eq!(
        watching.say(&tied, "still watched").await["result"]["said"],
        "still watched"
    );
    assert_eq!(backend.live(), 2);

    watching.ask("session.detach", Some(&tied), json!({})).await;
    until("the tied chat ends with its last viewer", || {
        backend.live() == 1
    })
    .await;
    watching.attach(&kept, None).await;
    assert_eq!(
        watching.say(&kept, "still here").await["result"]["said"],
        "still here"
    );
}

#[tokio::test]
async fn a_tied_chat_whose_only_client_goes_ends_with_it() {
    let backend = backend();
    let mut only = connect(&backend);
    let made = json!({"ends_with_client": true});
    let tied = only.ask("session.create", None, made).await["result"]["session"]
        .as_str()
        .unwrap()
        .to_string();
    only.attach(&tied, None).await;
    assert_eq!(only.say(&tied, "here").await["result"]["said"], "here");
    drop(only);
    until("the tied chat ends with its client", || backend.live() == 0).await;
}

#[tokio::test]
async fn a_client_gone_before_its_tied_chat_started_leaves_no_chat_behind() {
    let backend = backend();
    let mut client = connect(&backend);
    client
        .post(json!({"method": "session.create",
            "params": {"slow": true, "ends_with_client": true}}))
        .await;
    drop(client);
    // Give the backend time to see the client go before its chat starts.
    tokio::time::sleep(Duration::from_millis(50)).await;
    backend.chats.released.notify_waiters();
    until("the chat starts", || {
        backend.chats.opened.load(Ordering::Relaxed) == 2
    })
    .await;
    until("the orphan ends", || backend.live() == 0).await;
}
