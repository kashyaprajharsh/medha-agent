//! Desktop rewind uses the existing event-log fork, rollback planner and
//! sandbox restore. The original conversation always remains in history.
use kernel::{EventKind, EventLog, Kernel, Message, Provider, Session};
use serde_json::{Value, json};
use std::sync::Arc;
use ulid::Ulid;

pub(crate) async fn points<P: Provider, L: EventLog>(
    kernel: &Kernel<P, L>,
    session: &Session,
    root: &std::path::Path,
) -> Result<Value, String> {
    let events = kernel
        .log
        .checked_events(session.id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"points": events.iter().filter(|event| event.is_from_person()).map(|event| json!({ "id": event.id.to_string(), "text": event.payload["text"].as_str().unwrap_or_default(), "files": kernel::rollback_plan_in(&events, event.id, root).len() })).collect::<Vec<_>>() }),
    )
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn rewind<P: Provider, L: EventLog>(
    kernel: &Kernel<P, L>,
    session: &mut Session,
    transcript: &mut Vec<Message>,
    resources: &crate::desktop_extensions::Runtime,
    agents: Option<&Arc<orchestrator::AgentControl>>,
    params: &Value,
) -> Result<Value, String> {
    let at = params["at"]
        .as_str()
        .and_then(|id| Ulid::from_string(id).ok())
        .ok_or("Choose a valid rewind point")?;
    let scope = params["scope"].as_str().ok_or("Choose what to rewind")?;
    if !["conversation", "code", "both"].contains(&scope) {
        return Err("Unknown rewind scope".into());
    }
    let _lease = kernel
        .log
        .acquire_mutation_lease("state:*")
        .await
        .map_err(|e| e.to_string())?;
    let events = kernel
        .log
        .checked_events(session.id)
        .await
        .map_err(|e| e.to_string())?;
    let index = kernel::cut_index(&events, at)
        .filter(|index| events[*index].kind == EventKind::UserMessage)
        .ok_or("The rewind point no longer exists")?;
    let selected = &events[index];
    let mut images = Vec::new();
    if scope != "code" {
        use base64::Engine;
        let media = serde_json::from_value(
            selected
                .payload
                .get("attachments")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .map_err(|e| format!("Saved image metadata is invalid: {e}"))?;
        let restored = crate::attachments::restage(media, kernel.artifacts.clone())
            .await
            .map_err(|e| e.to_string())?;
        for image in restored {
            let kernel::MediaSource::Artifact(hash) = &image.part.source else {
                return Err("Saved image has no artifact".into());
            };
            let bytes = kernel::artifacts::read_all(&kernel.artifacts, hash).await?;
            let normalized = media::normalize_for_transport(bytes, 2 * 1024 * 1024)
                .map_err(|e| e.to_string())?;
            let data = base64::engine::general_purpose::STANDARD.encode(normalized.bytes);
            images.push(json!({"id": Ulid::new().to_string(), "name": image.label, "mime": normalized.mime, "data": data, "note": normalized.note}));
        }
    }
    if agents.is_some_and(|control| !control.adopt(session.id)) {
        return Err("An agent is still starting. Rewind was not applied.".into());
    }
    let new_id = if scope != "code" {
        Some(
            kernel
                .log
                .fork(session.id, at)
                .await
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };
    let mut restored = 0;
    if scope != "conversation" {
        for file in kernel::rollback_plan_in(&events, at, resources.workspace.root()) {
            resources.workspace.restore(&file.path, file.snapshot.as_deref()).await.map_err(|error| format!("Restored {restored} files, then could not restore {}: {error}. Conversation was kept.", file.path))?;
            restored += 1;
        }
    }
    let source = session.id;
    if let Some(id) = new_id {
        let mut system = transcript
            .first()
            .cloned()
            .unwrap_or_else(|| Message::system(""));
        if let Some(memory) = &resources.memory {
            let fork_events = kernel
                .log
                .checked_events(id)
                .await
                .map_err(|e| e.to_string())?;
            memory
                .rebuild_project(fork_events.into_iter())
                .map_err(|e| e.to_string())?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_secs_f64())
                .unwrap_or_default();
            let block = memory::recall::compile_k3_configured(
                memory,
                resources.memory_budget,
                now,
                resources.memory_stale_days,
            )
            .map_err(|e| e.to_string())?;
            system.content = memory::recall::replace_k3(&system.content, &block);
        }
        if agents.is_some_and(|control| !control.adopt(id)) {
            return Err(
                "The rewind branch was saved, but an agent prevented switching to it".into(),
            );
        }
        transcript.clear();
        transcript.push(system);
        transcript.extend(kernel::project_messages(&events[..index]));
        session.id = id;
        session.done = false;
    }
    Ok(
        json!({"source": source.to_string(), "session": session.id.to_string(), "code_only": scope == "code", "restored": restored, "prefill": if scope == "code" { "" } else { selected.payload["text"].as_str().unwrap_or_default() }, "images": images }),
    )
}
