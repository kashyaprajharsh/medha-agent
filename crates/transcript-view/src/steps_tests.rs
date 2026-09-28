use super::Steps;
use serde_json::{Value, json};

fn call(tool: &str, args: Value) -> crate::Call {
    Steps::default().call("c1", tool, &args)
}

fn run(tool: &str, args: Value, output: Value) -> crate::Outcome {
    let mut steps = Steps::default();
    steps.call("c1", tool, &args);
    steps.result("c1", "", true, &output)
}

#[test]
fn a_read_is_one_line_and_opens_to_the_file_text() {
    let read = call(
        "read",
        json!({"path": "src/main.rs", "offset": 1, "limit": 200}),
    );
    assert_eq!(
        (read.verb.as_str(), read.target.as_deref(), read.input),
        ("Read", Some("src/main.rs"), None)
    );
    let done = run(
        "fs.read",
        json!({"path": "src/main.rs"}),
        json!({"content": "fn main() {}", "path": "src/main.rs", "start_line": 1, "end_line": 200, "total_lines": 1400}),
    );
    assert_eq!(done.summary.as_deref(), Some("lines 1–200 of 1,400"));
    assert_eq!(done.output.as_deref(), Some("fn main() {}"));
}

#[test]
fn a_later_page_of_a_stored_read_is_named_after_its_file() {
    let mut steps = Steps::default();
    steps.call("r1", "read", &json!({"path": "docs/WHAT_IS_MEDHA.md"}));
    steps.result(
        "r1",
        "read",
        true,
        &json!({"content": "…", "hash": "64d557f8bb"}),
    );
    let page = steps.call(
        "r2",
        "read_artifact",
        &json!({"hash": "64d557f8bb", "offset": 14000, "length": 20000}),
    );
    assert_eq!(page.verb, "Read");
    assert_eq!(
        page.target.as_deref(),
        Some("docs/WHAT_IS_MEDHA.md · 14,000–34,000")
    );
}

#[test]
fn a_stored_page_that_holds_a_tool_result_shows_its_content() {
    let done = run(
        "read_artifact",
        json!({"hash": "aa"}),
        json!({"content": "{\"content\": \"class Model: ...\"}", "hash": "aa", "offset": 0}),
    );
    assert_eq!(done.output.as_deref(), Some("class Model: ..."));
}

#[test]
fn a_page_cut_from_the_middle_of_stored_json_reads_as_plain_text() {
    let page =
        r#" MCP servers\n- **Delegates** to \"sub-agents\"\n","path":"docs/WHAT_IS_MEDHA.md"}"#;
    let done = run(
        "read",
        json!({"hash": "aa", "offset": 2000}),
        json!({"content": page, "hash": "aa"}),
    );
    assert_eq!(
        done.output.as_deref(),
        Some("MCP servers\n- **Delegates** to \"sub-agents\"\n")
    );
    let first = r##"{"content":"# What is MEDHA?\n\nA harness"##;
    let done = run("read", json!({"hash": "aa"}), json!({"content": first}));
    assert_eq!(
        done.output.as_deref(),
        Some("# What is MEDHA?\n\nA harness")
    );
}

#[test]
fn a_command_opens_to_its_output_and_names_a_bad_exit() {
    let short = call("shell.exec", json!({"command": "cargo test"}));
    assert_eq!(
        (short.verb.as_str(), short.target.as_deref(), short.input),
        ("Ran", Some("cargo test"), None)
    );
    let script = call("shell.exec", json!({"command": "cd x\nmake"}));
    assert_eq!(script.input_label, Some("Command"));
    assert_eq!(script.input.as_deref(), Some("cd x\nmake"));
    let failed = run(
        "shell.exec",
        json!({"command": "cargo test"}),
        json!({"stdout": "ok\n", "stderr": "warning: x\n", "exit_code": 1, "command": "cargo test"}),
    );
    assert_eq!(failed.summary.as_deref(), Some("exit 1"));
    assert_eq!(failed.output.as_deref(), Some("ok\nwarning: x"));
    let passed = run(
        "git",
        json!({"subcommand": "log"}),
        json!({"stdout": "abc\n", "exit_code": 0}),
    );
    assert_eq!(passed.summary, None);
}

#[test]
fn a_search_names_what_it_looked_for_and_counts_what_it_found() {
    let grep = call(
        "grep",
        json!({"pattern": "fn main", "path": "crates", "case_insensitive": true}),
    );
    assert_eq!(
        (grep.verb.as_str(), grep.target.as_deref(), grep.input),
        ("Searched", Some("“fn main” in crates"), None)
    );
    let found = run(
        "grep",
        json!({"pattern": "x"}),
        json!({"count": 1, "truncated": true, "matches": [{"path": "plan.md", "line": 8, "text": "## Product"}]}),
    );
    assert_eq!(found.summary.as_deref(), Some("1 match+"));
    assert_eq!(found.output.as_deref(), Some("plan.md:8: ## Product"));
    let web = run(
        "web.search",
        json!({"query": "rust"}),
        json!({"count": 2, "results": [{"title": "Rust", "url": "https://rust-lang.org", "snippet": "…"}]}),
    );
    assert_eq!(web.summary.as_deref(), Some("2 results"));
    assert_eq!(web.output.as_deref(), Some("Rust\nhttps://rust-lang.org"));
}

