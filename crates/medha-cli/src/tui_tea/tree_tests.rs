use super::backend_features::branched;
use super::*;

fn agent(path: &str) -> orchestrator::Agent {
    orchestrator::Agent {
        path: orchestrator::AgentPath::parse(path).unwrap(),
        session: ulid::Ulid::new().to_string(),
        objective: "work".into(),
        started_ms: 0,
        state: orchestrator::State::Running,
        write: false,
        tools: None,
    }
}

fn drawn(paths: &[&str]) -> Vec<String> {
    let idle = std::collections::HashMap::new();
    branched(paths.iter().map(|path| agent(path)).collect(), &idle)
        .into_iter()
        .map(|row| match row {
            AgentRow::Agent { agent, branch, .. } => {
                format!("{branch}{}", agent.path.name())
            }
            AgentRow::Patch { agent, .. } => agent,
        })
        .collect()
}

#[test]
fn a_child_is_drawn_under_the_agent_that_started_it() {
    // Paths, rather than spawn time, define ownership order.
    assert_eq!(
        drawn(&["/writer", "/survey/parse", "/survey", "/survey/lex"]),
        ["survey", "├ lex", "└ parse", "writer"]
    );
}

#[test]
fn the_last_child_closes_its_branch() {
    assert_eq!(
        drawn(&["/survey", "/survey/only"]),
        ["survey", "└ only"],
        "an only child is also a last child"
    );
}

#[test]
fn a_grandchild_is_not_mistaken_for_a_sibling() {
    // Branch drawing compares depth so descendants remain attached.
    assert_eq!(
        drawn(&[
            "/survey",
            "/survey/parse",
            "/survey/parse/ast",
            "/survey/lex"
        ]),
        ["survey", "├ lex", "└ parse", "  └ ast"]
    );
}

#[test]
fn a_flat_tree_is_drawn_flat() {
    assert_eq!(drawn(&["/one", "/two"]), ["one", "two"]);
}
