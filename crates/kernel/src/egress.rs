//! Checks model-authored outbound content after a chat has read web text.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    sync::{Arc, Mutex},
};

pub const FILE_CHUNK: usize = 256;
const FILE_BYTES: usize = 8 * 1024 * 1024;
const BLOCKS: usize = 64;

#[derive(Default)]
pub struct Guard {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    files: Vec<String>,
    file_ids: HashSet<[u8; 32]>,
    bytes: usize,
    file_secrets: Vec<String>,
    secret_bytes: usize,
    secret_full: bool,
    blocked: BTreeMap<String, Blocked>,
}

struct Blocked {
    review: Value,
    allowed: bool,
    answered: bool,
}

struct Active {
    guard: Arc<Guard>,
    tainted: bool,
    notice: Arc<Mutex<Option<Value>>>,
}

tokio::task_local! { static ACTIVE: Active; }

pub async fn scope<T>(
    guard: Arc<Guard>,
    tainted: bool,
    future: impl Future<Output = T>,
) -> (T, Option<Value>) {
    let notice = Arc::new(Mutex::new(None));
    let output = ACTIVE
        .scope(
            Active {
                guard,
                tainted,
                notice: notice.clone(),
            },
            future,
        )
        .await;
    let review = notice.lock().unwrap().take();
    (output, review)
}

pub fn check(destination: &str, payload: &Value) -> Result<(), String> {
    ACTIVE
        .try_with(|active| {
            if !active.tainted {
                return Ok(());
            }
            active.guard.check(destination, payload, &active.notice)
        })
        .unwrap_or(Ok(()))
}

pub fn peek(destination: &str, payload: &Value) -> Result<(), String> {
    looked(destination, payload, false, true)
}

/// For a server the person set up themselves: only real secrets are held back.
/// Sending file text there, such as a snippet for an issue, is ordinary work.
pub fn check_tool(destination: &str, payload: &Value) -> Result<(), String> {
    looked(destination, payload, true, false)
}

pub fn peek_tool(destination: &str, payload: &Value) -> Result<(), String> {
    looked(destination, payload, false, false)
}

fn looked(destination: &str, payload: &Value, consume: bool, files: bool) -> Result<(), String> {
    ACTIVE
        .try_with(|active| {
            if !active.tainted {
                return Ok(());
            }
            let notice = &active.notice;
            active
                .guard
                .inspect(destination, payload, notice, consume, files)
        })
        .unwrap_or(Ok(()))
}

pub fn file_read(content: &str) {
    let _ = ACTIVE.try_with(|active| active.guard.file_read(content));
}

impl Guard {
    pub fn file_read(&self, content: &str) {
        let id: [u8; 32] = Sha256::digest(content.as_bytes()).into();
        let mut state = self.state.lock().unwrap();
        if state.file_ids.contains(&id) {
            return;
        }
        for value in crate::redact::file_values(content) {
            if state.file_secrets.contains(&value) {
                continue;
            }
            if state.file_secrets.len() >= 4096
                || state.secret_bytes.saturating_add(value.len()) > 1024 * 1024
            {
                state.secret_full = true;
                break;
            }
            state.secret_bytes += value.len();
            state.file_secrets.push(value);
        }
        state
            .file_secrets
            .sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        if content.len() < FILE_CHUNK || content.len() > FILE_BYTES {
            return;
        }
        // Past the limit the oldest reads are forgotten; outbound work is never blocked for it.
        while state.bytes.saturating_add(content.len()) > FILE_BYTES && !state.files.is_empty() {
            let oldest = state.files.remove(0);
            state.bytes -= oldest.len();
            let oldest: [u8; 32] = Sha256::digest(oldest.as_bytes()).into();
            state.file_ids.remove(&oldest);
        }
        state.bytes += content.len();
        state.files.push(content.to_owned());
        state.file_ids.insert(id);
    }

    pub fn restore(&self, events: &[crate::Event]) {
        for event in events {
            if event.kind == crate::EventKind::ToolObs
                && event.payload["tool"] == "read"
                && event.trust != crate::TrustLabel::Web
                && let Some(content) = event.payload["payload"]["content"].as_str()
            {
                self.file_read(content);
            }
        }
    }

    fn check(
        &self,
        destination: &str,
        payload: &Value,
        notice: &Mutex<Option<Value>>,
    ) -> Result<(), String> {
        self.inspect(destination, payload, notice, true, true)
    }