#[test]
fn an_edit_shows_its_diff_and_how_many_lines_changed() {
    let edit = call(
        "fs.edit",
        json!({"path": "a.rs", "old_string": "a", "new_string": "b"}),
    );
    assert_eq!((edit.verb.as_str(), edit.input), ("Edited", None));
    let done = run(
        "fs.edit",
        json!({"path": "a.rs"}),
        json!({"diff": "--- a.rs\n+++ a.rs\n-a\n+b\n+c", "path": "a.rs", "new": "", "old": ""}),
    );
    assert_eq!(done.summary.as_deref(), Some("+2 −1"));
    assert_eq!(
        done.output.as_deref(),
        Some("--- a.rs\n+++ a.rs\n-a\n+b\n+c")
    );
    assert_eq!(
        call("edit", json!({"path": "a.md", "content": "x"})).verb,
        "Wrote"
    );
}

#[test]
fn a_spawn_names_the_agent_and_shows_its_task_as_prose() {
    let spawn = call(
        "agent.spawn",
        json!({"name": "read-medha-doc", "objective": "Read the doc", "contract": "Per-section summary"}),
    );
    assert_eq!(spawn.verb, "Started agent");
    assert_eq!(spawn.target.as_deref(), Some("read-medha-doc"));
    assert_eq!(spawn.input_label, Some("Task"));
    assert_eq!(
        spawn.input.as_deref(),
        Some("Read the doc\n\nExpected back: Per-section summary")
    );
    let started = run(
        "agent.spawn",
        json!({"name": "read-medha-doc"}),
        json!({"agent": "/read-medha-doc", "note": "Started. Do not poll.", "session": "01M3", "status": "running"}),
    );
    assert_eq!(started.summary.as_deref(), Some("started"));
    assert_eq!(started.output, None);
    let unnamed = call(
        "agent.spawn",
        json!({"objective": "Audit the kernel loop now please"}),
    );
    assert_eq!(unnamed.target.as_deref(), Some("audit-the-kernel-loop"));
}

#[test]
fn waiting_on_agents_says_who_reported_and_shows_their_reports() {
    let wait = call("agent", json!({"action": "wait", "timeout_seconds": 300}));
    assert_eq!(
        (wait.verb.as_str(), wait.target, wait.input),
        ("Waited for agents", None, None)
    );
    let done = run(
        "agent",
        json!({"action": "wait"}),
        json!({"_agent_report_dispatches": ["x"], "note": "These are their answers.", "settled": 1, "timed_out": false,
               "reports": [{"agent": "/read-medha-doc", "report": "## Budgets\n- per turn"}]}),
    );
    assert_eq!(done.summary.as_deref(), Some("read-medha-doc reported"));
    assert_eq!(
        done.output.as_deref(),
        Some("read-medha-doc\n## Budgets\n- per turn")
    );
    let quiet = run(
        "agent.wait",
        json!({}),
        json!({"note": "x", "settled": 0, "timed_out": true}),
    );
    assert_eq!(quiet.summary.as_deref(), Some("still running"));
    assert_eq!(quiet.output, None);
}

#[test]
fn other_agent_actions_name_the_agent_they_address() {
    let steer = call(
        "agent.steer",
        json!({"agent": "/kernel", "text": "Check the tests too"}),
    );
    assert_eq!(
        (
            steer.verb.as_str(),
            steer.target.as_deref(),
            steer.input.as_deref()
        ),
        ("Steered", Some("kernel"), Some("Check the tests too"))
    );
    let transcript = call(
        "agent",
        json!({"action": "transcript", "agent": "read-medha-doc", "tail": 6}),
    );
    assert_eq!(transcript.target.as_deref(), Some("read-medha-doc"));
    let read = run(
        "agent.transcript",
        json!({"agent": "x"}),
        json!({"agent": "x", "showing": 2, "total": 9, "steps": ["read a.rs", "  → ok"]}),
    );
    assert_eq!(read.summary.as_deref(), Some("last 2 of 9 steps"));
    assert_eq!(read.output.as_deref(), Some("read a.rs\n  → ok"));
}

#[test]
fn an_unknown_tool_reads_as_fields_and_a_failure_is_not_repeated_as_output() {
    let unknown = call(
        "mcp__github__create_issue",
        json!({"title": "Bug", "labels": ["a"]}),
    );
    assert_eq!(unknown.verb, "Create issue");
    assert_eq!(unknown.input_label, Some("Details"));
    assert_eq!(
        unknown.input.as_deref(),
        Some("labels: [\"a\"]\ntitle: Bug")
    );
    let mut steps = Steps::default();
    steps.call("c1", "read", &json!({"path": "missing.rs"}));
    let failed = steps.result(
        "c1",
        "read",
        false,
        &json!({"error": "read failed: No such file"}),
    );
    assert_eq!(failed.detail.as_deref(), Some("read failed: No such file"));
    assert_eq!(failed.output, None);
}

#[test]
fn written_filenames_remain_clickable_without_output_and_are_not_clipped_paths() {
    let path = format!("docs/{}/report.md", "long-folder/".repeat(25));
    for tool in ["write", "fs.write", "edit", "fs.edit"] {
        let step = call(tool, json!({"path": path, "content": "saved"}));
        assert_eq!(step.file_path.as_deref(), Some(path.as_str()));
    }
    assert!(
        call("shell.exec", json!({"command": "echo hi"}))
            .file_path
            .is_none()
    );
}
