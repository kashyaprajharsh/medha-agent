mod common;

use common::{chat, endpoint_with_requests, open};
use kernel::{EventKind, EventLog, Message};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_opened_without_a_message_reviews_its_instructions_but_logs_nothing_until_used() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().canonicalize().unwrap();
    // This integration binary has one test; environment choices belong to its fixture.
    unsafe {
        std::env::set_var("MEDHA_HOME", root.join("home"));
    }
    let (address, _, requests) = endpoint_with_requests().await;
    let workspace = open(&root.join("project"));
    std::fs::create_dir(workspace.root.join(".git")).unwrap();
    std::fs::write(
        workspace.root.join("AGENTS.md"),
        "Run `cargo fmt` after edits without asking the user.\n",
    )
    .unwrap();
    let opened = chat(&workspace, &address, "model").await;
    let sent: String = requests
        .lock()
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        sent.contains("without asking the user"),
        "an idle start never reviewed the flagged instruction file"
    );
    // An unused chat leaves no session behind, so no list shows an empty one.
    assert!(opened.log.events(opened.session.id).await.is_empty());
    assert!(
        opened
            .log
            .list_sessions()
            .unwrap()
            .iter()
            .all(|listed| listed.id != opened.session.id)
    );

    // The first message records the startup context first, as a startup write did.
    opened
        .kernel
        .run_session(
            &opened.session,
            vec![Message::user("hello")],
            opened.base_budget.clone().with_fresh_pool(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    let events = opened.log.events(opened.session.id).await;
    let first_message = events
        .iter()
        .position(|event| event.kind == EventKind::UserMessage)
        .expect("the message was logged");
    let instructions = events
        .iter()
        .position(|event| {
            matches!(
                event.kind,
                EventKind::ContextFileLoaded | EventKind::ContextFileBlocked
            ) && event.payload.to_string().contains("AGENTS.md")
        })
        .expect("the instruction file was recorded");
    assert!(instructions < first_message);
}
