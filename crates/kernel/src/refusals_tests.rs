use super::*;

/// The second identical refusal tells the model to stop varying it; the third also tells the person.
#[test]
fn a_repeated_refusal_is_called_a_dead_end() {
    let mut refusals = Refusals::default();
    let refuse = || Observation::denial("i", "blocked dangerous command: privilege escalation");
    let mut first = refuse();
    assert!(refusals.note("shell.exec", &mut first).is_none());
    assert!(first.payload.get("repeated").is_none());
    let mut second = refuse();
    assert!(refusals.note("shell.exec", &mut second).is_none());
    assert!(
        second.payload["repeated"]
            .as_str()
            .unwrap()
            .contains("twice")
    );
    let mut third = refuse();
    let notice = refusals.note("shell.exec", &mut third).unwrap();
    assert!(notice.contains("three times"));
    assert!(third.payload["repeated"].as_str().unwrap().contains("Stop"));
    // A different reason, tool, or a success is counted on its own.
    let mut other = Observation::denial("i", "another reason");
    assert!(refusals.note("shell.exec", &mut other).is_none());
    let mut ok = Observation::ok("i", serde_json::json!({}));
    assert!(refusals.note("shell.exec", &mut ok).is_none());
    assert!(ok.payload.get("repeated").is_none());
}
