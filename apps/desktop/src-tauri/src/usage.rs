//! Usage across a workspace. A project has one store; the Personal library keeps
//! each chat in its own folder, so its summaries are read one by one and added.

use serde_json::{Map, Value, json};

const USAGE_KEYS: [&str; 4] = [
    "calls",
    "prompt_tokens",
    "completion_tokens",
    "cached_tokens",
];

fn add_usage(into: &mut Value, from: &Value) {
    for key in USAGE_KEYS.iter().chain(["unpriced_calls"].iter()) {
        let sum = into[*key].as_u64().unwrap_or(0) + from[*key].as_u64().unwrap_or(0);
        into[*key] = json!(sum);
    }
    into["cost_usd"] = match (into["cost_usd"].as_f64(), from["cost_usd"].as_f64()) {
        (None, None) => Value::Null,
        (a, b) => json!(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    };
}

fn tokens(row: &Value) -> u64 {
    row["usage"]["prompt_tokens"].as_u64().unwrap_or(0)
        + row["usage"]["completion_tokens"].as_u64().unwrap_or(0)
}

/// Adds summaries from separate stores into one, keyed the same way.
pub fn merge(summaries: Vec<Value>, days: u64) -> Value {
    let mut total = json!({"cost_usd": null});
    let mut models: Map<String, Value> = Map::new();
    let mut by_day: Map<String, Value> = Map::new();
    let mut sessions = Vec::new();
    for summary in summaries {
        add_usage(&mut total, &summary["total"]);
        for (rows, key, target) in [
            (&summary["models"], "model", &mut models),
            (&summary["by_day"], "day", &mut by_day),
        ] {
            for row in rows.as_array().into_iter().flatten() {
                let name = row[key].as_str().unwrap_or_default().to_owned();
                let slot = target
                    .entry(name.clone())
                    .or_insert_with(|| json!({key: name, "usage": {"cost_usd": null}}));
                add_usage(&mut slot["usage"], &row["usage"]);
            }
        }
        sessions.extend(summary["sessions"].as_array().cloned().unwrap_or_default());
    }
    let mut models: Vec<Value> = models.into_values().collect();
    models.sort_by_key(|row| std::cmp::Reverse(tokens(row)));
    sessions.sort_by_key(|row| std::cmp::Reverse(tokens(row)));
    sessions.truncate(20);
    json!({
        "days": days,
        "total": total,
        "models": models,
        "sessions": sessions,
        "by_day": by_day.into_values().collect::<Vec<_>>(),
    })
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
