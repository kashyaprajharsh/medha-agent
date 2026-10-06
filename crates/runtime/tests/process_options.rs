//! Every surface starts a chat from what the process's environment asks. This is
//! the one test in its binary, so the environment it sets is nobody else's.

use runtime::SessionOptions;

fn set(name: &str, value: &str) {
    // SAFETY: this binary runs this one test, so no other thread reads the environment.
    unsafe { std::env::set_var(name, value) }
}

#[test]
fn everything_the_environment_asks_of_a_chat_is_in_the_options_every_surface_starts_from() {
    for (name, value) in [
        ("MEDHA_MODE", "plan"),
        ("MEDHA_VERIFY", "cargo test"),
        ("MEDHA_SANDBOX", "host"),
        ("MEDHA_TOOLS", "minimal"),
        ("MEDHA_MAX_PARALLEL_TOOLS", "3"),
        ("MEDHA_APPROVE", "shell.exec"),
        ("MEDHA_REASONING_EFFORT", "low"),
        ("MEDHA_MAX_TURNS", "7"),
    ] {
        set(name, value);
    }
    let asked = SessionOptions::from_process(None, None).unwrap();
    assert_eq!(asked.autonomy, Some(kernel::AutonomyLevel::Plan));
    assert_eq!(asked.verify_command.as_deref(), Some("cargo test"));
    assert_eq!(asked.sandbox, Some(sandbox::BackendKind::Host));
    assert_eq!(asked.tools_preset.as_deref(), Some("minimal"));
    assert_eq!(asked.max_parallel_tools, Some(3));
    assert_eq!(asked.approve.as_deref(), Some("shell.exec"));
    assert!(asked.reasoning.is_some());
    assert_eq!(asked.budget.max_turns, Some(7));

    // An empty command asks for no check, and a mode that does not exist is refused, not ignored.
    set("MEDHA_VERIFY", "  ");
    let unchecked = SessionOptions::from_process(None, None).unwrap();
    assert_eq!(unchecked.verify_command, None);
    set("MEDHA_MODE", "no-such-mode");
    set("MEDHA_REASONING_EFFORT", "no-such-effort");
    assert!(SessionOptions::from_process(None, None).is_err());

    // What the caller chose is used as it is, whatever the environment says in its place.
    let careful = Some(kernel::AutonomyLevel::Careful);
    let chosen = SessionOptions::from_process(careful, asked.reasoning.clone()).unwrap();
    assert_eq!(chosen.autonomy, careful);
    assert_eq!(chosen.reasoning, asked.reasoning);
    assert!(SessionOptions::from_process(careful, None).is_err());
}
