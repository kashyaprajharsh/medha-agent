//! One read loop for every wire format: stall timer, size cap, held text and whole-body fallback.

use crate::transport::http;
use crate::transport::sse::{SseDecoder, SseEvent};
use futures::StreamExt;
use futures::stream::BoxStream;
use kernel::{Block, ProviderError};
use std::time::Duration;

/// What a wire format supplies; the loop owns timing, buffering and error order.
pub(crate) trait StreamDecoder: Send + 'static {
    fn push(&mut self, event: &SseEvent) -> Result<Vec<Block>, ProviderError>;
    /// The reply reached its end, so a quiet connection after it is not a stall.
    fn completed(&self) -> bool;
    /// Stop reading after this event, even if the server keeps the connection open.
    fn ends_stream(&self, event: &SseEvent) -> bool;
    /// Text held back for a possible tag, given up before an error.
    fn flush_pending(&mut self) -> Option<Block> {
        None
    }
    fn finish(self) -> Result<Vec<Block>, ProviderError>;
    /// The reply from one ordinary body, for servers that ignore `stream`.
    fn parse_whole(&self, body: &str) -> Result<Vec<Block>, ProviderError>;
}

pub(crate) async fn read<D: StreamDecoder>(
    response: reqwest::Response,
    streaming: bool,
    idle: Duration,
    mut decoder: D,
) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
    if !streaming {
        let body = http::response_text(response).await?;
        let blocks = decoder.parse_whole(&body)?;
        return Ok(futures::stream::iter(blocks.into_iter().map(Ok)).boxed());
    }
    let byte_stream = response.bytes_stream();
    let stream = async_stream::stream! {
        let mut sse = SseDecoder::default();
        // Kept until the first block; deciding by content type would delay every unlabelled stream.
        let mut unread = Some(Vec::new());
        futures::pin_mut!(byte_stream);
        let mut progress_deadline = tokio::time::Instant::now() + idle;
        'chunks: loop {
            let next = tokio::time::timeout_at(progress_deadline, byte_stream.next()).await;
            let chunk = match next {
                Err(_) if decoder.completed() => break,
                Err(_) => {
                    if let Some(block) = decoder.flush_pending() {
                        yield Ok(block);
                    }
                    yield Err(http::stalled_stream(idle));
                    return;
                }
                Ok(None) => break,
                Ok(Some(chunk)) => chunk,
            };
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(error) => {
                    if let Some(block) = decoder.flush_pending() {
                        yield Ok(block);
                    }
                    yield Err(http::stream_read_failed(error));
                    return;
                }
            };
            if let Some(buffer) = unread.as_mut() {
                if buffer.len() + bytes.len() <= http::MAX_RESPONSE_BODY_BYTES {
                    buffer.extend_from_slice(&bytes);
                } else {
                    unread = None;
                }
            }
            let events = match sse.push(&bytes) {
                Ok(events) => events,
                Err(error) => { yield Err(error); return; }
            };
            for event in events {
                if event.is_delivery() {
                    progress_deadline = tokio::time::Instant::now() + idle;
                }
                match decoder.push(&event) {
                    Ok(blocks) => for block in blocks { unread = None; yield Ok(block); },
                    Err(error) => { yield Err(error); return; }
                }
                if decoder.ends_stream(&event) { break 'chunks; }
            }
        }
        // A server may close without the final blank line.
        if let Some(event) = sse.finish() {
            match decoder.push(&event) {
                Ok(blocks) => for block in blocks { unread = None; yield Ok(block); },
                Err(error) => { yield Err(error); return; }
            }
        }
        if let Some(buffer) = unread.filter(|buffer| read_whole_body(buffer)) {
            match decoder.parse_whole(&String::from_utf8_lossy(&buffer)) {
                Ok(blocks) => for block in blocks { yield Ok(block); },
                Err(error) => yield Err(error),
            }
            return;
        }
        if let Some(block) = decoder.flush_pending() {
            yield Ok(block);
        }
        match decoder.finish() {
            Ok(blocks) => for block in blocks { yield Ok(block); },
            Err(error) => yield Err(error),
        }
    };
    Ok(stream.boxed())
}

/// Read a non-stream reply whole so its parse error is reported.
fn read_whole_body(buffer: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buffer);
    match text.trim_start().chars().next() {
        Some('{') => true,
        Some(_) => !text
            .lines()
            .any(|line| line.starts_with("data:") || line.starts_with("event:")),
        None => false,
    }
}
