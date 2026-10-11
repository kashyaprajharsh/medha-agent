use super::*;
use serde_json::json;

/// The case the audit measured: renaming one name in a 9 KB file used to send
/// the model ~29 KB (the whole file three times). It now reads the change and
/// three lines either side, and the full result is left untouched for people.
#[test]
fn a_one_line_edit_in_a_large_file_reads_as_its_change() {
    let before: String = (1..=300)
        .map(|n| format!("let value_{n:03} = compute(old_name, {n});\n"))
        .collect();
    assert!(before.len() > 9_000);
    let after = before.replacen(
        "let value_150 = compute(old_name, 150);",
        "let value_150 = compute(new_name, 150);",
        1,
    );
    let payload = json!({"path": "src/lib.rs", "old": before, "new": after, "replacements": 1});
    let view = file_change(&payload).unwrap();
    assert!(view.len() < 1_000, "{} bytes:\n{view}", view.len());
    assert!(view.starts_with("Changed src/lib.rs: +1 -1 lines, 1 replacement(s)."));
    assert!(view.contains("-let value_150 = compute(old_name, 150);"));
    assert!(view.contains("+let value_150 = compute(new_name, 150);"));
    for neighbour in ["value_147", "value_153"] {
        assert!(view.contains(neighbour), "context line {neighbour} kept");
    }
    assert!(!view.contains("value_146") && !view.contains("value_154"));
    assert!(view.contains("@@ -147,7 +147,7 @@"), "{view}");
}

#[test]
fn a_page_is_numbered_exactly_and_says_where_the_next_one_starts() {
    let payload = json!({
        "content": "fn a() {}\r\n\n    let x = 1;", "start_line": 41,
        "end_line": 43, "total_lines": 900, "has_more": true
    });
    assert_eq!(
        file_page(&payload).unwrap(),
        "41|fn a() {}\r\n42|\n43|    let x = 1;\n[lines 41-43 of 900; continue with offset=44]"
    );
    let long = "x".repeat(5_000);
    let view = file_page(&json!({"content": long, "start_line": 1, "has_more": false})).unwrap();
    assert!(
        view.ends_with(" … [line cut: 2000 of 5000 bytes shown]\n"),
        "{view}"
    );
    assert_eq!(
        file_page(&json!({"content": "", "start_line": 1, "has_more": false})).unwrap(),
        "[empty file]"
    );
}

/// A test run as the model reads it: colour codes and progress-bar frames are
/// gone, errors come first and the run's ending last, and every other field
/// (a hint, a blocked path) is still there.
#[test]
fn a_command_reads_as_settled_text_with_errors_first_and_the_ending_last() {
    let payload = json!({
        "command": "cargo test", "workdir": ".", "exit_code": 101,
        "stderr": "\u{1b}[1m\u{1b}[32m   Compiling\u{1b}[0m demo\n 10%\r 50%\r100%\r\nerror: oops\r\n",
        "stdout": "test a ... ok\n\u{1b}]8;;https://x\u{7}link\u{1b}]8;;\u{7}\ntest result: FAILED. 1 passed; 1 failed\n",
        "stdout_truncated": true, "stderr_truncated": false,
        "sandbox_hint": "network is off", "denied_paths": ["/etc/x"], "note": null
    });
    assert_eq!(
        shell_run(&payload).unwrap(),
        "$ cargo test\nexit code: 101\ndenied_paths: [\"/etc/x\"]\nsandbox_hint: network is off\n\
         --- stderr ---\n   Compiling demo\n100%\nerror: oops\n\
         --- stdout ---\n[earlier output was dropped; the last part is kept]\n\
         test a ... ok\nlink\ntest result: FAILED. 1 passed; 1 failed"
    );
    assert_eq!(clean_terminal("\u{1b}(Bplain\u{1b}[?25l"), "plain");
}

#[test]
fn written_content_is_named_not_echoed_and_notes_and_diagnostics_are_kept() {
    let content = "fn main() {}\n".repeat(400);
    let view =
        file_change(&json!({"path": "a.rs", "old": "", "new": content, "written": true})).unwrap();
    assert_eq!(
        view,
        format!("Wrote a.rs (400 lines, {} bytes).", content.len())
    );

    let view = file_change(&json!({
        "path": "a.rs", "old": "a\n", "new": "b\n",
        "note": "matched loosely", "lsp": {"errors": 1}
    }))
    .unwrap();
    assert!(view.contains("Note: matched loosely"));
    assert!(view.contains(r#"Diagnostics after the change: {"errors":1}"#));
    assert_eq!(
        file_change(&json!({"path": "a.rs", "old": "same\n", "new": "same\n"})).unwrap(),
        "a.rs is unchanged."
    );
}
