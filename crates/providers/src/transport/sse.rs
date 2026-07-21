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

#[derive(Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    /// Append one arbitrary network chunk and return every complete event.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = find_record_end(&self.buffer) {
            let record: Vec<u8> = self.buffer.drain(..end).collect();
            if let Some(event) = decode_record(&record) {
                events.push(event);
            }
        }
        events
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
mod tests {
    use super::*;

    #[test]
    fn frames_lf_crlf_cr_and_mixed_endings() {
        for bytes in [
            b"data: one\n\n".as_slice(),
            b"data: one\r\n\r\n".as_slice(),
            b"data: one\r\r".as_slice(),
            b"data: one\r\n\n".as_slice(),
        ] {
            let mut decoder = SseDecoder::default();
            assert_eq!(
                decoder.push(bytes),
                vec![SseEvent {
                    event: None,
                    data: "one".into(),
                }]
            );
        }
    }

    #[test]
    fn arbitrary_byte_fragmentation_preserves_typed_multiline_events() {
        let fixture = b"event: content.delta\r\ndata: {\"value\":\r\ndata: \"hello\"}\r\n\r\n";
        for split in 0..=fixture.len() {
            let mut decoder = SseDecoder::default();
            let mut events = decoder.push(&fixture[..split]);
            events.extend(decoder.push(&fixture[split..]));
            assert_eq!(
                events,
                vec![SseEvent {
                    event: Some("content.delta".into()),
                    data: "{\"value\":\n\"hello\"}".into(),
                }],
                "split at byte {split}"
            );
        }
    }

    #[test]
    fn finish_decodes_an_unterminated_final_record_and_ignores_comments() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b": keepalive\n").is_empty());
        assert_eq!(
            decoder.finish(),
            None,
            "a comment-only record is not a protocol event"
        );

        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: final").is_empty());
        assert_eq!(
            decoder.finish(),
            Some(SseEvent {
                event: None,
                data: "final".into(),
            })
        );
    }
}
