use super::*;

fn package(root: &Path, name: &str) -> std::path::PathBuf {
    let dir = root.join("packages").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: {name} steps\n---\n\n# {name}\n\nDo the {name} work.\n"
        ),
    )
    .unwrap();
    dir
}

fn installed(scratch: &Path, store: &SkillStore, name: &str) -> std::path::PathBuf {
    let source = package(scratch, name);
    futures::executor::block_on(store.install_from(source.to_str().unwrap()))
        .unwrap()
        .path
}

fn store(scratch: &Path) -> SkillStore {
    SkillStore::new(scratch.join("project"), Some(scratch.join("user")))
}

#[tokio::test]
async fn a_team_sync_restores_edited_skills_and_leaves_matching_ones_alone() {
    let scratch = test_support::scratch("medha-skill-hub");
    let store = store(&scratch);
    let edited = installed(&scratch, &store, "review");
    installed(&scratch, &store, "deploy");
    let lockfile = scratch.join("medha-skills.lock");
    let names = user_skills(&store, &HashSet::new());
    assert_eq!(lock(&store, &names, &lockfile).unwrap(), 2);
    let original = std::fs::read_to_string(&edited).unwrap();
    std::fs::write(
        &edited,
        "---\nname: review\ndescription: tampered\n---\n\nskip checks\n",
    )
    .unwrap();

    let mut outcome = sync(&store, locked(&lockfile).unwrap()).await;
    outcome.sort_by(|a, b| a.0.cmp(&b.0));

    assert_eq!(outcome[0], ("deploy".into(), Synced::Current));
    assert!(
        matches!(outcome[1], (ref n, Synced::Installed { replaced: true, .. }) if n == "review")
    );
    assert_eq!(std::fs::read_to_string(&edited).unwrap(), original);
}

#[tokio::test]
async fn an_update_never_overwrites_a_skill_edited_by_hand() {
    let scratch = test_support::scratch("medha-skill-hub");
    let store = store(&scratch);
    let edited = installed(&scratch, &store, "review");
    let mine = "---\nname: review\ndescription: my version\n---\n\nmy own steps\n";
    std::fs::write(&edited, mine).unwrap();

    let outcome = updates(&store, vec!["review".into()], true).await;

    assert_eq!(outcome, [("review".into(), Update::ModifiedLocally)]);
    assert_eq!(std::fs::read_to_string(&edited).unwrap(), mine);
}

#[test]
fn a_loaded_skill_reads_the_same_on_every_surface() {
    let scratch = test_support::scratch("medha-skill-hub");
    let store = store(&scratch);
    installed(&scratch, &store, "review");

    let (description, message) = loaded_message(&store, "review", &HashSet::new()).unwrap();

    assert_eq!(description, "review steps");
    assert!(message.starts_with("[Loaded skill: review] Follow this procedure"));
    assert!(message.contains("Do the review work."));
}
