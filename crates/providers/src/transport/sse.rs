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

/// Each byte is looked at once however a record is split: the search for a
/// record's end goes on where the last chunk left it, and records already
/// handed out are skipped, not moved, until they are half the buffer.
#[derive(Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    /// Bytes before this were handed out as records.
    consumed: usize,
    /// Where the search for the current record's end goes on.
    scan: usize,
    /// Where the line being searched began.
    line_start: usize,
    /// The last chunk ended on a CR, so a LF that starts the next ends the same line.
    pending_cr: bool,
    #[cfg(test)]
    looked_at: usize,
}

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

impl SseDecoder {
    /// Append one arbitrary network chunk and return every complete event.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, kernel::ProviderError> {
        let waiting = self.buffer.len() - self.consumed;
        if waiting.saturating_add(chunk.len()) > MAX_FRAME_BYTES {
            *self = Self::default();
            return Err(kernel::ProviderError::Decode(
                "provider SSE frame exceeds 8 MiB".into(),
            ));
        }
        if self.consumed > 0 && self.consumed >= self.buffer.len() / 2 {
            self.buffer.drain(..self.consumed);
            self.scan -= self.consumed;
            self.line_start -= self.consumed;
            self.consumed = 0;
        }
        self.buffer.extend_from_slice(chunk);
        // Settled only by a byte that arrived: an empty chunk leaves the CR undecided.
        if self.pending_cr
            && let Some(&next) = self.buffer.get(self.scan)
        {
            self.pending_cr = false;
            if next == b'\n' {
                if self.scan == self.consumed {
                    self.consumed += 1;
                }
                self.scan += 1;
                self.line_start = self.scan;
            }
        }
        let mut events = Vec::new();
        while let Some(end) = self.record_end() {
            if let Some(event) = decode_record(&self.buffer[self.consumed..end]) {
                events.push(event);
            }
            self.consumed = end;
        }
        Ok(events)
    }

    /// Preserve the existing tolerant EOF behavior: if a server closes after
    /// a final record without its blank-line terminator, decode that record.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let record = std::mem::take(&mut self.buffer);
        let start = std::mem::take(self).consumed;
        (start < record.len()).then(|| decode_record(&record[start..]))?
    }

    /// Byte index immediately after the next SSE blank line, searching on from
    /// where the last search stopped. SSE permits CRLF, LF, or CR line endings,
    /// including mixed line endings across a record.
    fn record_end(&mut self) -> Option<usize> {
        let bytes = &self.buffer;
        while self.scan < bytes.len() {
            #[cfg(test)]
            {
                self.looked_at += 1;
            }
            let ending = match bytes[self.scan] {
                b'\n' => 1,
                b'\r' if bytes.get(self.scan + 1) == Some(&b'\n') => 2,
                b'\r' => {
                    // A CR the chunk ends on may be half of a CRLF the next chunk completes.
                    self.pending_cr = self.scan + 1 == bytes.len();
                    1
                }
                _ => {
                    self.scan += 1;
                    continue;
                }
            };
            let blank = self.scan == self.line_start;
            self.scan += ending;
            self.line_start = self.scan;
            if blank {
                return Some(self.scan);
            }
        }
        None
    }
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
