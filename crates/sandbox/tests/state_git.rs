//! Medha's own state folders are shared by every Medha process, so a command's Git guard never touches them.
#![cfg(target_os = "linux")]

#[allow(dead_code)]
mod common;

use sandbox::{ApprovedRoots, StateAccess};

#[tokio::test]
async fn a_shared_state_folder_gets_no_git_placeholders_while_the_project_keeps_them() {
    let root = common::fresh("state-git");
    let state = root.join("state");
    let cache = state.join("mcp-cache");
    let project = root.join("project");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    // SAFETY: this file holds one test, so nothing else reads the environment meanwhile.
    unsafe {
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("MEDHA_HOME", &state);
    }
    let backend = common::backend(
        StateAccess {
            read: Vec::new(),
            write: vec![cache.clone()],
        },
        ApprovedRoots::default(),
    );
    let script = format!(
        "if [ -e {}/.git ]; then printf 'placeholder '; else printf 'clean '; fi; \
         if [ -f .git/config ] && ! (printf x >> .git/config) 2>/dev/null; then echo protected; else echo open; fi",
        cache.display()
    );
    assert_eq!(
        common::shell(&backend, &project, &script).await,
        "clean protected"
    );
    assert!(!cache.join(".git").exists());
    std::fs::remove_dir_all(&root).ok();
}
