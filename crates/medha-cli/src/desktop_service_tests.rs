use super::*;
use kernel::{
    ObsStatus, Observation, Protocol, ProviderState, Role, Session, TextPart, ToolIntent,
    TrustLabel,
};

fn kinds(page: &EventPage) -> Vec<&'static str> {
    page.events.iter().map(|event| event.kind).collect()
}

#[test]
fn provider_state_never_reaches_the_window_while_tool_input_does() {
    let session = Session::new();
    let call = Event::model_intent(
        &session,
        &ToolIntent {
            id: "call-1".into(),
            tool: "shell.exec".into(),
            args: json!({"command": "ls -la"}),
        },
    );
    let message = Event::model_message(
        &session,
        &ModelMessage {
            role: Role::Assistant,
            parts: vec![ContentPart::Text(TextPart {
                text: "Visible answer".into(),
                provider_state: vec![ProviderState {
                    protocol: Protocol::OpenAiResponses,
                    kind: "replay".into(),
                    value: json!("private-provider-state"),
                }],
            })],
            trust: None,
        },
    );
    let page = page_events(&[call, message], None, None).unwrap();
    let wire = serde_json::to_string(&page).unwrap();
    assert!(wire.contains("Visible answer"));
    assert_eq!(page.events[0].target.as_deref(), Some("ls -la"));
    assert!(!wire.contains("private-provider-state"));
}

#[test]
fn a_dispatched_subagent_carries_its_child_objective_and_outcome() {
    let parent = Session::new();
    let (child, dispatch) = (Ulid::new(), Ulid::new());
    let spawned = Event::agent_dispatched(
        &parent,
        dispatch,
        "kernel",
        child,
        "Audit the kernel loop",
        Ulid::new(),
    );
    let finished = Event::agent_report(
        &parent,
        EventKind::AgentCompleted,
        dispatch,
        child,
        json!({"status": "exhausted", "duration_ms": 4128}),
        TrustLabel::System,
    );
    let page = page_events(&[spawned, finished], None, None).unwrap();
    assert_eq!(kinds(&page), ["subagent"]);
    let row = &page.events[0];
    assert_eq!(row.text.as_deref(), Some("kernel"));
    assert_eq!(row.child_id, Some(child.to_string()));
    assert_eq!(row.detail.as_deref(), Some("Audit the kernel loop"));
    assert_eq!(row.status.as_deref(), Some("exhausted"));
    assert_eq!(row.duration_ms, Some(4128));
}

#[test]
fn an_unfinished_subagent_reports_no_outcome() {
    let parent = Session::new();
    let spawned =
        Event::agent_dispatched(&parent, Ulid::new(), "tools", Ulid::new(), "", Ulid::new());
    let page = page_events(&[spawned], None, None).unwrap();
    assert_eq!(page.events[0].status, None);
    assert_eq!(page.events[0].detail.as_deref(), Some(""));
}

#[test]
fn every_message_a_child_receives_comes_from_its_parent_agent() {
    let child = Session::new();
    let events = [
        Event::agent_spawned(&child, "kernel", "Audit", &[]),
        Event::user_message(&child, "You are a focused sub-agent."),
        Event::user_message(&child, "CORRECTION: check the tests too."),
    ];
    let page = page_events(&events, None, None).unwrap();
    assert_eq!(kinds(&page), ["handoff", "handoff"]);
}

#[test]
fn a_parent_sessions_messages_stay_the_persons() {
    let parent = Session::new();
    let events = [
        Event::user_message(&parent, "Audit the kernel"),
        Event::agent_dispatched(
            &parent,
            Ulid::new(),
            "kernel",
            Ulid::new(),
            "Audit",
            Ulid::new(),
        ),
        Event::user_message(&parent, "Thanks"),
    ];
    let page = page_events(&events, None, None).unwrap();
    assert_eq!(kinds(&page), ["user", "subagent", "user"]);
}

fn call(session: &Session, id: &str, args: Value) -> Event {
    Event::model_intent(
        session,
        &ToolIntent {
            id: id.into(),
            tool: "shell.exec".into(),
            args,
        },
    )
}

fn result(session: &Session, id: &str, status: ObsStatus, payload: Value) -> Event {
    Event::tool_obs(
        session,
        &Observation {
            intent_id: id.into(),
            status,
            payload,
            media: Vec::new(),
            relayed_trust: None,
            net_denied: false,
        },
        TrustLabel::System,
    )
}

#[test]
fn a_tool_call_is_one_line_with_a_verb_and_target() {
    let session = Session::new();
    let events = [call(
        &session,
        "c1",
        json!({"command": "cargo test -p kernel"}),
    )];
    let page = page_events(&events, None, None).unwrap();
    let step = &page.events[0];
    assert_eq!(step.verb.as_deref(), Some("Ran"));
    assert_eq!(step.target.as_deref(), Some("cargo test -p kernel"));
    assert_eq!((step.input.as_deref(), &step.plan), (None, &None));
}

