use super::*;

#[test]
fn a_child_step_and_its_usage_reach_the_terminal_as_events() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let watch = AgentWatch { tx };
    let path = orchestrator::AgentPath::root().child("worker").unwrap();
    let session = Some(ulid::Ulid::new());

    watch.step(session, &path, AgentStep::Restarted);
    watch.usage(&kernel::Usage {
        prompt_tokens: 10_000,
        ..Default::default()
    });

    assert!(matches!(
        rx.try_recv(),
        Ok(TuiEvent::AgentStep { surface_session, path: seen, step: AgentStep::Restarted })
            if surface_session == session && seen == path
    ));
    assert!(matches!(
        rx.try_recv(),
        Ok(TuiEvent::Usage(usage)) if usage.prompt_tokens == 10_000
    ));
}
