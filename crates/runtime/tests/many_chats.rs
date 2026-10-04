//! One process, two folders, two chats in each: what a single backend has to be able to do.

mod common;

use common::{chat, endpoint, open, say};
use runtime::session::Started;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_folders_with_two_chats_each_run_in_one_process_without_crossing() {
    let root = tempfile::tempdir().unwrap();
    let (address, asked) = endpoint().await;
    // SAFETY: this file holds one test, so nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("MEDHA_HOME", root.path().join("home")) };
    let home_a = open(&root.path().join("a"));
    let home_b = open(&root.path().join("b"));

    let a1 = chat(&home_a, &address, "model-a1").await;
    let a2 = chat(&home_a, &address, "model-a2").await;
    let b1 = chat(&home_b, &address, "model-b1").await;
    let b2 = chat(&home_b, &address, "model-b2").await;
    assert!(
        Arc::ptr_eq(&a1.log, &a2.log) && Arc::ptr_eq(&b1.log, &b2.log),
        "chats in one folder share its one log connection"
    );
    assert!(!Arc::ptr_eq(&a1.log, &b1.log));

    let (ra1, ra2, rb1, rb2) = tokio::join!(
        say(&a1, "from a1"),
        say(&a2, "from a2"),
        say(&b1, "from b1"),
        say(&b2, "from b2"),
    );
    assert_eq!(
        [ra1, ra2, rb1, rb2],
        [
            "echo: from a1",
            "echo: from a2",
            "echo: from b1",
            "echo: from b2"
        ]
    );
    let mut asked = asked.lock().unwrap().clone();
    asked.sort();
    let pair = |model: &str, text: &str| (model.to_string(), text.to_string());
    assert_eq!(
        asked,
        [
            pair("model-a1", "from a1"),
            pair("model-a2", "from a2"),
            pair("model-b1", "from b1"),
            pair("model-b2", "from b2"),
        ],
        "each chat asked its own model, once"
    );

    assert_ne!(home_a.state, home_b.state);
    let ids = |chat: &Started| -> Vec<_> {
        let mut ids: Vec<_> = chat
            .log
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|session| session.id)
            .collect();
        ids.sort();
        ids
    };
    let mut in_a = vec![a1.session.id, a2.session.id];
    let mut in_b = vec![b1.session.id, b2.session.id];
    in_a.sort();
    in_b.sort();
    assert_eq!(ids(&a1), in_a, "folder a's log holds only its own chats");
    assert_eq!(ids(&b1), in_b, "folder b's log holds only its own chats");
    assert_ne!(a1.workspace.root(), b1.workspace.root());

    if sandbox::native_backend_available() {
        let mut homes = Vec::new();
        for chat in [&a1, &a2, &b1, &b2] {
            let script = r#"printf %s "$HOME""#.to_string();
            let out = chat
                .workspace
                .exec("/bin/sh", &["-c".into(), script], Vec::new(), false)
                .await
                .unwrap();
            homes.push(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let distinct: std::collections::HashSet<_> = homes.iter().collect();
        assert_eq!(
            distinct.len(),
            4,
            "each chat's commands get their own HOME: {homes:?}"
        );
    }
}