#[test]
fn a_plan_update_carries_its_steps() {
    let session = Session::new();
    let events = [Event::model_intent(
        &session,
        &ToolIntent {
            id: "p1".into(),
            tool: "update_plan".into(),
            args: json!({"steps": [{"title": "Map the diff", "status": "in_progress"}]}),
        },
    )];
    let page = page_events(&events, None, None).unwrap();
    let plan = page.events[0].plan.as_ref().unwrap();
    assert_eq!(plan["steps"][0]["title"], "Map the diff");
    assert_eq!(plan["steps"][0]["status"], "in_progress");
}

#[test]
fn long_or_multiline_targets_are_clipped() {
    let session = Session::new();
    let events = [call(
        &session,
        "c1",
        json!({"command": "echo one\necho two"}),
    )];
    let page = page_events(&events, None, None).unwrap();
    assert_eq!(page.events[0].target.as_deref(), Some("echo one…"));
}

#[test]
fn a_failed_or_denied_step_says_why_and_every_step_carries_its_output() {
    let session = Session::new();
    let events = [
        call(&session, "a", json!({"command": "cat x"})),
        call(&session, "b", json!({"command": "rm -rf x"})),
        call(&session, "c", json!({"command": "ls | wc -l"})),
        result(
            &session,
            "a",
            ObsStatus::Error,
            json!({"error": "read failed: No such file"}),
        ),
        result(
            &session,
            "b",
            ObsStatus::Denied,
            json!({"reason": "rejected by human"}),
        ),
        result(
            &session,
            "c",
            ObsStatus::Ok,
            json!({"stdout": "3 files\n", "stderr": "", "exit_code": 0}),
        ),
    ];
    let page = page_events(&events, None, None).unwrap();
    let results = &page.events[3..];
    assert_eq!(results[2].output.as_deref(), Some("3 files"));
    assert_eq!(results[0].output, None);
    let details: Vec<_> = results
        .iter()
        .map(|event| event.detail.as_deref())
        .collect();
    assert_eq!(
        details,
        [
            Some("read failed: No such file"),
            Some("rejected by human"),
            None
        ]
    );
}

#[test]
fn reasoning_is_shown_with_how_long_it_took() {
    let session = Session::new();
    let mut asked = Event::user_message(&session, "hii");
    let mut thought = Event::model_reasoning(&session, "The user said hii.");
    asked.ts = 100.0;
    thought.ts = 107.5;
    let page = page_events(&[asked, thought], None, None).unwrap();
    assert_eq!(kinds(&page), ["user", "reasoning"]);
    assert_eq!(page.events[1].text.as_deref(), Some("The user said hii."));
    assert_eq!(page.events[1].duration_ms, Some(7500));
}

/// Every page of a history in turn, each checked to be no more than a page may be on the wire.
fn pages(events: &[Event]) -> Vec<Vec<EventView>> {
    let (mut pages, mut cursor) = (Vec::new(), None::<String>);
    loop {
        let page = page_events(events, cursor.as_deref(), None).unwrap();
        let bytes = serde_json::to_string(&page).unwrap().len();
        assert!(bytes <= PAGE_BYTES + 1024, "a page of {bytes} bytes");
        assert!(!page.events.is_empty(), "a page with nothing on it");
        pages.push(page.events);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return pages,
        }
    }
}

#[test]
fn a_page_of_history_is_bounded_in_bytes_and_the_next_takes_up_where_it_stopped() {
    let session = Session::new();
    let long = "x".repeat(PAGE_BYTES / 3);
    let events: Vec<Event> = (0..8).map(|_| Event::model_text(&session, &long)).collect();
    let pages = pages(&events);
    let shown: usize = pages.iter().map(Vec::len).sum();
    assert_eq!(shown, events.len(), "an event was lost between pages");
    assert!(pages.len() > 1, "everything went out as one page");
}

#[test]
fn history_that_grows_as_it_is_written_still_fits_and_an_event_too_large_alone_is_cut() {
    let session = Session::new();
    // A control character is stored as one byte and written as six.
    let grows = "\u{1}".repeat(200_000);
    let mut events: Vec<Event> = (0..20)
        .map(|_| Event::model_text(&session, &grows))
        .collect();
    events.push(Event::model_text(&session, &"\u{1}".repeat(PAGE_BYTES)));

    let shown: Vec<EventView> = pages(&events).into_iter().flatten().collect();
    assert_eq!(shown.len(), events.len(), "an event was lost between pages");
    let (whole, large) = shown.split_at(20);
    assert!(
        whole
            .iter()
            .all(|event| event.text.as_ref() == Some(&grows))
    );
    let cut = large[0].text.as_deref().unwrap();
    assert!(
        cut.ends_with(CUT) && cut.starts_with('\u{1}'),
        "{}",
        cut.len()
    );
}

