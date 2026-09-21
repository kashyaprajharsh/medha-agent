//! One usage settlement per provider attempt, including interrupted attempts.
use crate::{PreparedModelRequest, StreamSink, Usage};
use sha2::{Digest, Sha256};

pub(crate) struct AttemptUsage<'a> {
    sink: &'a dyn StreamSink,
    latest: Option<Usage>,
    id: ulid::Ulid,
    started: std::time::Instant,
}

impl<'a> AttemptUsage<'a> {
    pub(crate) fn new(sink: &'a dyn StreamSink, request: &PreparedModelRequest) -> Self {
        let id = ulid::Ulid::new();
        tracing::info!(attempt = %id, model = %request.model, protocol = ?request.protocol,
            fingerprint = %request.request_fingerprint, "model request dispatched");
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
