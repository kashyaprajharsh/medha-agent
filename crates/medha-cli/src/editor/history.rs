//! Replay visible durable records without letting compaction checkpoints erase
//! the editor's history. Compatibility text/intent events are coalesced with
//! their canonical message, including across page boundaries.
use super::{session::emit, text};
use crate::chat::Writer;
use medha_client::View;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const RECORD_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
struct Replay {
    text: Option<String>,
    reasoning: Option<String>,
    canonical_calls: HashSet<String>,
    admissions: HashMap<String, [u8; 32]>,
}

impl Replay {
    async fn flush(&mut self, writer: &Writer, session: &str) {
        if let Some(content) = self.reasoning.take() {
            text(writer, session, "agent_thought_chunk", &content).await;
        }
        if let Some(content) = self.text.take() {
            text(writer, session, "agent_message_chunk", &content).await;
        }
    }

    async fn record(&mut self, writer: &Writer, session: &str, record: protocol::HistoryRecord) {
        let payload = record.payload;
        match record.kind.as_str() {
            "user.message" => {
                self.flush(writer, session).await;
                self.canonical_calls.clear();
                let fingerprint: [u8; 32] = Sha256::digest(
                    serde_json::to_vec(&json!({"payload":{
                    "text":payload["text"],"attachments":payload["attachments"]},
                    "trust":record.trust,"source":record.source}))
                    .expect("history serializes"),
                )
                .into();
                let retry = payload["retry_of"]
                    .as_str()
                    .and_then(|id| self.admissions.get(id))
                    == Some(&fingerprint);
                // Retries belong to the current admission. Bound memory even
                // for adversarial historic retry chains; unknown references
                // are displayed rather than silently losing an input.
                if payload["retry_of"].is_null() || self.admissions.len() >= 1024 {
                    self.admissions.clear();
                }
                self.admissions.insert(record.id, fingerprint);
                if !retry {
                    text(
                        writer,
                        session,
                        "user_message_chunk",
                        payload["text"].as_str().unwrap_or_default(),
                    )
                    .await;
                    if let Some(attachments) = payload["attachments"].as_array() {
                        for attachment in attachments {
                            writer.wait_for_room().await;
                            if attachment["source"]["kind"] == "base64" {
                                super::update(
                                    writer,
                                    session,
                                    json!({"sessionUpdate":"user_message_chunk",
                                    "content":{"type":"image","mimeType":attachment["mime_type"],
                                        "data":attachment["source"]["value"]}}),
                                );
                            } else {
                                text(
                                    writer,
                                    session,
                                    "user_message_chunk",
                                    &format!(
                                        "\n[image unavailable: {} — {}]\n",
                                        attachment["label"].as_str().unwrap_or("image"),
                                        attachment["unavailable"]
                                            .as_str()
                                            .unwrap_or("no saved image bytes")
                                    ),
                                )
                                .await;
                            }
                        }
                    }
                }
            }
            "model.text" => {
                if let Some(content) = self.text.take() {
                    text(writer, session, "agent_message_chunk", &content).await;
                }
                self.text = payload["text"].as_str().map(str::to_owned);
            }
            "model.reasoning" => {
                self.flush(writer, session).await;
                self.reasoning = payload["text"].as_str().map(str::to_owned);
            }
            "model.message" => {
                let parts = payload["parts"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let canonical_text: String = parts
                    .iter()
                    .filter(|part| part["type"] == "text")
                    .filter_map(|part| part["text"].as_str())
                    .collect();
                let canonical_reasoning: String = parts
                    .iter()
                    .filter(|part| part["type"] == "reasoning")
                    .filter_map(|part| part["text"].as_str())
                    .collect();
                if self.reasoning.as_ref() == Some(&canonical_reasoning) {
                    self.reasoning = None;
                } else if let Some(content) = self.reasoning.take() {
                    text(writer, session, "agent_thought_chunk", &content).await;
                }
                if self.text.as_ref() == Some(&canonical_text) {
                    self.text = None;
                } else {
                    self.flush(writer, session).await;
                }
                self.canonical_calls.clear();
                for part in parts {
                    match part["type"].as_str() {
                        Some("text") => {
                            text(
                                writer,
                                session,
                                "agent_message_chunk",
                                part["text"].as_str().unwrap_or_default(),
                            )
                            .await
                        }
                        Some("reasoning") => {
                            text(
                                writer,
                                session,
                                "agent_thought_chunk",
                                part["text"].as_str().unwrap_or_default(),
                            )
                            .await
                        }
                        Some("tool_call") => {
                            if let Some(id) = part["id"].as_str() {
                                self.canonical_calls.insert(id.to_owned());
                                emit(
                                    writer,
                                    session,
                                    protocol::TurnEvent::ToolCall {
                                        id: Some(id.into()),
                                        tool: part["tool"].as_str().unwrap_or("tool").into(),
                                        args: part["args"].clone(),
                                    },
                                )
                                .await;
                            }
                        }
                        _ => {}
                    }
                }
            }
            "model.intent" => {
                let id = payload["id"].as_str().unwrap_or_default();
                if !self.canonical_calls.contains(id) {
                    self.flush(writer, session).await;
                    emit(
                        writer,
                        session,
                        protocol::TurnEvent::ToolCall {
                            id: Some(id.into()),
                            tool: payload["tool"].as_str().unwrap_or("tool").into(),
                            args: payload["args"].clone(),
                        },
                    )
                    .await;
                }
            }
            "tool.observation" => {
                self.flush(writer, session).await;
                emit(
                    writer,
                    session,
                    protocol::TurnEvent::ToolResult {
                        id: payload["intent_id"].as_str().map(str::to_owned),
                        tool: payload["tool"].as_str().unwrap_or("tool").into(),
                        ok: payload["status"] == "ok",
                        payload: payload["payload"].clone(),
                    },
                )
                .await;
            }
            _ => {}
        }
    }
}

pub(super) async fn replay(
    view: &mut View,
    writer: &Writer,
    session: &str,
) -> Result<protocol::PresentationSnapshot, String> {
    let mut request = protocol::ReadHistory::default();
    let mut replay = Replay::default();
    let mut record = String::new();
    let mut live = None;
    loop {
        let page: protocol::HistoryFragment = view.call(&request).await?;
        if request.through.is_none() {
            if let Some(cursor) = page.cursor {
                view.covered_through(cursor)?;
            }
            live = page.live;
        }
        if record.len().saturating_add(page.data.len()) > RECORD_BYTES {
            return Err(
                "A history record exceeds the editor replay limit (32 MiB). Nothing was deleted."
                    .into(),
            );
        }
        record.push_str(&page.data);
        if page.event_finished && !record.is_empty() {
            writer.wait_for_room().await;
            let parsed: protocol::HistoryRecord =
                serde_json::from_str(&record).map_err(|_| "Invalid history projection")?;
            replay.record(writer, session, parsed).await;
            record.clear();
        }
        if page.finished {
            break;
        }
        request = protocol::ReadHistory {
            after: page.after,
            through: Some(page.through),
            offset: page.offset,
            conversation: Some(page.conversation),
        };
    }
    writer.wait_for_room().await;
    replay.flush(writer, session).await;
    if let Some(items) = live {
        for item in items {
            super::session::presentation(writer, session, item, view).await;
        }
    }
    view.call(&protocol::GetPresentation {}).await
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
