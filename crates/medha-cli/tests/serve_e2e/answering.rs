//! A chat started with nobody to answer is never left waiting on a person.
use super::*;
use kernel::EventLog;

/// What the chat logged about each approval answer, from whichever project folder holds it.
async fn answers_logged(world: &World, chat: &str) -> Vec<Value> {
    let mut answers = Vec::new();
    for state in std::fs::read_dir(world.home().join("projects")).unwrap() {
        let db = state.unwrap().path().join("events.db");
        if !db.is_file() {
            continue;
        }
        let log = store::SqliteLog::open(&db).unwrap();
        for event in log.events(chat.parse().unwrap()).await {
            if event.kind.as_str() == "approval.decision" {
                answers.push(event.payload);
            }
        }
    }
    answers
}

/// Each approval answer is logged once, naming who gave it and why: nobody in an unattended run, a person at a screen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_approval_answer_is_logged_with_who_gave_it() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let steps = [
        format!("read {}", json!({"path": "a.txt"})),
        format!("write {}", json!({"path": "a.txt", "content": "a1\n"})),
    ];
    let (_, unanswered) = unattended(&world, &mut client, "logged-nobody").await;
    // Where an OS sandbox asks for the network itself, that answer is recorded the same way.
    let fetch = json!({"command": "curl -s https://example.com", "network": true});
    let mut unattended_steps = steps.to_vec();
    let mut expected = vec![json!("tool")];
    if cfg!(unix) {
        unattended_steps.push(format!("shell_exec {fetch}"));
        expected.push(json!("access"));
    }
    scripted(&mut client, &world, &unanswered, &unattended_steps).await;
    let nobody = answers_logged(&world, &unanswered).await;
    let kinds: Vec<Value> = nobody.iter().map(|answer| answer["kind"].clone()).collect();
    assert_eq!(kinds, expected, "{nobody:?}");
    for answer in &nobody {
        assert_eq!(answer["by"], "nobody", "{answer}");
        assert_eq!(answer["outcome"], "denied", "{answer}");
        assert!(
            answer["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("no one can answer"))
        );
    }

    let attended = world.folder("logged-person");
    std::fs::write(attended.join("a.txt"), "a0\n").unwrap();
    let chat = client.open(&attended).await;
    scripted(&mut client, &world, &chat, &steps).await;
    let person = answers_logged(&world, &chat).await;
    assert_eq!(person.len(), 1, "{person:?}");
    assert_eq!(person[0]["by"], "person", "{person:?}");
    assert_eq!(person[0]["outcome"], "allowed", "{person:?}");
}

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
