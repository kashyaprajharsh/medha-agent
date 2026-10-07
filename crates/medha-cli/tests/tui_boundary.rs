//! Keep the terminal frontend a client. Shared rendering DTOs are allowed;
//! executing turns and owning durable stores/managers are not.
use std::path::Path;

fn check(folder: &Path) {
    let forbidden = [
        ".run_session(",
        "Kernel::new",
        "Kernel {",
        "Session::new(",
        "runtime::session::start",
        "Workspace::open(",
        "Store::open(",
        "SkillStore::",
        "ExtensionStore::",
        "McpManager",
        "AgentControl",
        "EventLog",
        "key_store::",
        "config::save_",
        "config::set_",
        "hooks::add(",
        "hooks::remove(",
    ];
    for entry in std::fs::read_dir(folder).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            check(&path);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = std::fs::read_to_string(&path).unwrap();
            for pattern in forbidden {
                assert!(
                    !source.contains(pattern),
                    "{} contains frontend engine/store ownership: {pattern}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn every_terminal_module_uses_the_backend_for_application_work() {
    let cli = Path::new(env!("CARGO_MANIFEST_DIR"));
    check(&cli.join("src/tui_tea"));
    let main = std::fs::read_to_string(cli.join("src/main.rs")).unwrap();
    let client = main.find("return tui_tea::backend_ui::run").unwrap();
    let native = main
        .find("let workspace_home = runtime::Workspace::open")
        .unwrap();
    assert!(
        client < native,
        "interactive startup must return before creating a local engine"
    );
    assert!(!main.contains("run_tea("));
    assert!(!main.contains("TuiGate"));
}
