//! The layout nearly everyone has: Medha's state is `~/.medha`.
#![cfg(unix)]

mod common;

#[tokio::test]
async fn in_the_default_layout_the_state_is_closed_except_what_medha_opens() {
    let root = common::fresh("default");
    // SAFETY: this file holds one test, so nothing else reads the environment meanwhile.
    unsafe {
        std::env::set_var("HOME", root.join("home"));
        std::env::remove_var("MEDHA_HOME");
    }
    if sandbox::native_backend_available() {
        let state = root.join("home/.medha");
        common::the_state_is_closed_except_what_medha_opens(&state, &root.join("plain")).await;
    }
    std::fs::remove_dir_all(&root).ok();
}
