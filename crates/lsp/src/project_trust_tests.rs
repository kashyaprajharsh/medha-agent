use super::*;

#[test]
fn a_notice_requires_code_for_an_installed_server_in_this_checkout() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().canonicalize().unwrap();
    let mut config = Config {
        servers: vec![ServerAdapter::rust_analyzer()],
        ..Default::default()
    };
    config.servers[0].command = vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ];
    std::fs::write(root.join("README.md"), "Plain notes").unwrap();
    assert!(!would_start_in(&root, &config));
    let nested = root.join("vendor/nested");
    std::fs::create_dir_all(nested.join(".git")).unwrap();
    std::fs::write(nested.join("main.rs"), "fn main() {}").unwrap();
    assert!(
        !would_start_in(&root, &config),
        "nested checkouts cannot cause an outer notice"
    );
    assert!(would_start_in(&nested, &config));
    std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
    assert!(would_start_in(&root, &config));
    config.servers[0].command = vec!["medha-missing-fixture-language-server".into()];
    assert!(!would_start_in(&root, &config));
    config.enabled = false;
    assert!(!would_start_in(&root, &config));
}
