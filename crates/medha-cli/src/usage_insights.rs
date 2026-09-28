//! Tokens and cost from the event log, shared by the TUI's `/usage` and the
//! desktop Usage page. Each model call's `context.usage` record carries its
//! tokens and, when the model's price was known, its cost; calls recorded
//! before that carry prompt tokens only and are counted, never guessed.

use kernel::{Event, EventKind, EventLog};
use serde_json::{Value, json};
use std::collections::HashMap;
use ulid::Ulid;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Call {
    pub session: Ulid,
    pub ts: f64,
    pub identity: String,
    pub prompt: u64,
    pub completion: Option<u64>,
    pub cached: Option<u64>,
    pub cost: Option<f64>,
}

/// The model calls one session made, and the sub-agent sessions it started.
pub(crate) fn calls(events: &[Event]) -> (Vec<Call>, Vec<Ulid>) {
    let mut calls = Vec::new();
    let mut children = Vec::new();
    for event in events {
        match event.kind {
            EventKind::ContextUsage => {
                let number = |key: &str| event.payload.get(key).and_then(Value::as_u64);
                calls.push(Call {
                    session: event.session_id,
                    ts: event.ts,
                    identity: event.payload["identity"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    prompt: number("prompt_tokens").unwrap_or(0),
                    completion: number("completion_tokens"),
                    cached: number("cached_prompt_tokens"),
                    cost: event.payload.get("cost_usd").and_then(Value::as_f64),
                });
            }
            EventKind::AgentSpawned => {
                if let Some(child) = event.payload["child"]
                    .as_str()
                    .and_then(|id| Ulid::from_string(id).ok())
                {
                    children.push(child);
                }
            }
            _ => {}
        }
    }
    (calls, children)
}

/// Reads every session active in the last `days` days.
pub(crate) async fn collect<L: EventLog>(
    log: &L,
    days: u32,
    now: f64,
) -> (Vec<Call>, HashMap<Ulid, (String, Option<Ulid>)>) {
    let since = now - f64::from(days) * 86_400.0;
    let mut all = Vec::new();
    let mut sessions = HashMap::new();
    let mut parents = HashMap::new();
    for meta in log.sessions().await {
        if meta.last_ts < since {
            continue;
        }
        let (found, children) = calls(&log.events(meta.id).await);
        for child in children {
            parents.insert(child, meta.id);
        }
        all.extend(found.into_iter().filter(|call| call.ts >= since));
        sessions.insert(meta.id, meta.title.clone());
    }
    let sessions = sessions
        .into_iter()
        .map(|(id, title)| (id, (title, parents.get(&id).copied())))
        .collect();
    (all, sessions)
}

/// A chat's usage, the last time it made a call, and the sub-agents counted in it.
type SessionTally = (Total, f64, std::collections::HashSet<Ulid>);

#[derive(Default)]
struct Total {
    calls: u64,
    prompt: u64,
    completion: u64,
    cached: u64,
    cost: f64,
    priced: u64,
}

impl Total {
    fn add(&mut self, call: &Call) {
        self.calls += 1;
        self.prompt += call.prompt;
        self.completion += call.completion.unwrap_or(0);
        self.cached += call.cached.unwrap_or(0);
        if let Some(cost) = call.cost {
            self.cost += cost;
            self.priced += 1;
        }
    }
    fn view(&self) -> Value {
        json!({
            "calls": self.calls,
            "prompt_tokens": self.prompt,
            "completion_tokens": self.completion,
            "cached_tokens": self.cached,
            "cost_usd": (self.priced > 0).then_some(self.cost),
            "unpriced_calls": self.calls - self.priced,
        })
    }
}

/// The model part of `protocol:endpoint:model`; an endpoint's own colons
/// (scheme, port) sit before its path, so the last colon after the path wins.
pub(crate) fn model_label(identity: &str) -> String {
    let rest = identity.split_once(':').map_or(identity, |(_, rest)| rest);
    let path = rest.find("://").map_or(0, |at| at + 3);
    let after_host = rest[path..].find('/').map_or(path, |slash| path + slash);
    rest[after_host..]
        .find(':')
        .map(|colon| rest[after_host + colon + 1..].to_owned())
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| rest.to_owned())
}

