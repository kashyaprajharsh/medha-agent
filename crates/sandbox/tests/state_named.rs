//! `MEDHA_HOME` names another folder: it is Medha's state just as `~/.medha` is,
//! and a `~/.medha` that is also there stays closed.
#![cfg(unix)]

mod common;

#[tokio::test]
async fn a_named_state_folder_is_closed_except_what_medha_opens() {
    let root = common::fresh("named");
    let state = root.join("kept-elsewhere");
    std::fs::create_dir_all(&state).unwrap();
    // SAFETY: this file holds one test, so nothing else reads the environment meanwhile.
    unsafe {
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("MEDHA_HOME", &state);
    }
    if sandbox::native_backend_available() {
        common::the_state_is_closed_except_what_medha_opens(&state, &root.join("plain")).await;
        let also = root.join("home/.medha");
        common::the_state_is_closed_except_what_medha_opens(&also, &root.join("plain")).await;
    }
    std::fs::remove_dir_all(&root).ok();
}
