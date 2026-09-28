use super::*;

fn call(
    session: Ulid,
    identity: &str,
    prompt: u64,
    completion: Option<u64>,
    cost: Option<f64>,
) -> Call {
    Call {
        session,
        ts: 1_750_000_000.0,
        identity: identity.into(),
        prompt,
        completion,
        cached: Some(prompt / 2),
        cost,
    }
}

#[test]
fn the_model_is_read_past_the_endpoints_own_colons() {
    assert_eq!(
        model_label("open-ai-chat:http://127.0.0.1:8080/v1:qwen3:8b"),
        "qwen3:8b"
    );
    assert_eq!(
        model_label("open-ai-chat:https://api.example.com/v1:gpt-5"),
        "gpt-5"
    );
    assert_eq!(model_label("anthropic"), "anthropic");
}

#[test]
fn sub_agents_count_in_their_parent_and_unknown_cost_stays_unknown() {
    let (parent, child, other) = (Ulid::new(), Ulid::new(), Ulid::new());
    let sessions = HashMap::from([
        (parent, ("Fix the login bug".to_owned(), None)),
        (child, ("reader".to_owned(), Some(parent))),
        (other, ("Local chat".to_owned(), None)),
    ]);
    let cloud = "open-ai-chat:https://api.example.com/v1:gpt-5";
    let local = "open-ai-chat:http://127.0.0.1:8080/v1:qwen";
    let calls = [
        call(parent, cloud, 1_000, Some(200), Some(0.01)),
        call(child, cloud, 3_000, Some(100), Some(0.02)),
        call(other, local, 500, None, None),
    ];
    let summary = summarize(&calls, &sessions, 30);
    assert_eq!(summary["total"]["calls"], 3);
    assert_eq!(summary["total"]["prompt_tokens"], 4_500);
    assert_eq!(summary["total"]["unpriced_calls"], 1);
    assert!((summary["total"]["cost_usd"].as_f64().unwrap() - 0.03).abs() < 1e-9);
    let top = &summary["sessions"][0];
    assert_eq!(top["title"], "Fix the login bug");
    assert_eq!(top["agents"], 1);
    assert_eq!(top["usage"]["calls"], 2);
    let local_model = summary["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["model"] == "qwen")
        .unwrap();
    assert_eq!(local_model["usage"]["cost_usd"], Value::Null);
}

#[test]
fn the_tui_text_says_when_cost_is_unknown() {
    let session = Ulid::new();
    let sessions = HashMap::from([(session, ("chat".to_owned(), None))]);
    let summary = summarize(
        &[call(session, "p:http://h/v1:m", 2_500, Some(40), None)],
        &sessions,
        7,
    );
    let text = render(&summary, &summary);
    assert!(
        text.contains("1 calls · 2.5k in (1.2k cached) · 40 out · cost unknown"),
        "{text}"
    );
}
