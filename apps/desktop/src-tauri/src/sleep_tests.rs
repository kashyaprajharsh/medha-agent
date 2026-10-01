use super::*;

fn slept(session: Option<&str>) -> Value {
    json!({ "jsonrpc": "2.0", "id": ASK, "result": {
        "slept": true, "session": session, "settings": { "mode": "auto", "profile": "fast" } } })
}

#[test]
fn a_chat_is_asked_once_and_only_after_nothing_was_asked_of_it_for_the_grace() {
    let rest = Rest::new();
    assert!(rest.ask(Instant::now() + GRACE / 2, GRACE).is_none());
    rest.touch();
    let used = Instant::now();
    assert!(
        rest.ask(used + GRACE / 2, GRACE).is_none(),
        "a request restarts the count"
    );
    let ask = rest
        .ask(used + GRACE, GRACE)
        .expect("unused for the whole grace");
    assert_eq!(ask["method"], "session.sleep");
    assert!(
        rest.ask(used + GRACE * 3, GRACE).is_none(),
        "never asked twice"
    );
}

#[test]
fn a_busy_answer_keeps_the_chat_awake_and_restarts_the_count() {
    let rest = Rest::new();
    rest.ask(Instant::now() + GRACE, GRACE).unwrap();
    assert!(rest.answer(&json!({ "id": ASK, "result": { "slept": false } })));
    assert_eq!(rest.settle(), Ok(Settled::Awake));
    assert!(rest.ask(Instant::now() + GRACE / 2, GRACE).is_none());
}

#[test]
fn a_request_while_asking_waits_for_the_answer_and_a_yes_wakes_it_at_once() {
    let rest = Rest::new();
    rest.ask(Instant::now() + GRACE, GRACE).unwrap();
    let waiter = {
        let rest = Arc::clone(&rest);
        std::thread::spawn(move || rest.settle())
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !waiter.is_finished(),
        "nothing is written before the answer"
    );
    assert!(rest.answer(&slept(Some("01M3CEMMYGTBYNFBAERDWV1A0W"))));
    let Ok(Settled::Asleep(wake)) = waiter.join().unwrap() else {
        panic!("a yes means the chat can be started again");
    };
    assert_eq!(wake.resume.as_deref(), Some("01M3CEMMYGTBYNFBAERDWV1A0W"));
    assert_eq!(wake.settings.unwrap()["profile"], "fast");
    assert!(
        rest.closed(),
        "the old process leaving is not reported as a stop"
    );
}

#[test]
fn an_unanswered_question_gives_up_instead_of_freezing_the_window() {
    let rest = Rest::new();
    rest.ask(Instant::now() + GRACE, GRACE).unwrap();
    let started = Instant::now();
    assert!(rest.settle().is_err());
    assert!(started.elapsed() < PATIENCE + Duration::from_secs(1));
}

#[test]
fn a_chat_with_nothing_saved_wakes_fresh() {
    let rest = Rest::new();
    rest.ask(Instant::now() + GRACE, GRACE).unwrap();
    rest.answer(&slept(None));
    rest.closed();
    assert_eq!(
        rest.settle(),
        Ok(Settled::Asleep(Wake {
            resume: None,
            settings: Some(json!({ "mode": "auto", "profile": "fast" })),
        }))
    );
}

#[test]
fn a_process_that_dies_before_answering_is_still_reported_as_stopped() {
    let rest = Rest::new();
    rest.ask(Instant::now() + GRACE, GRACE).unwrap();
    assert!(!rest.closed());
    assert_eq!(rest.settle(), Ok(Settled::Awake));
    let awake = Rest::new();
    assert!(!awake.closed());
}

#[test]
fn only_the_sleep_answer_is_taken_out_of_the_stream() {
    let rest = Rest::new();
    for frame in [
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "slept": true } }),
        json!({ "method": "event", "params": { "kind": "turn.done" } }),
        json!({ "method": "ready", "params": {} }),
    ] {
        assert!(!rest.answer(&frame));
    }
    assert_eq!(rest.settle(), Ok(Settled::Awake));
}
