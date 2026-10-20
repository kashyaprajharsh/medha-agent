//! Protocol-neutral Server-Sent Events framing.
//!
//! HTTP owns arbitrary byte chunks; provider protocols own event meaning. This
//! decoder is the boundary between them: it buffers incomplete records and
//! exposes the SSE `event` and joined `data` fields without interpreting JSON.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    /// Whether the provider sent something, as opposed to holding the line
    /// open. A keep-alive comment never becomes an event at all, and an event
    /// with no data says nothing. What the data means is not judged here: a
    /// provider building a large tool call may send only empty deltas.
    pub(crate) fn is_delivery(&self) -> bool {
        !self.data.trim().is_empty()
    }
}

#[derive(Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
}

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

impl SseDecoder {
    /// Append one arbitrary network chunk and return every complete event.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, kernel::ProviderError> {
        if self.buffer.len().saturating_add(chunk.len()) > MAX_FRAME_BYTES {
            self.buffer.clear();
            return Err(kernel::ProviderError::Decode(
                "provider SSE frame exceeds 8 MiB".into(),
            ));
        }
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = find_record_end(&self.buffer) {
            let record: Vec<u8> = self.buffer.drain(..end).collect();
            if let Some(event) = decode_record(&record) {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Preserve the existing tolerant EOF behavior: if a server closes after
    /// a final record without its blank-line terminator, decode that record.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.buffer.is_empty() {
            return None;
        }
        let record = std::mem::take(&mut self.buffer);
        decode_record(&record)
    }
}

/// Byte index immediately after the first SSE blank line. SSE permits CRLF,
/// LF, or CR line endings, including mixed line endings across a record.
fn find_record_end(bytes: &[u8]) -> Option<usize> {
    let mut line_start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let ending = match bytes[index] {
            b'\n' => 1,
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => 2,
            b'\r' => 1,
            _ => {
                index += 1;
                continue;
            }
        };
        if index == line_start {
            return Some(index + ending);
        }
        index += ending;
        line_start = index;
    }
    None
}

pub(crate) fn decode_record(record: &[u8]) -> Option<SseEvent> {
    let record = String::from_utf8_lossy(record);
    let normalized = record.replace("\r\n", "\n").replace('\r', "\n");
    let mut event = None;
    let mut data = Vec::new();
    let mut saw_field = false;

    for line in normalized.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => {
                event = Some(value.to_string());
                saw_field = true;
            }
            "data" => {
                data.push(value);
                saw_field = true;
            }
            _ => {}
        }
    }

    saw_field.then(|| SseEvent {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
