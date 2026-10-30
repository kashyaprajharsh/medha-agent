use super::*;

fn chats() -> (tempfile::TempDir, Chats) {
    let root = tempfile::tempdir().unwrap();
    let chats = Chats::at(root.path().join("chats.json"));
    (root, chats)
}

#[test]
fn a_map_written_before_project_still_continues_its_chats() {
    let (root, chats) = chats();
    std::fs::write(
        root.path().join("chats.json"),
        r#"{"telegram:1": "conversation-a"}"#,
    )
    .unwrap();
    assert_eq!(
        chats.conversation("telegram:1").unwrap().as_deref(),
        Some("conversation-a")
    );
    assert_eq!(chats.folder("telegram:1").unwrap(), None);
    assert_eq!(chats.all().unwrap().len(), 1);
}

#[test]
fn choosing_a_folder_starts_over_there_and_new_keeps_the_folder() {
    let (_root, chats) = chats();
    let site = PathBuf::from("/work/site");
    chats.remember("telegram:1", "in-medha").unwrap();
    chats.choose_folder("telegram:1", &site).unwrap();
    // The old conversation belongs to the old folder, so it is not continued here.
    assert_eq!(chats.conversation("telegram:1").unwrap(), None);
    assert_eq!(chats.folder("telegram:1").unwrap(), Some(site.clone()));
    chats.remember("telegram:1", "in-site").unwrap();
    chats.forget("telegram:1").unwrap();
    assert_eq!(chats.conversation("telegram:1").unwrap(), None);
    assert_eq!(chats.folder("telegram:1").unwrap(), Some(site));
    // Another chat is untouched.
    assert_eq!(chats.folder("telegram:2").unwrap(), None);
}
