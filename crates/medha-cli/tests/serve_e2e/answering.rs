//! A chat started with nobody to answer is never left waiting on a person.
use super::*;

/// A chat with nobody to answer, opened on a folder holding one file the chat has read.
async fn unattended(world: &World, client: &mut Client, name: &str) -> (PathBuf, String) {
    let folder = world.folder(name);
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
    (folder, chat)
}

/// A run refused three times in a row stops there, says why, and does not ask the model again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_refused_three_times_in_a_row_stops_and_says_why() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let (_, chat) = unattended(&world, &mut client, "stuck").await;
    let mut steps = vec![format!("read {}", json!({"path": "a.txt"}))];
    let write = |n: usize| json!({"path": "a.txt", "content": format!("a{n}\n")});
    steps.extend((1..=5).map(|n| format!("write {}", write(n))));
    client
        .send(&chat, &format!("SCRIPT\n{}", steps.join("\n")))
        .await;
    let (_, done) = client.until(|frame| kind(frame, "turn.done")).await;
    let asked = std::iter::from_fn(|| world.provider.seen.try_recv().ok()).count();
    assert!(
        done.to_string().contains("refused_repeatedly"),
        "the run did not say it stopped for repeated refusals: {done}"
    );
    assert_eq!(
        asked, 4,
        "the model was asked again after the third refusal in a row"
    );
}

/// With nobody to answer, an action that needs approval is denied at once, nothing runs, and the model is told why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_with_nobody_to_answer_denies_at_once_and_says_why() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let (folder, chat) = unattended(&world, &mut client, "unattended").await;
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
