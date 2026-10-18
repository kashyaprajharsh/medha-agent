use super::*;

#[test]
fn renamed_programs_launchers_and_bundles_are_identified_without_execution() {
    let root = std::env::temp_dir().join(format!("medha-open-types-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for (name, bytes, expected) in [
        ("report.pdf", b"%PDF".as_slice(), false),
        ("source.rs", b"fn main() {}", false),
        ("report.txt", b"\x7fELF", true),
        ("image.png", b"MZ\0\0", true),
        ("script", b"#!/bin/sh\n", true),
        ("page.desktop", b"[Desktop Entry]", true),
        ("note.lnk", b"shortcut", true),
    ] {
        let path = root.join(name);
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(runnable(&path).unwrap(), expected, "{name}");
    }
    std::fs::create_dir_all(root.join("Program.app")).unwrap();
    assert!(runnable(&root.join("Program.app")).unwrap());
    assert!(!runnable(&root).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let program = root.join("program");
        std::fs::write(&program, "contents").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&program, root.join("document.md")).unwrap();
        assert!(runnable(&root.join("document.md")).unwrap());
    }
    std::fs::remove_dir_all(root).unwrap();
}
