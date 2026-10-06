//! What a window asks about a folder, answered by the backend: its history,
//! and typed settings and extension actions. These share the existing Medha
//! stores; credential values never enter replies. A request names a `method`,
//! such as `sessions.list` or `sessions.events` (`session_id`, optional
//! `cursor` and `limit`), and is answered with its result or its error.

use anyhow::Result;
use kernel::{ContentPart, Event, EventKind, EventLog, ModelMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use ulid::Ulid;

#[derive(Deserialize)]
struct Request {
    id: u64,
    method: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct Response {
    id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Response {
    fn ok(id: u64, result: impl Serialize) -> Self {
        Self {
            id,
            result: Some(serde_json::to_value(result).expect("serializable desktop response")),
            error: None,
        }
    }

    fn error(id: u64, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(message.into()),
        }
    }
}

#[derive(Serialize)]
struct SessionView {
    id: String,
    title: String,
    parent_id: Option<String>,
    started_ts: f64,
    last_ts: f64,
    events: u64,
}

#[derive(Serialize, Default)]
struct EventView {
    id: String,
    kind: &'static str,
    ts: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_label: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    child_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    /// A screen the tool's server offers for this result, and what it draws.
    #[serde(skip_serializing_if = "Option::is_none")]
    screen: Option<Value>,
    /// Structured parts retained in the log but omitted from this bounded view.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    omitted: Vec<&'static str>,
}

/// What one page of history may take on the wire, and so the most one event on it may.
const PAGE_BYTES: usize = wire::MAX_FRAME / 4;
const CUT: &str = "\n\n[Too large to show in full. The rest is left out.]";

impl EventView {
    /// What this takes on the wire, as it is written and not as it is stored:
    /// text can take six times its length once it is JSON.
    fn bytes(&self) -> usize {
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut count = Count(1);
        let _ = serde_json::to_writer(&mut count, self);
        count.0
    }

    /// With its size. An event too large for a page of its own is shown with
    /// its long parts cut short, and says so where it was cut.
    fn fitted(mut self) -> (Self, usize) {
        let mut keep = PAGE_BYTES / 8;
        loop {
            let bytes = self.bytes();
            if bytes <= PAGE_BYTES {
                return (self, bytes);
            }
            let text = [
                &mut self.text,
                &mut self.input,
                &mut self.summary,
                &mut self.output,
                &mut self.detail,
                &mut self.target,
                &mut self.file_path,
                &mut self.verb,
                &mut self.status,
                &mut self.tool_id,
                &mut self.child_id,
            ];
            for text in text.into_iter().flatten() {
                if text.len() > keep + CUT.len() {
                    let end = (0..=keep).rev().find(|end| text.is_char_boundary(*end));
                    text.truncate(end.unwrap_or(0));
                    text.push_str(CUT);
                }
            }
            // A plan, or what a tool's screen draws, is of no use in part.
            for (name, value) in [("plan", &mut self.plan), ("screen", &mut self.screen)] {
                if value
                    .as_ref()
                    .is_some_and(|value| value.to_string().len() > keep)
                {
                    *value = None;
                    self.omitted.push(name);
                }
            }
            if !self.omitted.is_empty() {
                let output = self.output.get_or_insert_with(String::new);
                if !output.ends_with(CUT) {
                    output.push_str(CUT);
                }
            }
            if keep == 0 {
                let bytes = self.bytes();
                return (self, bytes);
            }
            keep /= 2;
        }
    }
}

#[derive(Serialize)]
struct EventPage {
    events: Vec<EventView>,
    next_cursor: Option<String>,
}

/// One request about a folder, for a caller that already holds that folder's
/// event log: the backend keeps one connection per folder and answers on it.
pub(crate) async fn answer(
    log: &store::SqliteLog,
    workspace: &std::path::Path,
    frame: &Value,
) -> Result<Value, String> {
    let text = |key: &str| frame[key].as_str().map(str::to_owned);
    let request = Request {
        id: 0,
        method: text("method").unwrap_or_default(),
        session_id: text("session_id"),
        cursor: text("cursor"),
        limit: frame["limit"].as_u64().map(|limit| limit as usize),
        params: frame["params"].clone(),
    };
    let reply = handle(log, workspace, request).await;
    match reply.error {
        Some(error) => Err(error),
        None => Ok(reply.result.unwrap_or(Value::Null)),
    }
}

async fn handle(log: &store::SqliteLog, workspace: &std::path::Path, req: Request) -> Response {
    if req.method == "extensions.connectors" {
        return match super::config::load() {
            Ok(cfg) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|since| since.as_secs_f64())
                    .unwrap_or_default();
                let servers = cfg.map(|cfg| cfg.mcp).unwrap_or_default();
                Response::ok(
                    req.id,
                    crate::connectors::listing(log, workspace, &servers, now).await,
                )
            }
            Err(error) => Response::error(req.id, error.to_string()),
        };
    }
    if req.method.starts_with("extensions.")
        || req.method.starts_with("instructions.")
        || (req.method.starts_with("settings.") && req.method != "settings.defaults")
    {
        return match crate::desktop_preferences::handle(&req.method, &req.params, workspace).await {
            Ok(value) => Response::ok(req.id, value),
            Err(error) => Response::error(req.id, error.to_string()),
        };
    }
    match req.method.as_str() {
        "settings.defaults" => {
            let result = (|| -> Result<Value> {
                let cfg = super::config::load()?.unwrap_or_default();
                let lock =
                    lockfile::MedhaLock::load(workspace.join("medha.lock"))?.unwrap_or_default();
                Ok(json!({
                    "profiles": super::desktop_controls::profiles(&cfg),
                    "profile": cfg.startup_model(),
                    "model": cfg.startup_model().and_then(|name| cfg.model_profile(name)).map(|profile| profile.model.clone()),
                    "mode": std::env::var("MEDHA_MODE").unwrap_or(lock.policy.autonomy),
                    "reasoning": lock.reasoning.enabled.map_or("auto", |on| if on { "on" } else { "off" }),
                    "effort": lock.reasoning.effort.unwrap_or_else(|| "auto".into()),
                    "efforts": [], "reasoning_support": "unverified",
                    "streaming": lock.reasoning.stream.unwrap_or(true), "context_limit": Value::Null,
                }))
            })();
            match result {
                Ok(value) => Response::ok(req.id, value),
                Err(error) => Response::error(req.id, error.to_string()),
            }
        }
        "hello" => Response::ok(
            req.id,
            json!({
                "protocol_version": 1,
                "workspace": workspace.to_string_lossy(),
            }),
        ),
        "sessions.list" => match list_session_views(log) {
            Ok(sessions) => Response::ok(req.id, sessions),
            Err(error) => Response::error(req.id, error.to_string()),
        },
        "library.sessions" => {
            let prune_before = req.params["prune_before"].as_f64();
            let mut rows = Vec::new();
            for folder in library_chats(workspace) {
                let listed = chat_store(&folder)
                    .map(|events| list_session_views(&store::SqliteLog::open_read_only(events)?));
                let sessions = match listed {
                    Some(Ok(sessions)) => sessions,
                    None => Vec::new(),
                    Some(Err(error)) => {
                        eprintln!("skipping {}: {error:#}", folder.display());
                        continue;
                    }
                };
                if sessions.is_empty() {
                    if let Some(before) = prune_before {
                        prune_unused(&folder, before);
                    }
                    continue;
                }
                for session in sessions {
                    let mut row = json!(session);
                    row["folder"] = json!(folder);
                    rows.push(row);
                }
            }
            Response::ok(req.id, rows)
        }
        "library.usage" => {
            let days = req.params["days"].as_u64().unwrap_or(30).clamp(1, 365) as u32;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs_f64())
                .unwrap_or_default();
            let (mut calls, mut sessions) = (Vec::new(), HashMap::new());
            for events in library_chats(workspace)
                .iter()
                .filter_map(|folder| chat_store(folder))
            {
                // A chat left out would show a total that is too low as if it were right.
                let log = match store::SqliteLog::open(&events) {
                    Ok(log) => log,
                    Err(error) => {
                        let reason = format!("A chat's history could not be read: {error}");
                        return Response::error(req.id, reason);
                    }
                };
                let (more, known) = crate::usage_insights::collect(&log, days, now).await;
                calls.extend(more);
                sessions.extend(known);
            }
            Response::ok(
                req.id,
                crate::usage_insights::summarize(&calls, &sessions, days),
            )
        }
        "sessions.events" => {
            let Some(id) = req
                .session_id
                .as_deref()
                .and_then(|id| Ulid::from_string(id).ok())
            else {
                return Response::error(req.id, "valid session_id required");
            };
            match log.checked_events(id).await {
                Ok(events) if events.is_empty() => Response::error(req.id, "session not found"),
                Ok(events) => match page_events(&events, req.cursor.as_deref(), req.limit) {
                    Ok(page) => Response::ok(req.id, page),
                    Err(error) => Response::error(req.id, error),
                },
                Err(error) => Response::error(req.id, error.to_string()),
            }
        }
        "usage.summary" => {
            let days = req.params["days"].as_u64().unwrap_or(30).clamp(1, 365) as u32;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs_f64())
                .unwrap_or_default();
            let (calls, sessions) = crate::usage_insights::collect(log, days, now).await;
            Response::ok(
                req.id,
                crate::usage_insights::summarize(&calls, &sessions, days),
            )
        }
        "sessions.changes" => {
            let Some(id) = req
                .session_id
                .as_deref()
                .and_then(|id| Ulid::from_string(id).ok())
            else {
                return Response::error(req.id, "valid session_id required");
            };
            match log.checked_events(id).await {
                Ok(events) => Response::ok(
                    req.id,
                    json!({ "files": crate::desktop_changes::session_changes(&events) }),
                ),
                Err(error) => Response::error(req.id, error.to_string()),
            }
        }
        _ => Response::error(req.id, "unknown method"),
    }
}

