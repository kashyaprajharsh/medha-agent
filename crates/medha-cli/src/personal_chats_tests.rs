use super::*;

/// The terminal looks for personal chats where the desktop app keeps them.
#[test]
fn the_desktop_identifier_is_the_one_the_desktop_app_uses() {
    let config =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/desktop/src-tauri/tauri.conf.json");
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(config).unwrap()).unwrap();
    assert_eq!(config["identifier"], DESKTOP_APP);
}
