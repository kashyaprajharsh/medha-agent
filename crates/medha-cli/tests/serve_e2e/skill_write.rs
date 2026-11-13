//! A skill written in the app is checked as it is typed and saved through the install scan.
use super::*;

async fn call(
    client: &mut Client,
    folder: &Path,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    client
        .about(folder, json!({"method": method, "params": params}))
        .await
}

const GOOD: &str = "---\nname: release-notes\ndescription: Writes release notes from merged pull requests.\nrequired_tools: [git]\n---\n\n1. List merged pull requests.\n";

/// The check reads SKILL.md as the backend will; a bad name is named as the problem.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skill_being_written_is_checked_as_the_backend_reads_it() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("write");
    client.open(&folder).await;
    let seen = call(
        &mut client,
        &folder,
        "extensions.skill.check",
        json!({"text": GOOD}),
    )
    .await
    .unwrap();
    assert_eq!(seen["name"], "release-notes", "{seen}");
    assert_eq!(
        seen["description"], "Writes release notes from merged pull requests.",
        "{seen}"
    );
    assert_eq!(seen["required_tools"], json!(["git"]), "{seen}");
    assert!(seen["error"].is_null(), "{seen}");
    let seen = call(
        &mut client,
        &folder,
        "extensions.skill.check",
        json!({"text": GOOD.replace("release-notes", "Release Notes")}),
    )
    .await
    .unwrap();
    assert!(
        seen["error"]
            .as_str()
            .is_some_and(|why| why.contains("kebab-case")),
        "{seen}"
    );
}

/// Saving writes SKILL.md and its files where the person chose; an existing skill is not overwritten unasked; paths stay inside.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_written_skill_is_saved_with_its_files_and_never_over_another() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("write");
    client.open(&folder).await;
    let files = json!([{"path": "SKILL.md", "text": GOOD}, {"path": "references/style.md", "text": "Plain words.\n"}]);
    let saved = call(
        &mut client,
        &folder,
        "extensions.skill.write",
        json!({"files": files, "scope": "project"}),
    )
    .await
    .unwrap();
    assert_eq!(saved["name"], "release-notes", "{saved}");
    let home = folder.join(".medha/skills/release-notes");
    assert_eq!(
        std::fs::read_to_string(home.join("references/style.md")).unwrap(),
        "Plain words.\n"
    );
    assert!(home.join("SKILL.md").is_file());

    let again = call(
        &mut client,
        &folder,
        "extensions.skill.write",
        json!({"files": files, "scope": "project"}),
    )
    .await;
    assert!(
        again.as_ref().is_err_and(|why| why.contains("already")),
        "{again:?}"
    );
    let changed = json!([{"path": "SKILL.md", "text": GOOD.replace("1. List", "1. Gather")}]);
    call(
        &mut client,
        &folder,
        "extensions.skill.write",
        json!({"files": changed, "scope": "project", "replace": true}),
    )
    .await
    .unwrap();
    assert!(
        std::fs::read_to_string(home.join("SKILL.md"))
            .unwrap()
            .contains("Gather")
    );

    let escaping = json!([{"path": "SKILL.md", "text": GOOD.replace("release-notes", "other")}, {"path": "../../outside.md", "text": "x"}]);
    let refused = call(
        &mut client,
        &folder,
        "extensions.skill.write",
        json!({"files": escaping}),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(!world.home().join("skills/other").exists() && !folder.join("outside.md").exists());
}
