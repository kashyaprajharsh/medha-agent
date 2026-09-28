//! One usage settlement per provider attempt, including interrupted attempts.
use crate::{PreparedModelRequest, StreamSink, Usage};
use sha2::{Digest, Sha256};

/// Hash the entire priced prefix, not just its last message: edits anywhere
/// inside it, changed tools, routing or provider settings invalidate calibration.
/// The rest of the call's usage rides along, so the log is also the record of
/// what each model call cost; older anchors simply lack those fields.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct UsageAnchor {
    identity: String,
    fingerprint: String,
    message_count: usize,
    ordered_count: Option<usize>,
    pub prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_prompt_tokens: Option<u32>,
    /// `None` when no price is known for the model, never a guessed zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
}

impl UsageAnchor {
    pub fn capture(
        identity: String,
        request: &PreparedModelRequest,
        usage: &Usage,
        pricing: Option<crate::types::Pricing>,
    ) -> Self {
        Self {
            identity,
            fingerprint: request.request_fingerprint.clone(),
            message_count: request.context.messages.len(),
            ordered_count: request.context.ordered.as_ref().map(Vec::len),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cached_prompt_tokens: usage.cached_prompt_tokens,
            cost_usd: pricing.map(|pricing| pricing.cost(usage)),
        }
    }

    pub fn matching_context<P: crate::Provider>(
        &self,
        provider: &P,
        candidate: &crate::CompiledContext,
    ) -> Option<crate::CompiledContext> {
        if self.prompt_tokens == 0
            || self.message_count == 0
            || self.identity != provider.context_identity()
            || self.message_count > candidate.messages.len()
        {
            return None;
        }
        let mut prefix = candidate.clone();
        prefix.messages.truncate(self.message_count);
        match (self.ordered_count, &mut prefix.ordered) {
            (Some(count), Some(messages)) if count <= messages.len() => messages.truncate(count),
            (None, None) => {}
            _ => return None,
        }
        let prepared = provider.prepare_request(&prefix).ok()?;
        (prepared.request_fingerprint == self.fingerprint).then_some(prefix)
    }
}

pub(crate) struct AttemptUsage<'a> {
    sink: &'a dyn StreamSink,
    latest: Option<Usage>,
    id: ulid::Ulid,
    started: std::time::Instant,
}

impl<'a> AttemptUsage<'a> {
    pub(crate) fn new(sink: &'a dyn StreamSink, request: &PreparedModelRequest) -> Self {
        let id = ulid::Ulid::new();
        let mut images = 0usize;
        let mut image_pixels = 0u64;
        // Inspect borrowed canonical parts; never clone or serialize image data
        // just to report request shape.
        if let Some(messages) = &request.context.ordered {
            for media in messages
                .iter()
                .flat_map(|message| &message.parts)
                .filter_map(|part| {
                    if let crate::ContentPart::Media(media) = part {
                        Some(media)
                    } else {
                        None
                    }
                })
            {
                images += 1;
                image_pixels = image_pixels.saturating_add(
                    u64::from(media.width.unwrap_or(0)) * u64::from(media.height.unwrap_or(0)),
                );
            }
        } else {
            for media in request
                .context
                .messages
                .iter()
                .flat_map(|message| &message.attachments)
            {
                images += 1;
                image_pixels = image_pixels.saturating_add(
                    u64::from(media.width.unwrap_or(0)) * u64::from(media.height.unwrap_or(0)),
                );
            }
        }
        tracing::info!(attempt = %id, model = %request.model, protocol = ?request.protocol,
            fingerprint = %request.request_fingerprint,
            streaming = ?request.body.get("stream").and_then(serde_json::Value::as_bool),
            output_limit = ?request.body.get("max_tokens").and_then(serde_json::Value::as_u64),
            messages = request.context.messages.len(), tools = request.context.tools.len(),
            images, image_pixels, "model request dispatched");
        // Opt-in structural diagnostics. These hashes detect changed serialized
        // fields, not provider tokenization, KV blocks or actual cache hits.
        // Never log prompt contents or credentials.
        if tracing::enabled!(tracing::Level::DEBUG) {
            let fields: Vec<_> = request
                .body
                .as_object()
                .into_iter()
                .flat_map(|body| {
                    body.iter().filter(|(key, _)| {
                        matches!(
                            key.as_str(),
                            "messages" | "input" | "system" | "tools" | "instructions"
                        )
                    })
                })
                .map(|(key, value)| {
                    let hashes: Vec<_> = match value.as_array() {
                        Some(values) => values.iter().map(fingerprint).collect(),
                        None => vec![fingerprint(value)],
                    };
                    (key, hashes)
                })
                .collect();
            tracing::debug!(attempt = %id, fields = ?fields, "request prefix field fingerprints");
        }
        Self {
            sink,
            latest: None,
            id,
            started: std::time::Instant::now(),
        }
    }

    pub(crate) fn observe(&mut self, usage: Usage) {
        // Usage blocks are cumulative snapshots, not increments. A retry owns
        // another reporter and therefore remains a separately billed attempt.
        self.latest = Some(usage);
    }
}

fn fingerprint(value: &serde_json::Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

impl Drop for AttemptUsage<'_> {
    fn drop(&mut self) {
        tracing::info!(attempt = %self.id, elapsed_ms = self.started.elapsed().as_millis() as u64,
            prompt_tokens = ?self.latest.map(|u| u.prompt_tokens),
            cached_prompt_tokens = ?self.latest.and_then(|u| u.cached_prompt_tokens),
            completion_tokens = ?self.latest.map(|u| u.completion_tokens),
            "model attempt usage settled (last reported snapshot)");
        if let Some(usage) = self.latest {
            self.sink.usage(&usage);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    #[derive(Default)]
    struct Sink(Mutex<Vec<Usage>>);
    impl StreamSink for Sink {
        fn usage(&self, usage: &Usage) {
            self.0.lock().unwrap().push(*usage);
        }
    }
    fn reporter(sink: &Sink) -> AttemptUsage<'_> {
        AttemptUsage {
            sink,
            latest: None,
            id: ulid::Ulid::new(),
            started: std::time::Instant::now(),
        }
    }
    #[test]
    fn snapshots_replace_and_attempts_accumulate_without_inventing_missing_usage() {
        let sink = Sink::default();
        let first = Usage {
            prompt_tokens: 100,
            cached_prompt_tokens: Some(75),
            ..Usage::default()
        };
        let last = Usage {
            completion_tokens: 20,
            ..first
        };
        {
            let mut attempt = reporter(&sink);
            attempt.observe(first);
            attempt.observe(last);
            attempt.observe(last);
            assert!(sink.0.lock().unwrap().is_empty());
        }
        drop(reporter(&sink));
        {
            let mut retry = reporter(&sink);
            retry.observe(first);
        }
        let values = sink.0.lock().unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].completion_tokens, 20);
        assert_eq!(values[1].completion_tokens, 0);
    }
}