    fn inspect(
        &self,
        destination: &str,
        payload: &Value,
        notice: &Mutex<Option<Value>>,
        consume: bool,
        files: bool,
    ) -> Result<(), String> {
        let encoded = serde_json::to_string(payload).map_err(|error| error.to_string())?;
        let mut digest = Sha256::new();
        digest.update(destination.len().to_le_bytes());
        digest.update(destination.as_bytes());
        digest.update(encoded.as_bytes());
        let id = format!("{:x}", digest.finalize());
        let mut state = self.state.lock().unwrap();
        if let Some(blocked) = state.blocked.get_mut(&id) {
            if blocked.allowed {
                if consume {
                    blocked.allowed = false;
                }
                return Ok(());
            }
            return Err(format!(
                "Outbound payload blocked. Approve this exact payload with /outbound allow {id}"
            ));
        }
        let mut strings = Vec::new();
        collect(payload, &mut strings);
        let reason = if encoded.len() > 1024 * 1024 {
            Some("exceeds the 1 MiB outbound inspection limit")
        } else if crate::redact::known_saturated() {
            Some("the process's known-secret tracking limit was reached")
        } else if state.secret_full {
            Some("the chat's file-secret tracking limit was reached")
        } else if crate::redact::contains_known(&encoded)
            || strings
                .iter()
                .any(|text| crate::redact::contains_known(text))
            || state.file_secrets.iter().any(|value| {
                encoded.contains(value) || strings.iter().any(|text| text.contains(value))
            })
        {
            Some("contains a known secret value")
        } else if files
            && strings.iter().any(|text| {
                state
                    .files
                    .iter()
                    .any(|file| shared_chunk(text.as_bytes(), file.as_bytes()))
            })
        {
            Some("contains at least 256 consecutive bytes from a file read in this chat")
        } else {
            None
        };
        let Some(reason) = reason else {
            return Ok(());
        };
        if state.blocked.len() >= BLOCKS {
            return Err("Outbound payload blocked; the chat has reached its limit of 64 blocked payload reviews".into());
        }
        let clean = crate::redact::outbound_preview(&encoded, &state.file_secrets);
        let preview: String = clean.chars().take(2048).collect();
        let review = json!({"id": id, "destination": crate::redact::outbound_preview(destination, &state.file_secrets), "reason": reason, "preview": preview});
        *notice.lock().unwrap() = Some(review.clone());
        state.blocked.insert(
            id.clone(),
            Blocked {
                review,
                allowed: false,
                answered: false,
            },
        );
        Err(format!(
            "Outbound payload blocked: {reason}. Approve this exact payload with /outbound allow {id}"
        ))
    }

    pub fn reviews(&self) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .blocked
            .values()
            .filter(|blocked| !blocked.answered)
            .map(|blocked| blocked.review.clone())
            .collect()
    }

    pub fn redact<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        let state = self.state.lock().unwrap();
        let mut output = std::borrow::Cow::Borrowed(text);
        for value in &state.file_secrets {
            if output.contains(value.as_str()) {
                output = std::borrow::Cow::Owned(output.replace(value, "[secret removed]"));
            }
        }
        output
    }

    pub fn approve(&self, id: &str) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let blocked = state
            .blocked
            .get_mut(id)
            .ok_or("This blocked payload is no longer available in this chat")?;
        blocked.allowed = true;
        blocked.answered = true;
        Ok(())
    }

    pub fn dismiss(&self, id: &str) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let blocked = state
            .blocked
            .get_mut(id)
            .ok_or("This blocked payload is no longer available in this chat")?;
        blocked.answered = true;
        blocked.allowed = false;
        Ok(())
    }
}

impl<P: crate::Provider, L: crate::EventLog> crate::Kernel<P, L> {
    pub(crate) async fn check_outbound_intent(
        &self,
        session: &crate::Session,
        intent: &crate::ToolIntent,
    ) -> Option<crate::Observation> {
        let Err(reason) = self.executor.check_outbound(intent).await else {
            return None;
        };
        let decision = crate::types::Decision::Deny {
            reason: reason.clone(),
        };
        if let Err(error) = self
            .log
            .append(crate::Event::policy(session, intent, &decision))
            .await
        {
            return Some(crate::Observation::error(
                &intent.id,
                format!("outbound denial could not be recorded: {error}"),
            ));
        }
        Some(crate::Observation::denial(&intent.id, reason))
    }
}

fn collect<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => {
            for item in items {
                collect(item, out);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                out.push(key);
                collect(value, out);
            }
        }
        _ => {}
    }
}

fn shared_chunk(payload: &[u8], file: &[u8]) -> bool {
    if payload.len() < FILE_CHUNK || file.len() < FILE_CHUNK {
        return false;
    }
    // Hash only the outbound windows; a hit is always verified against real bytes.
    let mut windows: Vec<_> = rolling(payload)
        .map(|(offset, hash)| (hash, offset))
        .collect();
    windows.sort_unstable();
    for (offset, hash) in rolling(file) {
        let start = windows.partition_point(|(value, _)| *value < hash);
        if windows[start..]
            .iter()
            .take_while(|(value, _)| *value == hash)
            .any(|(_, candidate)| {
                payload[*candidate..*candidate + FILE_CHUNK] == file[offset..offset + FILE_CHUNK]
            })
        {
            return true;
        }
    }
    false
}

fn rolling(bytes: &[u8]) -> impl Iterator<Item = (usize, u64)> + '_ {
    let power = (1..FILE_CHUNK).fold(1u64, |value, _| value.wrapping_mul(257));
    let initial = bytes.iter().take(FILE_CHUNK).fold(0u64, |value, byte| {
        value.wrapping_mul(257).wrapping_add(u64::from(*byte))
    });
    let mut hash = initial;
    (0..bytes.len().saturating_sub(FILE_CHUNK - 1)).map(move |offset| {
        if offset > 0 {
            hash = hash
                .wrapping_sub(u64::from(bytes[offset - 1]).wrapping_mul(power))
                .wrapping_mul(257)
                .wrapping_add(u64::from(bytes[offset + FILE_CHUNK - 1]));
        }
        (offset, hash)
    })
}

#[cfg(test)]
#[path = "egress_tests.rs"]
mod tests;
