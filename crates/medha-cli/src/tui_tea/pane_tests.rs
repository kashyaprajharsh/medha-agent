//! Pane behavior remains a frontend concern; execution is tested at the backend boundary.
use super::*;

fn model() -> (Model, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("medha-pane-")
        .tempdir()
        .unwrap();
    let model = Model::new(
        "m".into(),
        None,
        kernel::ReasoningConfig::default(),
        lockfile::UiConfig::default(),
        HashMap::new(),
        Arc::new(WorkspaceSandbox::new_jailed(dir.path()).unwrap()),
    );
    (model, dir)
}

fn path(name: &str) -> orchestrator::AgentPath {
    orchestrator::AgentPath::root().child(name).unwrap()
}

fn shown(model: &Model) -> Vec<String> {
    model
        .items
        .iter()
        .filter_map(|entry| match &entry.item {
            Item::User(text) => Some(format!("user:{text}")),
            Item::Assistant(text) => Some(format!("assistant:{text}")),
            Item::Thinking(text) => Some(format!("thinking:{text}")),
            Item::ToolCall { tool, .. } => Some(format!("call:{tool}")),
            _ => None,
        })
        .collect()
}

fn notices(model: &Model) -> Vec<String> {
    model
        .items
        .iter()
        .filter_map(|entry| match &entry.item {
            Item::Notice(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_childs_stream_stays_out_of_the_conversation_until_it_is_opened() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    m.push_item(Item::User("the real question".into()));
    m.push_agent_step(worker.clone(), AgentStep::Text("child chatter".into()));

    assert_eq!(
        shown(&m),
        vec!["user:the real question"],
        "three children streaming into one conversation is unreadable"
    );

    m.focus_pane(Some(worker));
    assert_eq!(shown(&m), vec!["assistant:child chatter"]);
}

#[test]
fn returning_from_a_pane_restores_the_conversation_intact() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    m.push_item(Item::User("keep me".into()));
    m.push_agent_step(worker.clone(), AgentStep::Text("theirs".into()));

    m.focus_pane(Some(worker.clone()));
    m.focus_pane(None);
    assert_eq!(shown(&m), vec!["user:keep me"]);

    // And the child's pane survived the round trip.
    m.focus_pane(Some(worker));
    assert_eq!(shown(&m), vec!["assistant:theirs"]);
}

#[test]
fn steps_arriving_while_a_pane_is_open_land_in_it() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    m.focus_pane(Some(worker.clone()));
    m.push_agent_step(worker.clone(), AgentStep::Reasoning("mine".into()));
    m.push_agent_step(worker.clone(), AgentStep::Text("answer".into()));
    assert_eq!(shown(&m), vec!["thinking:mine", "assistant:answer"]);

    // Parking and reopening must not lose what arrived while it was open.
    m.focus_pane(None);
    m.focus_pane(Some(worker));
    assert_eq!(shown(&m), vec!["thinking:mine", "assistant:answer"]);
}

#[test]
fn a_child_retry_rewinds_the_partial_attempt_before_streaming_again() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    m.push_agent_step(worker.clone(), AgentStep::Text("partial".into()));
    m.push_agent_step(
        worker.clone(),
        AgentStep::SteerQueued("keep the queue".into()),
    );
    m.push_agent_step(worker.clone(), AgentStep::Restarted);
    m.push_agent_step(worker.clone(), AgentStep::Text("complete".into()));
    m.focus_pane(Some(worker));

    assert_eq!(shown(&m), vec!["assistant:complete"]);
    assert_eq!(
        notices(&m),
        vec![
            "↳ queued for this agent: keep the queue",
            "the model's connection dropped — retrying"
        ]
    );
}

#[test]
fn an_unapplied_child_steer_returns_to_the_global_composer() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    m.input = "new draft".into();
    m.cursor = m.input.len();
    m.push_agent_step(
        worker.clone(),
        AgentStep::SteerQueued("do this instead".into()),
    );
    m.push_agent_step(
        worker.clone(),
        AgentStep::SteersReturned(vec!["do this instead".into()]),
    );

    assert_eq!(m.input, "new draft\ndo this instead");
    assert_eq!(m.cursor, m.input.len());
    m.focus_pane(Some(worker));
    assert_eq!(
        notices(&m),
        vec!["queued message returned to the input box — not sent"]
    );
}

#[test]
fn a_child_steer_keeps_the_session_boundary_owned_until_it_settles() {
    let (mut m, _workspace) = model();
    m.pending_agent_steers = 1;
    assert!(m.has_active_agents());

    m.push_agent_step(
        path("worker"),
        AgentStep::SteersReturned(vec!["preserve this message".into()]),
    );

    assert!(!m.has_active_agents());
    assert_eq!(m.input, "preserve this message");
}

