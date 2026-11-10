//! What a finished task used and cost, told when it ends.
use super::*;
use kernel::EventLog;

async fn usage_logged(world: &World, chat: &str) -> Vec<kernel::Event> {
    let state = std::fs::read_dir(world.home().join("projects"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join("events.db").is_file())
        .unwrap();
    let log = store::SqliteLog::open(state.join("events.db")).unwrap();
    log.events(chat.parse().unwrap())
        .await
        .into_iter()
        .filter(|event| event.kind == kernel::EventKind::ContextUsage)
        .collect()
}

/// A finished task says how long it took, its model calls and tokens, and what they cost at the operator's rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_task_says_what_it_used_and_what_it_cost() {
    let world = World::new();
    let folder = world.folder("summary");
    let rate = "[pricing]\ninput_per_mtok = 2.0\noutput_per_mtok = 10.0\n";
    std::fs::write(folder.join("medha.lock"), rate).unwrap();
    std::fs::create_dir_all(folder.join(".git")).unwrap();
    let backend = world.backend();
    let mut client = backend.connect().await;
    // A project's own rate counts only once the person trusts the project, as the app asks.
    let read = json!({"method": "application.resources", "params": {"resource": "project_trust"}});
    let review = client.about(&folder, read).await.unwrap();
    let review = &review["data"];
    let trust = json!({"method": "application.resources.change", "params": {"action": "trust_project",
        "data": {"root": review["root"], "folder": review["folder"], "fingerprint": review["fingerprint"]}}});
    client.about(&folder, trust).await.unwrap();
    let chat = client.open(&folder).await;
    client.send(&chat, "USAGE please").await;
    let (_, done) = client.until(|frame| kind(frame, "turn.done")).await;
    let summary = &done["params"]["summary"];
    assert_eq!(summary["calls"], 1, "{done}");
    assert_eq!(summary["input_tokens"], 1200, "{done}");
    assert_eq!(summary["cached_tokens"], 1000, "{done}");
    assert_eq!(summary["output_tokens"], 34, "{done}");
    assert_eq!(summary["priced"], "own", "{done}");
    assert!(
        summary["seconds"]
            .as_f64()
            .is_some_and(|seconds| seconds >= 0.0),
        "{done}"
    );
    let logged: f64 = usage_logged(&world, &chat)
        .await
        .iter()
        .filter_map(|event| event.payload["cost_usd"].as_f64())
        .sum();
    let told = summary["cost_usd"]
        .as_f64()
        .expect("a priced task tells its cost");
    assert!(
        logged > 0.0 && (told - logged).abs() < 1e-12,
        "told {told}, logged {logged}"
    );
}
