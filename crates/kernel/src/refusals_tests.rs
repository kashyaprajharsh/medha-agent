use super::*;

fn call(path: &str) -> ToolIntent {
    ToolIntent {
        id: "i".into(),
        tool: "shell.exec".into(),
        args: serde_json::json!({ "command": format!("cat {path}") }),
    }
}

/// The second identical refusal tells the model to stop varying it; the third also tells the person.
#[test]
fn a_repeated_refusal_is_called_a_dead_end() {
    let mut refusals = Refusals::default();
    let refuse = || Observation::denial("i", "blocked dangerous command: privilege escalation");
    let mut first = refuse();
    assert!(refusals.note(&call("a"), false, &mut first).is_none());
    assert!(first.payload.get("repeated").is_none());
    let mut second = refuse();
    assert!(refusals.note(&call("b"), false, &mut second).is_none());
    assert!(
        second.payload["repeated"]
            .as_str()
            .unwrap()
            .contains("twice")
    );
    let mut third = refuse();
    let notice = refusals.note(&call("c"), false, &mut third).unwrap();
    assert!(notice.contains("three times") && notice.contains("told to stop"));
    assert!(third.payload["repeated"].as_str().unwrap().contains("Stop"));
    // A different reason, or a success, is counted on its own.
    let mut other = Observation::denial("i", "another reason");
    assert!(refusals.note(&call("d"), false, &mut other).is_none());
    let mut ok = Observation::ok("i", serde_json::json!({}));
    assert!(refusals.note(&call("e"), false, &mut ok).is_none());
    assert!(ok.payload.get("repeated").is_none());
}
