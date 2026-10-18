mod common;

use common::{chat, endpoint_with_requests, open};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_opened_without_a_message_still_reviews_a_flagged_instruction_file() {
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
    let _chat = chat(&workspace, &address, "model").await;
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
}