/// The Personal library keeps each chat in its own folder; one process reads them all.
fn library_chats(library: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(library) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("chat-"))
        .filter_map(|entry| entry.path().canonicalize().ok())
        .filter(|folder| folder.is_dir() && folder.starts_with(library))
        .collect()
}

/// Where a chat's history is kept, or `None` for one that never started.
fn chat_store(folder: &std::path::Path) -> Option<PathBuf> {
    let events = super::config::existing_state_dir(folder)?.join("events.db");
    events.is_file().then_some(events)
}

/// A chat with no sessions and an empty folder, untouched since `before`, was never used.
fn prune_unused(folder: &std::path::Path, before: f64) {
    let untouched = std::fs::metadata(folder)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|at| at.as_secs_f64() < before);
    if !untouched {
        return;
    }
    // Found first: the lookup needs the folder. `remove_dir` refuses one that is not empty.
    let state = super::config::existing_state_dir(folder);
    if std::fs::remove_dir(folder).is_ok()
        && let Some(state) = state
    {
        let _ = std::fs::remove_dir_all(state);
    }
}

fn list_session_views(log: &store::SqliteLog) -> Result<Vec<SessionView>> {
    let sessions = log.list_sessions()?;
    let session_ids = sessions
        .iter()
        .map(|session| session.id)
        .collect::<std::collections::HashSet<_>>();
    let mut parents = HashMap::new();
    for (session, payload) in log.agent_spawns()? {
        let Some(child) = payload
            .get("child")
            .and_then(Value::as_str)
            .and_then(|id| Ulid::from_string(id).ok())
        else {
            continue;
        };
        if session_ids.contains(&session) && session_ids.contains(&child) && child != session {
            let name = payload
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or("Subagent");
            parents
                .entry(child)
                .or_insert_with(|| (session, name.to_owned()));
        }
    }
    Ok(sessions
        .into_iter()
        .map(|session| {
            let (parent_id, title) = match parents.get(&session.id) {
                Some((parent, name)) => (Some(parent.to_string()), name.clone()),
                None => (None, session.title),
            };
            SessionView {
                id: session.id.to_string(),
                title,
                parent_id,
                started_ts: session.started_ts,
                last_ts: session.last_ts,
                events: session.events,
            }
        })
        .collect())
}

