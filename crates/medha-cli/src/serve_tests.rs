use super::*;
use std::os::unix::fs::PermissionsExt;

/// A socket put where another user can write could be kept from ever being made.
#[test]
fn a_socket_is_never_put_in_a_folder_others_can_write_in() {
    let root = tempfile::tempdir().unwrap();
    let mode = |name: &str, mode: u32| {
        let folder = root.path().join(name);
        std::fs::create_dir(&folder).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(mode)).unwrap();
        folder
    };
    assert!(is_only_mine(&mode("mine", 0o700)));
    assert!(is_only_mine(&mode("read-by-all", 0o755)));
    // As a shared temporary folder is: anyone may make what they like in it.
    assert!(!is_only_mine(&mode("shared", 0o1777)));
    assert!(!is_only_mine(&mode("group", 0o770)));
    let file = root.path().join("file");
    std::fs::write(&file, "").unwrap();
    assert!(!is_only_mine(&file));
    assert!(!is_only_mine(Path::new("relative")));
    assert!(!is_only_mine(&root.path().join("absent")));
}