/// Totals, and the same by model, by session (sub-agents counted in their
/// parent) and by local day.
pub(crate) fn summarize(
    calls: &[Call],
    sessions: &HashMap<Ulid, (String, Option<Ulid>)>,
    days: u32,
) -> Value {
    let root = |mut id: Ulid| {
        for _ in 0..16 {
            match sessions.get(&id).and_then(|(_, parent)| *parent) {
                Some(parent) => id = parent,
                None => break,
            }
        }
        id
    };
    let mut total = Total::default();
    let mut models: HashMap<String, Total> = HashMap::new();
    let mut by_session: HashMap<Ulid, SessionTally> = HashMap::new();
    let mut by_day: HashMap<String, Total> = HashMap::new();
    for call in calls {
        total.add(call);
        models
            .entry(model_label(&call.identity))
            .or_default()
            .add(call);
        let owner = root(call.session);
        let entry = by_session.entry(owner).or_default();
        entry.0.add(call);
        entry.1 = entry.1.max(call.ts);
        if owner != call.session {
            entry.2.insert(call.session);
        }
        let day = chrono::DateTime::from_timestamp(call.ts as i64, 0)
            .map(|time| {
                time.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .unwrap_or_default();
        by_day.entry(day).or_default().add(call);
    }
    let tokens = |total: &Total| total.prompt + total.completion;
    let mut models: Vec<(String, Total)> = models.into_iter().collect();
    models.sort_by_key(|(_, total)| std::cmp::Reverse(tokens(total)));
    let mut sessions_view: Vec<(Ulid, SessionTally)> = by_session.into_iter().collect();
    sessions_view.sort_by_key(|(_, (total, _, _))| std::cmp::Reverse(tokens(total)));
    let mut days_view: Vec<(String, Total)> = by_day.into_iter().collect();
    days_view.sort_by(|a, b| a.0.cmp(&b.0));
    json!({
        "days": days,
        "total": total.view(),
        "models": models.iter().map(|(model, total)| json!({"model": model, "usage": total.view()})).collect::<Vec<_>>(),
        "sessions": sessions_view.iter().take(20).map(|(id, (total, last, agents))| json!({
            "id": id.to_string(),
            "title": sessions.get(id).map(|(title, _)| title.as_str()).unwrap_or(""),
            "last_ts": last,
            "agents": agents.len(),
            "usage": total.view(),
        })).collect::<Vec<_>>(),
        "by_day": days_view.iter().map(|(day, total)| json!({"day": day, "usage": total.view()})).collect::<Vec<_>>(),
    })
}

/// Plain text for the TUI: this session, then the window by model.
pub(crate) fn render(session: &Value, window: &Value) -> String {
    let line = |usage: &Value| {
        let cost = usage["cost_usd"]
            .as_f64()
            .map_or_else(|| "cost unknown".to_owned(), |cost| format!("${cost:.4}"));
        let unpriced = usage["unpriced_calls"].as_u64().unwrap_or(0);
        format!(
            "{} calls · {} in ({} cached) · {} out · {cost}{}",
            usage["calls"],
            tokens(usage["prompt_tokens"].as_u64().unwrap_or(0)),
            tokens(usage["cached_tokens"].as_u64().unwrap_or(0)),
            tokens(usage["completion_tokens"].as_u64().unwrap_or(0)),
            if unpriced > 0 && usage["cost_usd"].is_number() {
                format!(" (+{unpriced} unpriced)")
            } else {
                String::new()
            },
        )
    };
    let mut text = format!("usage — this session\n  {}\n", line(&session["total"]));
    text.push_str(&format!(
        "\nlast {} days\n  {}\n",
        window["days"],
        line(&window["total"])
    ));
    for model in window["models"].as_array().into_iter().flatten() {
        text.push_str(&format!(
            "  {} — {}\n",
            model["model"].as_str().unwrap_or("?"),
            line(&model["usage"])
        ));
    }
    text.push_str("\nCalls from before usage was recorded count prompt tokens only.");
    text
}

fn tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{:.1}k", count as f64 / 1_000.0),
        _ => format!("{:.2}M", count as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
#[path = "usage_insights_tests.rs"]
mod tests;
