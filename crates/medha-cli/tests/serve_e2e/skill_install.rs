//! The model installs a skill only with the person's approval, into their skills or this project's, ready to use at once.
use super::*;

/// A skill folder outside any project, as a person would point the model at.
fn packaged(world: &World, name: &str) -> PathBuf {
    let folder = world.folder(&format!("package-{name}"));
    std::fs::write(
        folder.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Greets the person by name when asked to say hello.\n---\n\n# Greet\n\nSay hello.\n"),
    )
    .unwrap();
    folder
}

/// Runs the script, answering every card with `approve`; returns how many cards were shown.
async fn answered(client: &mut Client, chat: &str, lines: &[String], approve: bool) -> usize {
    client
        .send(chat, &format!("SCRIPT\n{}", lines.join("\n")))
        .await;
    let mut cards = 0;
    loop {
        let (_, frame) = client
            .until(|frame| frame["method"] == "approval" || kind(frame, "turn.done"))
            .await;
        if frame["method"] != "approval" {
            return cards;
        }
        cards += 1;
        let answer = json!({"gate_id": frame["params"]["gate_id"], "approve": approve});
        client.ask("approval.respond", Some(chat), answer).await;
    }
}

/// Installing asks first; a refusal leaves nothing behind, an approval puts the skill in the person's skills, usable at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_model_installs_a_skill_only_when_the_person_approves() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&world.folder("installs")).await;
    let source = packaged(&world, "greet");
    let install = format!("skill_save {}", json!({"job": "install", "source": source}));

    let cards = answered(&mut client, &chat, std::slice::from_ref(&install), false).await;
    assert_eq!(cards, 1, "the install did not ask the person");
    assert!(
        !world.home().join("skills/greet").exists(),
        "a refused install left files behind"
    );

    let cards = answered(&mut client, &chat, &[install, "skill {}".into()], true).await;
    assert_eq!(cards, 1, "the install did not ask the person");
    assert!(
        world.home().join("skills/greet/SKILL.md").is_file(),
        "the approved skill is not in the person's skills"
    );
    let listed = std::iter::from_fn(|| world.provider.seen.try_recv().ok())
        .last()
        .unwrap()["messages"]
        .to_string();
    assert!(
        listed.contains("Greets the person by name"),
        "the installed skill was not usable at once: {listed}"
    );
}

/// Asked for this project, the skill goes into the project's skills and not the person's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skill_installed_for_this_project_stays_in_the_project() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let project = world.folder("project-installs");
    let chat = client.open(&project).await;
    let source = packaged(&world, "local-greet");
    let install = format!(
        "skill_save {}",
        json!({"job": "install", "source": source, "scope": "project"})
    );
    answered(&mut client, &chat, &[install], true).await;
    assert!(
        project.join(".medha/skills/local-greet/SKILL.md").is_file(),
        "the skill is not in the project"
    );
    assert!(
        !world.home().join("skills/local-greet").exists(),
        "a project skill went into the person's skills"
    );
}
