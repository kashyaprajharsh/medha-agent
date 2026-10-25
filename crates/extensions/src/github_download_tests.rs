use super::*;
use std::io::Write;

#[test]
fn an_archive_unpacks_inside_its_folder_and_links_follow_the_clone_rule() {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    for (name, body) in [
        ("me-plugin-abc/plugin.json", "{}"),
        ("me-plugin-abc/skills/a.md", "skill"),
    ] {
        writer.start_file(name, options).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    writer
        .add_symlink("me-plugin-abc/copy.md", "skills/a.md", options)
        .unwrap();
    writer
        .add_symlink("me-plugin-abc/secret", "../../etc/passwd", options)
        .unwrap();
    let archive = writer.finish().unwrap().into_inner();
    let dir = tempfile::tempdir().unwrap();
    unpack(&archive, dir.path()).unwrap();
    assert!(
        dir.path().join("plugin.json").is_file(),
        "wrapper folder stripped"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("copy.md")).unwrap(),
        "skill"
    );
    assert!(
        !dir.path().join("secret").exists(),
        "a link out of the repository is dropped"
    );

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file("me-plugin-abc/../../escape", options)
        .unwrap();
    writer.write_all(b"x").unwrap();
    let escaping = writer.finish().unwrap().into_inner();
    assert!(matches!(
        unpack(&escaping, tempfile::tempdir().unwrap().path()),
        Err(Error::UnsafePackage(_))
    ));
}