fn page_events(
    events: &[Event],
    cursor: Option<&str>,
    limit: Option<usize>,
) -> std::result::Result<EventPage, String> {
    let start = match cursor {
        Some(cursor) => events
            .iter()
            .position(|event| event.id.to_string() == cursor)
            .map(|position| position + 1)
            .ok_or_else(|| "invalid event cursor".to_string())?,
        None => 0,
    };
    let end = start
        .saturating_add(limit.unwrap_or(200).clamp(1, 500))
        .min(events.len());
    // A legacy text event may fall on the preceding page while its canonical
    // message falls on this one. Keep that small bit of projection state.
    let mut text_before_canonical = events[..start]
        .iter()
        .rev()
        .find_map(|event| match event.kind {
            EventKind::ModelText => Some(true),
            EventKind::ModelMessage | EventKind::UserMessage => Some(false),
            _ => None,
        })
        .unwrap_or(false);
    let handoffs = handoff_messages(events);
    let outcomes = agent_outcomes(events);
    let mut steps = transcript_view::Steps::default();
    for event in &events[..start] {
        tool_view(&mut steps, event);
    }
    let mut view = |offset: usize, event: &Event| match event.kind {
        EventKind::ModelReasoning => {
            let text = event.payload.get("text")?.as_str()?.trim().to_owned();
            let since = (start + offset)
                .checked_sub(1)
                .map(|previous| events[previous].ts);
            (!text.is_empty()).then(|| EventView {
                duration_ms: since.map(|since| ((event.ts - since).max(0.0) * 1000.0) as u64),
                ..event_view(event, "reasoning", text)
            })
        }
        EventKind::UserMessage => {
            text_before_canonical = false;
            if let Some((ok, summary, output)) = verifier_result(event) {
                return Some(EventView {
                    status: Some(if ok { "passed" } else { "failed" }.into()),
                    output: (!output.is_empty()).then_some(output),
                    ..event_view(event, "verification", summary)
                });
            }
            let kind = if handoffs.contains(&event.id) {
                "handoff"
            } else if event.is_from_person() {
                "user"
            } else {
                return None;
            };
            Some(event_view(
                event,
                kind,
                event.payload.get("text")?.as_str()?.to_owned(),
            ))
        }
        EventKind::ModelText => {
            text_before_canonical = true;
            Some(event_view(
                event,
                "assistant",
                event.payload.get("text")?.as_str()?.to_owned(),
            ))
        }
        EventKind::ModelMessage => {
            let skip = text_before_canonical;
            text_before_canonical = false;
            if skip {
                None
            } else {
                let message: ModelMessage = serde_json::from_value(event.payload.clone()).ok()?;
                let text = message
                    .parts
                    .into_iter()
                    .filter_map(|part| match part {
                        ContentPart::Text(text) => Some(text.text),
                        _ => None,
                    })
                    .collect::<String>();
                (!text.is_empty()).then(|| event_view(event, "assistant", text))
            }
        }
        EventKind::ModelIntent | EventKind::ToolObs => tool_view(&mut steps, event),
        EventKind::AgentSpawned => {
            let child = event.payload.get("child")?.as_str()?.to_owned();
            let agent = event.payload.get("agent").and_then(Value::as_str);
            let outcome = event
                .payload
                .get("dispatch")
                .and_then(Value::as_str)
                .and_then(|dispatch| outcomes.get(dispatch));
            Some(EventView {
                child_id: Some(child),
                detail: event
                    .payload
                    .get("objective")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                status: outcome.map(|(status, _)| status.clone()),
                duration_ms: outcome.and_then(|(_, duration)| *duration),
                ..event_view(event, "subagent", agent.unwrap_or("sub-agent").to_owned())
            })
        }
        _ => None,
    };
    // A page is bounded in bytes as well as in events. No event is larger than
    // a page, so a page is never left empty by its size, nor made too large by one event.
    let (mut visible, mut bytes, mut end) = (Vec::new(), 0, end);
    for (offset, event) in events[start..end].iter().enumerate() {
        let Some((shown, size)) = view(offset, event).map(EventView::fitted) else {
            continue;
        };
        bytes += size;
        if bytes > PAGE_BYTES && !visible.is_empty() {
            end = start + offset;
            break;
        }
        visible.push(shown);
    }
    Ok(EventPage {
        events: visible,
        next_cursor: (end < events.len()).then(|| events[end - 1].id.to_string()),
    })
}

