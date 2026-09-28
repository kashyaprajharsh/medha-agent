use super::*;

fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("medha-hook-files-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn an_added_hook_is_listed_with_its_tools_and_command_then_removed() {
    let root = workspace("roundtrip");
    let path = add(&root, "pre-tool", "shell.exec", "echo checking").unwrap();
    let file = path.file_name().unwrap().to_string_lossy().into_owned();
    let rows = list(&root);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["event"], "pre-tool");
    assert_eq!(rows[0]["matcher"], "shell.exec");
    assert_eq!(rows[0]["command"], "echo checking");
    remove(&root, "pre-tool", &file).unwrap();
    assert!(list(&root).is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn removing_is_confined_to_the_event_folder() {
    let root = workspace("confined");
    std::fs::write(root.join("keep.txt"), "x").unwrap();
    for file in ["../keep.txt", "..", "", "a/b.sh"] {
        assert!(remove(&root, "pre-tool", file).is_err(), "{file}");
    }
    assert!(remove(&root, "not-an-event", "x.sh").is_err());
    assert!(root.join("keep.txt").exists());
    std::fs::remove_dir_all(root).unwrap();
}
