use super::*;
use std::sync::Barrier;
use ulid::Ulid;

fn registry() -> Arc<AgentRegistry> {
    Arc::new(AgentRegistry::new())
}

thread_local! {
    /// The loop side of every queue these tests create, held for the duration.
    /// `steer` reports whether anything is listening, so dropping the receiver
    /// would make it fail for a reason none of these tests is about.
    static QUEUES: std::cell::RefCell<Vec<kernel::InterruptQueue>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn live() -> Live {
    let (steer, queue) = kernel::InterruptQueue::pair();
    QUEUES.with(|queues| queues.borrow_mut().push(queue));
    Live {
        cancel: CancellationToken::new(),
        steer,
        budget: kernel::Budget::turns(5),
    }
}

fn agent(path: &AgentPath, started_ms: u64) -> Agent {
    Agent {
        path: path.clone(),
        session: Ulid::new().to_string(),
        objective: "work".into(),
        started_ms,
        state: State::Running,
        write: false,
        tools: None,
    }
}

fn start(registry: &Arc<AgentRegistry>, name: &str, started_ms: u64) -> AgentPath {
    let (path, reservation) = registry.claim(&AgentPath::root(), name).unwrap();
    reservation.commit(agent(&path, started_ms), live());
    path
}

#[test]
fn a_claim_finds_a_free_name_beside_a_taken_one() {
    let registry = registry();
    start(&registry, "survey", 1);
    let (next, _held) = registry.claim(&AgentPath::root(), "survey").unwrap();
    assert_eq!(next.as_str(), "/survey-2");
}

#[test]
fn an_outstanding_claim_is_not_handed_out_twice() {
    let registry = registry();
    let (first, _held) = registry.claim(&AgentPath::root(), "survey").unwrap();
    // Finding a free name and taking it happen under one lock; two spawns that
    // raced on the same name would otherwise both be told it was free.
    let (second, _also_held) = registry.claim(&AgentPath::root(), "survey").unwrap();
    assert_ne!(first, second);
}

#[test]
fn an_abandoned_claim_frees_the_name() {
    let registry = registry();
    let (path, reservation) = registry.claim(&AgentPath::root(), "survey").unwrap();
    drop(reservation);
    // A spawn that fails after claiming must not burn the name.
    let (again, _held) = registry.claim(&AgentPath::root(), "survey").unwrap();
    assert_eq!(again, path);
}

#[test]
fn a_claimed_name_never_shows_up_as_an_agent() {
    let registry = registry();
    let (_path, _held) = registry.claim(&AgentPath::root(), "survey").unwrap();
    // A half-built entry in the listing is one every reader has to know to
    // skip; keeping claims apart makes that unrepresentable.
    assert!(registry.all().is_empty());
    assert!(registry.find(&AgentPath::root(), "survey").is_none());
}

#[test]
fn a_settled_agent_leaves_running_but_stays_addressable() {
    let registry = registry();
    let path = start(&registry, "survey", 1);
    registry.settled(&path, AgentStatus::Completed);

    assert!(registry.running().is_empty());
    let found = registry.find(&AgentPath::root(), "survey").unwrap();
    assert_eq!(found.state, State::Settled(AgentStatus::Completed));
}

#[test]
fn a_settled_agent_no_longer_accepts_control() {
    let registry = registry();
    let path = start(&registry, "survey", 1);
    assert!(registry.steer(&path, "narrow it"));
    registry.settled(&path, AgentStatus::Completed);
    // Reporting success for a message nobody receives would let a caller
    // believe it had corrected a run.
    assert!(!registry.steer(&path, "too late"));
    assert!(!registry.cancel(&path));
}

#[test]
fn followup_and_settlement_choose_one_atomic_path() {
    let registry = registry();
    let path = start(&registry, "survey", 1);
    let barrier = Arc::new(Barrier::new(3));

    let followup_registry = Arc::clone(&registry);
    let followup_path = path.clone();
    let followup_barrier = Arc::clone(&barrier);
    let followup = std::thread::spawn(move || {
        followup_barrier.wait();
        followup_registry.followup(&followup_path, "check the lexer")
    });

    let settle_registry = Arc::clone(&registry);
    let settle_path = path.clone();
    let settle_barrier = Arc::clone(&barrier);
    let settle = std::thread::spawn(move || {
        settle_barrier.wait();
        settle_registry.settled(&settle_path, AgentStatus::Completed);
    });

    barrier.wait();
    let choice = followup.join().unwrap();
    settle.join().unwrap();
    let queued = QUEUES.with(|queues| {
        queues
            .borrow_mut()
            .last_mut()
            .unwrap()
            .drain_steers()
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
    });

    match choice {
        Some(Followup::Delivered(agent)) => {
            assert_eq!(agent.path, path);
            assert_eq!(queued, ["check the lexer"]);
        }
        Some(Followup::Resume(agent)) => {
            assert_eq!(agent.path, path);
            assert!(queued.is_empty());
        }
        None => panic!("the settle race silently lost the follow-up"),
    }
}

#[test]
fn agents_are_found_by_path_or_session_never_by_bare_name() {
    let registry = registry();
    let path = start(&registry, "survey", 1);
    let session = registry
        .find(&AgentPath::root(), path.as_str())
        .unwrap()
        .session;

    let root = AgentPath::root();
    assert!(registry.find(&root, "/survey").is_some());
    assert!(registry.find(&root, "survey").is_some());
    assert!(registry.find(&root, &session).is_some());
    assert!(registry.find(&root, "nope").is_none());

    // A name resolves only under the caller's own node. Two agents in different
    // subtrees can share one, so a bare-name search across the whole tree would
    // be a hash-order coin toss between their owners.
    let (nested, reservation) = registry
        .claim(&AgentPath::parse("/survey").unwrap(), "parse")
        .unwrap();
    reservation.commit(agent(&nested, 2), live());
    assert!(registry.find(&root, "parse").is_none());
    assert!(
        registry
            .find(&AgentPath::parse("/survey").unwrap(), "parse")
            .is_some()
    );
}

#[test]
fn listings_are_ordered_by_start_so_the_view_is_stable() {
    let registry = registry();
    start(&registry, "third", 3);
    start(&registry, "first", 1);
    start(&registry, "second", 2);
    let names: Vec<String> = registry
        .all()
        .iter()
        .map(|agent| agent.path.name().to_string())
        .collect();
    assert_eq!(names, ["first", "second", "third"]);
}

#[test]
fn settled_history_is_bounded() {
    let registry = Arc::new(AgentRegistry {
        tree: Mutex::new(Tree::default()),
        max_settled: 2,
    });
    for n in 0..4 {
        let path = start(&registry, &format!("agent-{n}"), n as u64);
        registry.settled(&path, AgentStatus::Completed);
    }
    assert_eq!(registry.all().len(), 2);
    assert!(registry.find(&AgentPath::root(), "agent-0").is_none());
    assert!(registry.find(&AgentPath::root(), "agent-3").is_some());
}

/// Roster eviction must not orphan a durable transcript: a nested parent
/// still resolves its own child's session id through the archive — by name
/// and by the documented stable id — while a sibling can do neither.
#[test]
fn evicted_agents_stay_resolvable_by_their_parent_through_the_archive() {
    let registry = Arc::new(AgentRegistry {
        tree: Mutex::new(Tree::default()),
        max_settled: 2,
    });
    let (parent, reservation) = registry.claim(&AgentPath::root(), "parent").unwrap();
    reservation.commit(agent(&parent, 0), live());
    let (sibling, reservation) = registry.claim(&AgentPath::root(), "sibling").unwrap();
    reservation.commit(agent(&sibling, 0), live());
    let (child, reservation) = registry.claim(&parent, "child").unwrap();
    let child_agent = agent(&child, 1);
    let child_session = child_agent.session.clone();
    reservation.commit(child_agent, live());

    registry.settled(&child, AgentStatus::Completed);
    for n in 0..3 {
        let path = start(&registry, &format!("late-{n}"), 10 + n);
        registry.settled(&path, AgentStatus::Completed);
    }
    assert!(
        registry.find(&parent, "child").is_none(),
        "the roster entry must be evicted for this test to mean anything"
    );

    assert_eq!(
        registry.archived_session(&parent, "child").as_deref(),
        Some(child_session.as_str()),
        "a nested parent must still resolve its child by name"
    );
    assert_eq!(
        registry
            .archived_session(&parent, &child_session)
            .as_deref(),
        Some(child_session.as_str()),
        "the documented stable id must answer under the same containment"
    );
    assert!(registry.archived_session(&sibling, "child").is_none());
    assert!(
        registry
            .archived_session(&sibling, &child_session)
            .is_none(),
        "learning a session id must not open a sibling's transcript"
    );
}

/// A follow-up revives a settled agent before admission can fail. Without a
/// rollback the agent it was asked to continue is simply gone — no roster
/// entry, nothing to retry against, and no record that it ever ran.
#[test]
fn a_revive_that_is_never_committed_puts_the_agent_back() {
    let registry = registry();
    let (path, reservation) = registry.claim(&AgentPath::root(), "writer").unwrap();
    reservation.commit(agent(&path, 0), live());
    registry.settled(&path, AgentStatus::Completed);
    assert_eq!(registry.all().len(), 1);

    // Admission fails after the revive — at capacity, no isolation, anything.
    drop(registry.revive(&path).unwrap());

    let found = registry.all();
    assert_eq!(found.len(), 1, "the revived agent was lost");
    assert_eq!(found[0].path, path);
    assert!(!found[0].is_running(), "it should be settled again");
    // And it is addressable, so the follow-up can be retried.
    assert!(registry.find(&AgentPath::root(), "writer").is_some());
}
