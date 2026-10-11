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
        written: None,
        cost,
    }
}

/// The hit rate counts only calls whose server reported re-reads. A local
/// server that says nothing must not make a caching route look like it misses.
#[test]
fn the_hit_rate_ignores_calls_that_reported_no_cache_count() {
    let session = Ulid::new();
    let sessions = HashMap::from([(session, ("chat".to_owned(), None))]);
    let mut caching = call(session, "p:https://a/v1:claude", 10_000, Some(100), None);
    caching.cached = Some(9_000);
    caching.written = Some(800);
    let mut silent = call(session, "p:http://h/v1:local", 30_000, Some(100), None);
    silent.cached = None;

    let summary = summarize(&[caching, silent], &sessions, 7);
    let total = &summary["total"];
    assert_eq!(total["prompt_tokens"], 40_000);
    assert_eq!(total["cache_measured_tokens"], 10_000);
    assert_eq!(total["cache_write_tokens"], 800);
    let text = render(&summary, &summary);
    assert!(
        text.contains("40.0k in (9.0k cached · 90% hit · 800 stored)"),
        "{text}"
    );

    // No call reported anything: no rate at all, never 0%.
    let mut quiet = call(session, "p:http://h/v1:local", 2_000, Some(10), None);
    quiet.cached = None;
    let summary = summarize(&[quiet], &sessions, 7);
    let text = render(&summary, &summary);
    assert!(text.contains("2.0k in (0 cached) · 10 out"), "{text}");
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
    assert_eq!(top["models"][0]["usage"], top["usage"]);
    let daily_models = summary["by_day"][0]["models"].as_array().unwrap();
    assert_eq!(daily_models.len(), 2);
    assert_eq!(
        daily_models
            .iter()
            .map(|row| row["usage"]["calls"].as_u64().unwrap())
            .sum::<u64>(),
        3
    );
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
        text.contains("1 calls · 2.5k in (1.2k cached · 50% hit) · 40 out · cost unknown"),
        "{text}"
    );
}