#[test]
fn a_handoff_stays_marked_when_it_starts_the_next_page() {
    let child = Session::new();
    let events = [
        Event::agent_spawned(&child, "kernel", "Audit", &[]),
        Event::user_message(&child, "You are a focused sub-agent."),
    ];
    let cursor = events[0].id.to_string();
    let page = page_events(&events, Some(&cursor), None).unwrap();
    assert_eq!(kinds(&page), ["handoff"]);
}

fn read(session: &Session, id: &str, args: Value) -> Event {
    Event::model_intent(
        session,
        &ToolIntent {
            id: id.into(),
            tool: "read".into(),
            args,
        },
    )
}

#[test]
fn paging_a_stored_read_names_the_file_even_across_pages() {
    let session = Session::new();
    let events = [
        read(&session, "r1", json!({"path": "docs/WHAT_IS_MEDHA.md"})),
        result(
            &session,
            "r1",
            ObsStatus::Ok,
            json!({"content": "…", "hash": "64d557f8bb"}),
        ),
        read(
            &session,
            "r2",
            json!({"hash": "64d557f8bb", "offset": 14000, "length": 20000}),
        ),
    ];
    let whole = page_events(&events, None, None).unwrap();
    assert_eq!(
        whole.events[2].target.as_deref(),
        Some("docs/WHAT_IS_MEDHA.md · 14,000–34,000")
    );
    let cursor = events[1].id.to_string();
    let later = page_events(&events, Some(&cursor), None).unwrap();
    assert_eq!(
        later.events[0].target.as_deref(),
        Some("docs/WHAT_IS_MEDHA.md · 14,000–34,000")
    );
}

#[test]
fn only_what_the_person_typed_shows_as_their_message() {
    let session = Session::new();
    let mut hook = Event::user_input(
        &session,
        "[session_start hook p/c] be terse",
        TrustLabel::Tool,
    );
    hook.payload["hook"] = json!({"point": "session_start", "plugin": "p", "component": "c"});
    let events = [
        Event::user_message(&session, "hii"),
        hook,
        Event::user_input(
            &session,
            "[background agent 'kernel' finished — completed]",
            TrustLabel::Tool,
        ),
        Event::user_input(
            &session,
            "[verifier] FAIL — 2 tests\nassert failed",
            TrustLabel::Tool,
        ),
    ];
    let page = page_events(&events, None, None).unwrap();
    assert_eq!(kinds(&page), ["user", "verification"]);
    assert_eq!(page.events[0].text.as_deref(), Some("hii"));
    let check = &page.events[1];
    assert_eq!(
        (
            check.status.as_deref(),
            check.text.as_deref(),
            check.output.as_deref()
        ),
        (Some("failed"), Some("2 tests"), Some("assert failed"))
    );
}

#[test]
fn a_recorded_verifier_result_shows_as_a_check_and_a_person_quoting_one_does_not() {
    let session = Session::new();
    let mut check = Event::user_input(&session, "[verifier] PASS — ok", TrustLabel::Tool);
    check.payload["verifier"] = json!({"ok": true, "summary": "cargo test passed", "output": ""});
    let events = [
        check,
        Event::user_message(&session, "[verifier] PASS — typed by me"),
    ];
    let page = page_events(&events, None, None).unwrap();
    assert_eq!(kinds(&page), ["verification", "user"]);
    assert_eq!(page.events[0].status.as_deref(), Some("passed"));
    assert_eq!(page.events[0].text.as_deref(), Some("cargo test passed"));
    assert_eq!(page.events[0].output, None);
}

#[tokio::test]
async fn a_subagent_session_is_listed_under_the_chat_that_dispatched_it() {
    let dir = tempfile::tempdir().unwrap();
    let log = store::SqliteLog::open(dir.path().join("events.db")).unwrap();
    let (parent, child, unrelated) = (Session::new(), Session::new(), Session::new());
    let dispatch = Event::agent_dispatched(
        &parent,
        Ulid::new(),
        "kernel",
        child.id,
        "Audit the kernel loop",
        Ulid::new(),
    );
    for event in [
        Event::user_message(&parent, "Review the kernel"),
        dispatch,
        Event::user_message(&child, "Audit the kernel loop"),
        Event::user_message(&unrelated, "Something else"),
    ] {
        log.append(event).await.unwrap();
    }
    let views = list_session_views(&log).unwrap();
    let view = |id: Ulid| views.iter().find(|view| view.id == id.to_string()).unwrap();
    assert_eq!(view(child.id).parent_id, Some(parent.id.to_string()));
    assert_eq!(view(child.id).title, "kernel");
    assert_eq!(view(parent.id).parent_id, None);
    assert_eq!(view(parent.id).title, "Review the kernel");
    assert_eq!(view(unrelated.id).parent_id, None);
}
