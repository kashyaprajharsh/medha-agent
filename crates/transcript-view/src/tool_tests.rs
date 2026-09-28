use super::*;

#[test]
fn the_combined_agent_tool_and_fs_spellings_resolve_to_one_name() {
    assert_eq!(canonical("agent", &json!({"action": "wait"})), "agent.wait");
    assert_eq!(canonical("fs.read", &json!({"path": "a"})), "read");
    assert_eq!(canonical("read_artifact", &json!({"hash": "a"})), "read");
    assert_eq!(canonical("shell.exec", &json!({})), "shell.exec");
}

#[test]
fn a_tool_without_a_view_reads_as_fields_not_json() {
    let text = fields(
        &json!({"_agent_report_dispatches": ["x"], "name": "notes", "count": 3, "body": "a\nb"}),
        OUTPUT_LIMIT,
    )
    .unwrap();
    assert_eq!(text, "body:\n  a\n  b\ncount: 3\nname: notes");
    assert_eq!(fields(&json!({}), OUTPUT_LIMIT), None);
}

#[test]
fn long_text_is_bounded_and_says_how_much_was_cut() {
    assert!(bounded(&"a".repeat(OUTPUT_LIMIT + 5), OUTPUT_LIMIT).ends_with("… 5 more characters"));
}

#[test]
fn a_plan_lists_each_step_with_its_status() {
    let args = json!({"explanation": "Map first", "steps": [
        {"title": "Map the diff", "status": "completed"},
        {"step": "Review", "status": "in_progress"},
        {"content": "Report"}
    ]});
    let plan = plan("update_plan", &args).unwrap();
    assert_eq!(plan["explanation"], "Map first");
    assert_eq!(
        plan["steps"][1],
        json!({"title": "Review", "status": "in_progress"})
    );
    assert_eq!(plan["steps"][2]["status"], "pending");
    assert_eq!(super::plan("read", &args), None);
}

#[test]
fn a_failure_says_why() {
    assert_eq!(
        failure_detail(&json!({"error": "read failed: No such file"})).as_deref(),
        Some("read failed: No such file")
    );
    assert_eq!(
        failure_detail(&json!({"reason": "rejected by human"})).as_deref(),
        Some("rejected by human")
    );
    assert_eq!(failure_detail(&json!({"stdout": "x"})), None);
}

#[test]
fn clipping_keeps_one_line_and_marks_the_cut() {
    assert_eq!(clip("echo one\necho two", 240), "echo one…");
    assert_eq!(clip(&"a".repeat(10), 4), "aaaa…");
    assert_eq!(clip("  short  ", 240), "short");
    assert_eq!(grouped(1_234_567), "1,234,567");
}
