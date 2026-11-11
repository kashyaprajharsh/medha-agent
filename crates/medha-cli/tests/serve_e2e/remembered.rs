//! What Medha remembers opens a chat as memory, never as a web page, whatever chat saved it.
use super::*;
use kernel::{EventLog, TrustLabel};
use memory::{ConfidenceRung, MemoryEntry, MemoryKind, MemoryOp, MemoryProjection, Scope};

/// A memory updated after a web page opens a chat by name only, and that chat is not web-tainted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_memory_saved_after_a_web_page_does_not_make_a_new_chat_web_tainted() {
    let world = World::new();
    let scratch = world.home().join("seed-project-memory.db");
    let store = MemoryProjection::open(&scratch, world.home().join("memory.db")).unwrap();
    store
        .apply(&MemoryOp::Write {
            entry: MemoryEntry {
                name: "user-name".into(),
                claim: "The person's name is Asha.".into(),
                description: "what to call the person".into(),
                kind: MemoryKind::Preference,
                scope: Scope::User,
                trust: TrustLabel::Web,
                confidence: ConfidenceRung::UserStated,
                provenance: vec![],
                sessions: vec![],
                version: 2,
                pinned: false,
                links: vec![],
                created: 1.0,
                updated: 1.0,
            },
        })
        .unwrap();
    drop(store);

    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("remembered");
    std::fs::write(folder.join("a.txt"), "a\n").unwrap();
    let chat = client.open(&folder).await;
    scripted(
        &mut client,
        &world,
        &chat,
        &[format!("read {}", json!({"path": "a.txt"}))],
    )
    .await;

    let events = logged(&world, &chat).await;
    let recalled = events
        .iter()
        .find(|event| event.payload["path"] == "memory recall index")
        .expect("the chat opened with its memory");
    let content = recalled.payload["content"].to_string();
    assert!(content.contains("user-name"), "{content}");
    assert!(
        !content.contains("what to call the person"),
        "a page's words reached a new chat: {content}"
    );
    assert_eq!(
        recalled.trust,
        TrustLabel::Memory,
        "the memory block opened the chat as {:?}",
        recalled.trust
    );
    let tainted: Vec<_> = events
        .iter()
        .filter(|event| event.trust == TrustLabel::Web)
        .map(|event| event.kind.as_str())
        .collect();
    assert!(
        tainted.is_empty(),
        "a chat that read no page is web-tainted by {tainted:?}"
    );

    scripted(
        &mut client,
        &world,
        &chat,
        &[format!(
            "memory {}",
            json!({"op": "search", "query": "Asha"})
        )],
    )
    .await;
    let read = logged(&world, &chat).await;
    let search = read
        .iter()
        .find(|event| {
            event.kind.as_str() == "tool.observation" && event.payload["tool"] == "memory"
        })
        .expect("the chat searched its memory");
    assert!(
        search.payload.to_string().contains("Asha"),
        "{}",
        search.payload
    );
    assert_eq!(
        search.trust,
        TrustLabel::Web,
        "reading a web-sourced memory in full left the chat unlabelled"
    );
}

async fn logged(world: &World, chat: &str) -> Vec<kernel::Event> {
    let mut events = Vec::new();
    for state in std::fs::read_dir(world.home().join("projects")).unwrap() {
        let db = state.unwrap().path().join("events.db");
        if db.is_file() {
            events.extend(
                store::SqliteLog::open(&db)
                    .unwrap()
                    .events(chat.parse().unwrap())
                    .await,
            );
        }
    }
    events
}
