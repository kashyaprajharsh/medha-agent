//! Memory for a desktop chat: what Medha remembers, where each memory came
//! from, and pin or forget. A change is appended to the event log under the
//! memory's lease before the projection is updated, exactly as the TUI does.

use kernel::{EventLog, Session};
use serde_json::{Value, json};
use std::sync::Arc;

fn store(
    memory: Option<&Arc<memory::MemoryProjection>>,
) -> Result<&Arc<memory::MemoryProjection>, String> {
    memory.ok_or_else(|| "Memory is off in this workspace".to_owned())
}

fn scope(params: &Value) -> Result<memory::Scope, String> {
    serde_json::from_value(params["scope"].clone())
        .map_err(|_| "Choose project or user memory".to_owned())
}

fn name(params: &Value) -> Result<String, String> {
    params["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Memory name required".to_owned())
}

fn view(entry: &memory::MemoryEntry) -> Value {
    json!({
        "name": entry.name,
        "claim": entry.claim,
        "description": entry.description,
        "kind": entry.kind,
        "scope": entry.scope,
        "trust": entry.trust,
        "confidence": entry.confidence,
        "pinned": entry.pinned,
        "sessions": entry.sessions.len(),
        "updated": entry.updated,
    })
}

pub(crate) async fn list(memory: Option<&Arc<memory::MemoryProjection>>) -> Result<Value, String> {
    let entries = store(memory)?
        .list_async()
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({"memories": entries.iter().map(view).collect::<Vec<_>>()}))
}

/// Pins, unpins or forgets one memory.
pub(crate) async fn change<L: EventLog>(
    log: &L,
    session: &Session,
    memory: Option<&Arc<memory::MemoryProjection>>,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    let store = store(memory)?;
    let (scope, name) = (scope(params)?, name(params)?);
    if store
        .get(scope, &name)
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Err("That memory no longer exists".into());
    }
    let op = match method {
        "memory.forget" => memory::MemoryOp::Forget {
            scope,
            name: name.clone(),
        },
        _ => memory::MemoryOp::Pin {
            scope,
            name: name.clone(),
            pinned: params["pinned"]
                .as_bool()
                .ok_or("pinned must be true or false")?,
        },
    };
    let _lease = log
        .acquire_mutation_lease(&format!("memory:{}:{name}", scope.as_str()))
        .await
        .map_err(|error| error.to_string())?;
    let payload = serde_json::to_value(&op).map_err(|error| error.to_string())?;
    log.append(kernel::Event::memory_write(session, payload))
        .await
        .map_err(|error| error.to_string())?;
    store
        .apply_async(&op)
        .await
        .map_err(|error| error.to_string())?;
    list(Some(store)).await
}

/// The event a memory was learned from, with the text around it.
pub(crate) async fn provenance<L: EventLog>(
    log: &L,
    memory: Option<&Arc<memory::MemoryProjection>>,
    params: &Value,
) -> Result<Value, String> {
    let entry = store(memory)?
        .get(scope(params)?, &name(params)?)
        .map_err(|error| error.to_string())?
        .ok_or("That memory no longer exists")?;
    for session in &entry.sessions {
        if let Some(event) = log
            .events(*session)
            .await
            .into_iter()
            .find(|event| entry.provenance.contains(&event.id))
        {
            let text = event
                .payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Ok(json!({
                "session": session.to_string(),
                "event": event.id.to_string(),
                "kind": event.kind.as_str(),
                "ts": event.ts,
                "excerpt": text.chars().take(600).collect::<String>(),
            }));
        }
    }
    Ok(json!({"session": null}))
}

#[cfg(test)]
#[path = "desktop_memory_tests.rs"]
mod tests;
