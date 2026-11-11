//! A chat started with nobody to answer is never left waiting on a person.
use super::*;

/// With nobody to answer, an action that needs approval is denied at once, nothing runs, and the model is told why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_with_nobody_to_answer_denies_at_once_and_says_why() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("unattended");
    std::fs::write(folder.join("a.txt"), "a0\n").unwrap();
    let made = client
        .ask(
            "session.create",
            None,
            json!({"folder": folder, "answer": "nobody"}),
        )
        .await;
    let chat = made["result"]["session"]
        .as_str()
        .unwrap_or_else(|| panic!("no chat was created: {made}"))
        .to_string();
    client
        .ask("session.attach", Some(&chat), json!({"after": 0}))
        .await;
    let started = Instant::now();
    let write = json!({"path": "a.txt", "content": "a1\n"});
    let read = json!({"path": "a.txt"});
    let steps = [format!("read {read}"), format!("write {write}")];
    let sent = scripted(&mut client, &world, &chat, &steps).await;
    assert_eq!(
        std::fs::read_to_string(folder.join("a.txt")).unwrap(),
        "a0\n",
        "the write ran, so someone was asked: {sent}"
    );
    assert!(
        sent.contains("no one can answer"),
        "the model was not told why: {sent}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the denial waited"
    );

    // A question is not left waiting an hour either; the model is told no one can answer.
    let question = json!({"questions": [{"question": "Which file?", "options": [{"label": "a"}, {"label": "b"}]}]});
    let steps = [format!("clarify {question}")];
    let asked = scripted(&mut client, &world, &chat, &steps);
    let sent = tokio::time::timeout(Duration::from_secs(60), asked)
        .await
        .expect("the question waited for an answer");
    assert!(
        sent.contains("no interactive user is available"),
        "the model was not told no one can answer: {sent}"
    );
}