#[test]
fn an_applied_child_steer_becomes_a_user_turn_not_assistant_text() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    m.push_agent_step(
        worker.clone(),
        AgentStep::SteerQueued("look at the tests".into()),
    );
    m.push_agent_step(
        worker.clone(),
        AgentStep::Steered("look at the tests".into()),
    );
    m.focus_pane(Some(worker));

    assert_eq!(shown(&m), vec!["user:look at the tests"]);
    assert!(notices(&m).is_empty());
}

#[test]
fn streamed_deltas_coalesce_into_one_block() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    for delta in ["a", "b", "c"] {
        m.push_agent_step(worker.clone(), AgentStep::Text(delta.into()));
    }
    m.focus_pane(Some(worker));
    assert_eq!(
        shown(&m),
        vec!["assistant:abc"],
        "a reply is a block, not a line per token"
    );
}

#[test]
fn a_pane_opens_with_the_task_the_child_was_given() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    m.push_agent_step(
        worker.clone(),
        AgentStep::Task {
            objective: "audit the backend".into(),
            contract: Some("file:line list".into()),
        },
    );
    m.push_agent_step(worker.clone(), AgentStep::Text("working".into()));
    m.focus_pane(Some(worker));
    let rendered = shown(&m);
    assert!(
        rendered[0].starts_with("user:audit the backend"),
        "{rendered:?}"
    );
    assert!(rendered[0].contains("file:line list"), "{rendered:?}");
}

#[test]
fn a_childs_pane_is_bounded_however_much_it_says() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    for n in 0..(MAX_AGENT_PANE_ITEMS + 50) {
        m.push_agent_step(
            worker.clone(),
            AgentStep::ToolCall {
                id: None,
                tool: format!("tool-{n}"),
                args: serde_json::json!({}),
            },
        );
    }
    m.focus_pane(Some(worker));
    assert_eq!(m.items.len(), MAX_AGENT_PANE_ITEMS);
    // The oldest go first, so the view keeps what it is doing now.
    assert!(
        shown(&m)
            .last()
            .unwrap()
            .ends_with(&format!("tool-{}", MAX_AGENT_PANE_ITEMS + 49))
    );
}

#[test]
fn the_switcher_keeps_the_open_pane_addressable_after_it_settles() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    m.focus_pane(Some(worker.clone()));
    // `agent_runs` is empty: the child has settled and left the roster.
    let rows = m.switch_rows();
    assert!(
        rows.contains(&Some(worker)),
        "a reader must never be stranded in a pane the switcher denies exists"
    );
    assert!(rows.contains(&None), "main is always a destination");
}

#[test]
fn a_settled_pane_can_be_reopened_after_returning_to_main() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    m.push_agent_step(worker.clone(), AgentStep::Text("finished answer".into()));
    assert!(m.agent_runs.is_empty());

    m.focus_pane(Some(worker.clone()));
    m.focus_pane(None);
    assert!(
        m.switch_rows().contains(&Some(worker.clone())),
        "parking a settled pane must not make its retained content unreachable"
    );
    m.focus_pane(Some(worker));
    assert_eq!(shown(&m), vec!["assistant:finished answer"]);
}

#[test]
fn panes_evicted_from_the_bounded_agent_registry_do_not_accumulate() {
    let (mut m, _workspace) = model();
    let old = path("old");
    let retained = path("retained");
    m.push_agent_step(old.clone(), AgentStep::Text("old answer".into()));
    m.push_agent_step(retained.clone(), AgentStep::Text("retained answer".into()));
    m.parked_scroll.insert(Some(old.clone()), (7, false));

    m.retain_known_agent_panes(&std::collections::HashSet::from([retained.clone()]));

    assert!(!m.agent_panes.contains_key(&old));
    assert!(!m.parked_scroll.contains_key(&Some(old)));
    assert!(m.agent_panes.contains_key(&retained));
}

#[test]
fn pane_scroll_offset_and_follow_mode_survive_round_trips() {
    let (mut m, _workspace) = model();
    let worker = path("worker");
    m.scroll_offset = 41;
    m.auto_scroll = false;
    m.focus_pane(Some(worker.clone()));
    m.scroll_offset = 7;
    m.auto_scroll = true;

    m.focus_pane(None);
    assert_eq!((m.scroll_offset, m.auto_scroll), (41, false));
    m.focus_pane(Some(worker));
    assert_eq!((m.scroll_offset, m.auto_scroll), (7, true));
}

#[test]
fn switcher_selection_follows_the_selected_path_when_rows_change() {
    let (mut m, _workspace) = model();
    let a = path("a");
    let b = path("b");
    m.push_agent_step(b.clone(), AgentStep::Text("b".into()));
    m.switching = true;
    m.switch_cursor = m
        .switch_rows()
        .iter()
        .position(|row| row == &Some(b.clone()))
        .unwrap();
    m.push_agent_step(a, AgentStep::Text("a".into()));
    assert_eq!(m.switch_rows()[m.switch_cursor], Some(b));
}