fn event_view(event: &Event, kind: &'static str, text: String) -> EventView {
    EventView {
        id: event.id.to_string(),
        kind,
        ts: event.ts,
        text: (!text.is_empty()).then_some(text),
        ..EventView::default()
    }
}

/// The completion check's outcome. Older sessions predate the `verifier` field
/// and carry only the kernel's own `[verifier] PASS|FAIL — summary` line.
fn verifier_result(event: &Event) -> Option<(bool, String, String)> {
    if let Some(result) = event.payload.get("verifier") {
        let text = |key: &str| {
            result
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        return Some((
            result.get("ok") == Some(&Value::Bool(true)),
            text("summary"),
            text("output"),
        ));
    }
    if event.trust == kernel::TrustLabel::User {
        return None;
    }
    let text = event.payload.get("text")?.as_str()?;
    let rest = text.strip_prefix("[verifier] ")?;
    let (verdict, rest) = rest.split_once(" — ")?;
    let ok = match verdict {
        "PASS" => true,
        "FAIL" => false,
        _ => return None,
    };
    let (summary, output) = rest.split_once('\n').unwrap_or((rest, ""));
    Some((ok, summary.to_owned(), output.trim_end().to_owned()))
}

fn tool_view(steps: &mut transcript_view::Steps, event: &Event) -> Option<EventView> {
    let field = |key: &str| event.payload.get(key).and_then(Value::as_str);
    let value = |key: &str| event.payload.get(key).unwrap_or(&Value::Null);
    match event.kind {
        EventKind::ModelIntent => {
            let (id, tool) = (field("id")?, field("tool")?);
            let call = steps.call(id, tool, value("args"));
            Some(EventView {
                tool_id: Some(id.to_owned()),
                verb: Some(call.verb),
                target: call.target,
                file_path: call.file_path,
                input_label: call.input_label,
                input: call.input,
                plan: call.plan,
                ..event_view(event, "tool_call", tool.to_owned())
            })
        }
        EventKind::ToolObs => {
            let (id, status) = (field("intent_id")?, field("status")?);
            let outcome = steps.result(id, "", status == "ok", value("payload"));
            Some(EventView {
                tool_id: Some(id.to_owned()),
                summary: outcome.summary,
                output: outcome.output,
                detail: outcome.detail,
                error_code: outcome.error_code,
                status: Some(status.to_owned()),
                screen: event.payload.get("screen").cloned(),
                ..event_view(event, "tool_result", String::new())
            })
        }
        _ => None,
    }
}

/// A child session opens with its own `agent.spawned`. Every user message after
/// it — the handoff prompt and any later steer — was written by the parent
/// agent, not the person.
fn handoff_messages(events: &[Event]) -> HashSet<Ulid> {
    let mut handoffs = HashSet::new();
    let mut in_child = false;
    for event in events {
        match event.kind {
            EventKind::AgentSpawned if event.payload.get("child").is_none() => in_child = true,
            EventKind::UserMessage if in_child => {
                handoffs.insert(event.id);
            }
            _ => {}
        }
    }
    handoffs
}

fn agent_outcomes(events: &[Event]) -> HashMap<String, (String, Option<u64>)> {
    events
        .iter()
        .filter_map(|event| {
            let fallback = match event.kind {
                EventKind::AgentCompleted => "completed",
                EventKind::AgentFailed => "failed",
                EventKind::AgentCancelled => "cancelled",
                _ => return None,
            };
            let dispatch = event.payload.get("dispatch")?.as_str()?.to_owned();
            let status = event
                .payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or(fallback)
                .to_owned();
            let duration = event.payload.get("duration_ms").and_then(Value::as_u64);
            Some((dispatch, (status, duration)))
        })
        .collect()
}

#[cfg(test)]
#[path = "desktop_service_tests.rs"]
mod tests;
