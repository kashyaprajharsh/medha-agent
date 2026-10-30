use super::*;
use serde_json::json;

#[test]
fn each_step_is_one_line_marked_as_it_ends_and_named_from_the_chats_folder() {
    let folder = Path::new("/work/site");
    let mut progress = Progress::default();
    progress.call(
        Some("1"),
        "read",
        &json!({"path": "/work/site/src/main.rs"}),
        folder,
    );
    progress.call(
        Some("2"),
        "shell.exec",
        &json!({"command": "cargo test"}),
        folder,
    );
    progress.call(Some("3"), "read", &json!({"path": "/etc/hosts"}), folder);
    progress.result(Some("2"), false);
    progress.result(Some("1"), true);
    assert_eq!(
        progress.text(),
        "✓ Read `src/main.rs`\n✗ Ran `cargo test`\n◦ Read `/etc/hosts`"
    );
    assert_eq!(
        crate::telegram_format::to_telegram(&progress.text()).0,
        "✓ Read <code>src/main.rs</code>\n✗ Ran <code>cargo test</code>\n◦ Read <code>/etc/hosts</code>"
    );
    progress.close();
    assert!(progress.is_empty());
    progress.result(Some("3"), true);
    assert!(
        progress.is_empty(),
        "a step from a closed message changes nothing"
    );
}

#[test]
fn a_long_run_keeps_the_message_short_and_a_target_cannot_break_out_of_its_code() {
    let folder = Path::new("/w");
    let mut progress = Progress::default();
    for n in 0..SHOWN + 3 {
        progress.call(
            Some(&n.to_string()),
            "shell.exec",
            &json!({"command": format!("step {n}")}),
            folder,
        );
    }
    let text = progress.text();
    assert_eq!(text.lines().count(), SHOWN + 1);
    assert!(
        text.starts_with("3 earlier steps\n◦ Ran `step 3`"),
        "{text}"
    );
    // The earliest steps are gone from the screen, so their results are not looked for.
    progress.result(Some("0"), true);
    assert_eq!(progress.text(), text);

    let mut tricky = Progress::default();
    tricky.call(
        None,
        "shell.exec",
        &json!({"command": "echo `id` <b>*x*</b>"}),
        folder,
    );
    assert_eq!(
        crate::telegram_format::to_telegram(&tricky.text()).0,
        "◦ Ran <code>echo `id` &lt;b&gt;*x*&lt;/b&gt;</code>"
    );
}
