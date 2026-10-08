//! Byte-bounded verified visible history. One serialized record is cached only
//! while it is fragmented; concurrent loaders share a fixed memory budget.
use kernel::{EventKind, EventLog, Session};
use serde_json::json;

const RECORD_BYTES: usize = 32 * 1024 * 1024;

pub(crate) fn images(parts: &[kernel::MediaPart]) -> Vec<protocol::SavedImage> {
    parts
        .iter()
        .filter_map(|part| match &part.source {
            kernel::MediaSource::Artifact(hash) => Some(protocol::SavedImage {
                hash: hash.clone(),
                mime: part.mime_type.clone(),
                name: part.label.clone().unwrap_or_else(|| "image".into()),
            }),
            _ => None,
        })
        .collect()
}

pub(crate) async fn image(
    artifacts: &std::sync::Arc<dyn kernel::ArtifactStore>,
    request: protocol::ReadHistoryImage,
) -> Result<protocol::Image, String> {
    let bytes = kernel::artifacts::read_all(artifacts, &request.hash).await?;
    let image =
        tokio::task::spawn_blocking(move || media::normalize_for_transport(bytes, 2_200_000))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
    use base64::Engine;
    Ok(protocol::Image {
        id: request.hash,
        name: "saved image".into(),
        mime: image.mime.into(),
        data: base64::engine::general_purpose::STANDARD.encode(image.bytes),
        note: image.note,
    })
}
#[derive(Default)]
pub(crate) struct Reader {
    cached: Option<Cached>,
}

#[cfg(test)]
#[path = "chat_history_tests.rs"]
mod tests;
struct Cached {
    _bytes: tokio::sync::OwnedSemaphorePermit,
    expires: std::time::Instant,
    conversation: String,
    through: u64,
    position: u64,
    data: String,
}

impl Reader {
    pub(crate) fn has_cached(&self) -> bool {
        self.cached.is_some()
    }
    pub(crate) fn expire(&mut self, now: std::time::Instant) {
        if self
            .cached
            .as_ref()
            .is_some_and(|cached| cached.expires <= now)
        {
            self.cached = None;
        }
    }
    pub(crate) async fn read<L: EventLog>(
        &mut self,
        log: &L,
        session: &Session,
        request: protocol::ReadHistory,
        artifacts: &std::sync::Arc<dyn kernel::ArtifactStore>,
    ) -> Result<protocol::HistoryFragment, String> {
        self.expire(std::time::Instant::now());
        let conversation = session.id.to_string();
        if request
            .conversation
            .as_ref()
            .is_some_and(|named| named != &conversation)
        {
            return Err(
                "The conversation changed while its history was loading. Reload it.".into(),
            );
        }
        if request.through.is_none() && (request.after != 0 || request.offset != 0) {
            return Err("Start history at its beginning to establish a snapshot.".into());
        }
        if request.through.is_some() && request.conversation.is_none() {
            return Err("A continued history read must name its conversation.".into());
        }
        let cached = request.offset != 0
            && self.cached.as_ref().is_some_and(|cached| {
                cached.conversation == conversation
                    && Some(cached.through) == request.through
                    && cached.position == request.after
            });
        if !cached {
            let (head, event) = log
                .checked_history_record(
                    session.id,
                    request.after,
                    request.through,
                    request.offset != 0,
                )
                .await
                .map_err(|e| e.to_string())?;
            if request.after > head {
                return Err("History position is past the snapshot".into());
            }
            let Some((position, event)) = event else {
                if request.offset != 0 {
                    return Err("The history fragment no longer exists.".into());
                }
                self.cached = None;
                return Ok(protocol::HistoryFragment {
                    conversation,
                    through: head,
                    after: request.after,
                    offset: 0,
                    data: String::new(),
                    event_finished: true,
                    finished: true,
                    cursor: None,
                    live: None,
                });
            };
            let mut payload = event.payload;
            match event.kind {
                EventKind::ModelMessage => {
                    if let Some(parts) = payload["parts"].as_array_mut() {
                        parts.retain(|part| {
                            matches!(
                                part["type"].as_str(),
                                Some("text" | "tool_call" | "reasoning")
                            )
                        });
                        for part in parts {
                            if let Some(object) = part.as_object_mut() {
                                object.remove("provider_state");
                            }
                        }
                    }
                    if let Some(object) = payload.as_object_mut() {
                        object.retain(|key, _| matches!(key.as_str(), "role" | "parts"));
                    }
                }
                EventKind::ToolObs => {
                    if let Some(object) = payload.as_object_mut() {
                        object.remove("screen");
                    }
                    if let Some(object) = payload["payload"].as_object_mut() {
                        object.remove(kernel::events::AGENT_REPORT_ACKS_FIELD);
                        object.remove(kernel::events::TOOL_SCREEN_FIELD);
                    }
                }
                EventKind::UserMessage => {
                    if let Some(parts) = payload["attachments"].as_array_mut() {
                        for part in parts {
                            if let Some(object) = part.as_object_mut() {
                                object.remove("provider_state");
                            }
                            if part["source"]["kind"] == "artifact"
                                && let Some(hash) = part["source"]["value"].as_str()
                            {
                                match kernel::artifacts::read_all(artifacts, hash).await {
                                    Ok(bytes) => {
                                        use base64::Engine;
                                        part["source"] = json!({"kind":"base64", "value":
                                            base64::engine::general_purpose::STANDARD.encode(bytes)});
                                    }
                                    Err(error) => {
                                        part["source"] = json!(null);
                                        part["unavailable"] = json!(error);
                                    }
                                }
                            }
                        }
                    }
                }
                EventKind::ModelText | EventKind::ModelIntent | EventKind::ModelReasoning => {}
                _ => payload = json!(null),
            }
            let record = protocol::HistoryRecord {
                id: event.id.to_string(),
                kind: event.kind.as_str().to_owned(),
                payload,
                trust: format!("{:?}", event.trust),
                source: event.provenance.source,
            };
            let data = serde_json::to_string(&record).map_err(|e| e.to_string())?;
            if data.len() > RECORD_BYTES {
                return Err(
                    "A history record exceeds the replay limit (32 MiB). Nothing was deleted."
                        .into(),
                );
            }
            static CACHE_BYTES: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
                std::sync::OnceLock::new();
            let bytes = CACHE_BYTES
                .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(128 * 1024 * 1024)))
                .clone()
                .try_acquire_many_owned(data.len() as u32)
                .map_err(|_| "History loading is busy. Retry when another load finishes.")?;
            self.cached = Some(Cached {
                _bytes: bytes,
                expires: std::time::Instant::now() + std::time::Duration::from_secs(60),
                conversation: conversation.clone(),
                through: head,
                position,
                data,
            });
        }
        let cached = self.cached.as_ref().expect("a record was loaded");
        if request.offset > cached.data.len() || !cached.data.is_char_boundary(request.offset) {
            return Err("Invalid history fragment offset.".into());
        }
        let mut end = request
            .offset
            .saturating_add(256 * 1024)
            .min(cached.data.len());
        while !cached.data.is_char_boundary(end) {
            end -= 1;
        }
        let event_finished = end == cached.data.len();
        let page = protocol::HistoryFragment {
            conversation,
            through: cached.through,
            after: cached.position,
            offset: if event_finished { 0 } else { end },
            data: cached.data[request.offset..end].to_owned(),
            event_finished,
            finished: event_finished && cached.position == cached.through,
            cursor: None,
            live: None,
        };
        if event_finished {
            self.cached = None;
        }
        Ok(page)
    }
}
