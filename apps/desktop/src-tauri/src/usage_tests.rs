use super::*;

fn usage(calls: u64, prompt: u64, cost: Option<f64>) -> Value {
    json!({"calls": calls, "prompt_tokens": prompt, "completion_tokens": 10, "cached_tokens": 0,
           "cost_usd": cost, "unpriced_calls": if cost.is_some() { 0 } else { calls }})
}

fn summary(model: &str, day: &str, calls: u64, prompt: u64, cost: Option<f64>) -> Value {
    json!({
        "total": usage(calls, prompt, cost),
        "models": [{"model": model, "usage": usage(calls, prompt, cost)}],
        "by_day": [{"day": day, "usage": usage(calls, prompt, cost),
            "models": [{"model": model, "usage": usage(calls, prompt, cost)}]}],
        "sessions": [{"id": format!("{model}-{day}"), "usage": usage(calls, prompt, cost),
            "models": [{"model": model, "usage": usage(calls, prompt, cost)}]}],
    })
}

#[test]
fn chats_in_separate_folders_add_up_by_model_and_day() {
    let merged = merge(
        vec![
            summary("qwen", "2026-09-27", 2, 1_000, None),
            summary("gpt-5", "2026-09-27", 1, 5_000, Some(0.05)),
            summary("qwen", "2026-09-28", 3, 2_000, None),
        ],
        30,
    );
    assert_eq!(merged["total"]["calls"], 6);
    assert_eq!(merged["total"]["prompt_tokens"], 8_000);
    assert_eq!(merged["total"]["unpriced_calls"], 5);
    assert_eq!(merged["total"]["cost_usd"], 0.05);
    assert_eq!(merged["models"][0]["model"], "gpt-5");
    assert_eq!(merged["models"][1]["usage"]["calls"], 5);
    assert_eq!(merged["models"][1]["usage"]["cost_usd"], Value::Null);
    assert_eq!(merged["by_day"][0]["day"], "2026-09-27");
    assert_eq!(merged["by_day"][0]["usage"]["calls"], 3);
    assert_eq!(merged["sessions"].as_array().unwrap().len(), 3);
    assert_eq!(merged["by_day"][0]["models"].as_array().unwrap().len(), 2);
    assert_eq!(merged["by_day"][0]["models"][0]["model"], "gpt-5");
    assert_eq!(merged["by_day"][0]["models"][0]["usage"]["cost_usd"], 0.05);
    assert_eq!(
        merged["by_day"][0]["models"][1]["usage"]["cost_usd"],
        Value::Null
    );
    assert_eq!(merged["sessions"][0]["models"][0]["model"], "gpt-5");
}

#[test]
fn the_same_model_on_the_same_day_merges_across_chats() {
    let merged = merge(
        vec![
            summary("qwen", "2026-09-27", 2, 1_000, None),
            summary("qwen", "2026-09-27", 3, 2_000, Some(0.05)),
        ],
        30,
    );
    let models = merged["by_day"][0]["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["usage"], merged["by_day"][0]["usage"]);
}

#[test]
fn mixed_backend_versions_preserve_totals_without_a_partial_breakdown() {
    let mut legacy = summary("qwen", "2026-09-27", 2, 1_000, None);
    legacy["by_day"][0]
        .as_object_mut()
        .unwrap()
        .remove("models");
    let merged = merge(
        vec![legacy, summary("gpt-5", "2026-09-27", 1, 5_000, Some(0.05))],
        30,
    );
    assert_eq!(merged["by_day"][0]["usage"]["calls"], 3);
    assert!(merged["by_day"][0].get("models").is_none());
}
