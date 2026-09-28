use super::*;
use orchestrator::AgentStatus;

fn agent(name: &str, started_ms: u64, state: State) -> Agent {
    Agent {
        path: AgentPath::root().child(name).unwrap(),
        session: format!("session-{name}"),
        objective: format!("Objective of {name}"),
        started_ms,
        state,
        write: false,
        tools: None,
    }
}

fn progress(phase: kernel::Phase, tool_calls: u32, tokens: u64) -> kernel::Progress {
    kernel::Progress {
        phase,
        tool_calls,
        tokens,
        ..kernel::Progress::default()
    }
}

#[test]
fn each_agent_says_what_it_is_doing_in_the_step_rows_words() {
    let reading = agent("read-medha-doc", 2_000, State::Running);
    let progress = HashMap::from([(
        reading.path.clone(),
        progress(
            kernel::Phase::InTool {
                tool: "read".into(),
                target: Some("docs/WHAT_IS_MEDHA.md".into()),
            },
            12,
            34_000,
        ),
    )]);
    let roster = roster(&[reading], &progress, 1_000);
    let row = &roster["agents"][0];
    assert_eq!(row["name"], "read-medha-doc");
    assert_eq!(row["session"], "session-read-medha-doc");
    assert_eq!(row["status"], "running");
    assert_eq!(
        row["doing"],
        json!({"state": "tool", "verb": "Read", "target": "docs/WHAT_IS_MEDHA.md"})
    );
    assert_eq!(
        (row["tool_calls"].as_u64(), row["tokens"].as_u64()),
        (Some(12), Some(34_000))
    );
}

#[test]
fn an_agent_waiting_on_a_person_says_for_what() {
    let waiting = agent("writer", 2_000, State::Running);
    let progress = HashMap::from([(
        waiting.path.clone(),
        progress(
            kernel::Phase::AwaitingApproval {
                action: "edit src/main.rs".into(),
            },
            1,
            10,
        ),
    )]);
    assert_eq!(
        roster(&[waiting], &progress, 0)["agents"][0]["doing"],
        json!({"state": "waiting", "action": "edit src/main.rs"})
    );
}

#[test]
fn finished_agents_stay_with_their_outcome_but_older_ones_are_left_out() {
    let agents = [
        agent("later", 3_000, State::Settled(AgentStatus::Exhausted)),
        agent("earlier", 2_000, State::Running),
        agent(
            "before-this-window",
            500,
            State::Settled(AgentStatus::Completed),
        ),
    ];
    let roster = roster(&agents, &HashMap::new(), 1_000);
    let rows = roster["agents"].as_array().unwrap();
    let names: Vec<_> = rows
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["earlier", "later"]);
    assert_eq!(rows[1]["status"], "exhausted");
    assert_eq!(rows[0]["doing"], Value::Null);
}
