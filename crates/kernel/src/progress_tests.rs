use super::*;

#[test]
fn a_phase_change_restarts_the_stall_clock() {
    let (handle, watch) = ProgressHandle::new();
    handle.enter(Phase::Generating);
    std::thread::sleep(Duration::from_millis(20));
    let before = watch.borrow().in_phase();
    assert!(before >= Duration::from_millis(20));

    handle.enter(Phase::InTool {
        tool: "fs.read".into(),
        target: Some("README.md".into()),
    });
    assert!(
        watch.borrow().in_phase() < before,
        "entering a new phase starts its clock afresh"
    );
}

#[test]
fn repeating_a_phase_does_not_hide_a_stream_going_quiet() {
    let (handle, watch) = ProgressHandle::new();
    handle.enter(Phase::Generating);
    std::thread::sleep(Duration::from_millis(20));
    handle.enter(Phase::Generating);
    assert!(
        watch.borrow().in_phase() >= Duration::from_millis(20),
        "a repeated phase must not reset the clock, or a stalled stream looks fresh"
    );
}

#[test]
fn a_tick_keeps_a_slow_stream_apart_from_a_dead_one() {
    let (handle, watch) = ProgressHandle::new();
    handle.enter(Phase::Generating);
    std::thread::sleep(Duration::from_millis(20));
    handle.ticked();
    assert!(watch.borrow().in_phase() < Duration::from_millis(20));
    assert_eq!(watch.borrow().phase, Phase::Generating, "still generating");
}

#[test]
fn waiting_on_a_human_is_never_evidence_of_a_stall() {
    let (handle, watch) = ProgressHandle::new();
    handle.enter(Phase::AwaitingApproval {
        action: "shell `npm ls`".into(),
    });
    assert_eq!(
        watch.borrow().stalled_for(),
        None,
        "an operator who stepped away has not wedged the agent"
    );
    assert!(!watch.borrow().phase.is_stall_evidence());
}

#[test]
fn a_settled_agent_is_not_stalled_either() {
    let (handle, watch) = ProgressHandle::new();
    handle.settled();
    assert_eq!(watch.borrow().stalled_for(), None);
}

#[test]
fn work_phases_are_stall_evidence() {
    let (handle, watch) = ProgressHandle::new();
    for phase in [
        Phase::Idle,
        Phase::Generating,
        Phase::InTool {
            tool: "shell".into(),
            target: None,
        },
    ] {
        handle.enter(phase.clone());
        assert!(
            watch.borrow().stalled_for().is_some(),
            "{phase:?} should be measurable as a stall"
        );
    }
}

#[test]
fn counters_accumulate_and_a_turn_is_one_generate() {
    let (handle, watch) = ProgressHandle::new();
    handle.enter(Phase::Generating);
    handle.tool_dispatched();
    handle.tool_dispatched();
    handle.metered(1_200);
    handle.enter(Phase::InTool {
        tool: "fs.read".into(),
        target: None,
    });
    handle.enter(Phase::Generating);
    handle.metered(800);

    let progress = watch.borrow().clone();
    assert_eq!(progress.turns, 2, "two generate phases, two turns");
    assert_eq!(progress.tool_calls, 2);
    assert_eq!(
        progress.tokens, 2_000,
        "providers report per-turn usage, so tokens accumulate"
    );
}

#[test]
fn spend_is_replaced_not_accumulated() {
    let (handle, watch) = ProgressHandle::new();
    // The governor reports the session total each turn. Adding those would
    // square the bill.
    handle.priced(0.004);
    handle.priced(0.006);
    assert!((watch.borrow().cost_usd - 0.006).abs() < f64::EPSILON);
}

#[test]
fn a_watcher_that_falls_behind_sees_the_latest_not_a_backlog() {
    let (handle, watch) = ProgressHandle::new();
    for step in 0..50u32 {
        handle.enter(Phase::InTool {
            tool: format!("tool-{step}"),
            target: None,
        });
    }
    assert_eq!(
        watch.borrow().phase,
        Phase::InTool {
            tool: "tool-49".into(),
            target: None
        },
        "liveness is lossy by design: the current state, never a queue of old ones"
    );
}

#[test]
fn the_label_names_what_it_is_doing_not_that_it_is_doing() {
    assert_eq!(
        Phase::InTool {
            tool: "fs.read".into(),
            target: Some("app.py".into())
        }
        .label(),
        "fs.read app.py"
    );
    assert_eq!(
        Phase::InTool {
            tool: "shell".into(),
            target: None
        }
        .label(),
        "shell"
    );
    assert_eq!(
        Phase::AwaitingApproval { action: "x".into() }.label(),
        "waiting on you"
    );
}
