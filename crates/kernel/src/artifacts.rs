//! Content-addressed storage for recoverable, range-readable tool output.

/// Local ingestion limit, independent of any provider's advertised limit.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Read a whole image artifact. The storage port caps a single read, so this
/// walks bounded ranges rather than trusting one call to return everything.
pub async fn read_all(
    store: &std::sync::Arc<dyn ArtifactStore>,
    hash: &str,
) -> Result<Vec<u8>, String> {
    let size = std::sync::Arc::clone(store)
        .size_async(hash.to_string())
        .await?;
    if size == 0 || size > MAX_IMAGE_BYTES {
        return Err("image artifact is empty or exceeds the 10 MiB limit".into());
    }
    let mut bytes = Vec::with_capacity(size);
    while bytes.len() < size {
        let chunk = std::sync::Arc::clone(store)
            .get_async(
                hash.to_string(),
                bytes.len(),
                Some((size - bytes.len()).min(1024 * 1024)),
            )
            .await?;
        if chunk.is_empty() || chunk.len() > size - bytes.len() {
            return Err("image artifact changed or was truncated".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Resolve a disposable request copy. Durable messages retain content hashes.
pub async fn resolve_media(
    context: &mut crate::CompiledContext,
    store: std::sync::Arc<dyn ArtifactStore>,
) -> Result<(), String> {
    use crate::{ContentPart, MediaSource};
    use base64::Engine;
    let mut messages = context.ordered_messages();
    let mut encoded = std::collections::HashMap::new();
    for message in &mut messages {
        for part in &mut message.parts {
            let ContentPart::Media(media) = part else {
                continue;
            };
            let MediaSource::Artifact(hash) = &media.source else {
                continue;
            };
            if !encoded.contains_key(hash) {
                let bytes = read_all(&store, hash).await?;
                encoded.insert(
                    hash.clone(),
                    base64::engine::general_purpose::STANDARD.encode(bytes),
                );
            }
            media.source = MediaSource::Base64(encoded[hash].clone());
        }
    }
    context.ordered = Some(messages);
    Ok(())
}

#[async_trait::async_trait]
pub trait ArtifactStore: Send + Sync + 'static {
    /// Store bytes, returning a content hash (idempotent for identical content).
    fn put(&self, bytes: &[u8]) -> Result<String, String>;
    /// Read a byte range (offset + optional length) of a stored artifact.
    fn get(&self, hash: &str, offset: usize, len: Option<usize>) -> Result<Vec<u8>, String>;
    /// Total size in bytes of a stored artifact.
    fn size(&self, hash: &str) -> Result<usize, String>;

    /// Async-runtime-safe wrappers for file-backed implementations. The
    /// synchronous methods remain the small storage-port API used by tests and
    /// non-async callers, while runtime paths must use these wrappers so
    /// hashing, range verification, writes, and fsync never occupy a Tokio
    /// worker.
    async fn put_async(self: std::sync::Arc<Self>, bytes: Vec<u8>) -> Result<String, String> {
        tokio::task::spawn_blocking(move || self.put(&bytes))
            .await
            .map_err(|error| format!("artifact writer task failed: {error}"))?
    }

    async fn get_async(
        self: std::sync::Arc<Self>,
        hash: String,
        offset: usize,
        len: Option<usize>,
    ) -> Result<Vec<u8>, String> {
        tokio::task::spawn_blocking(move || self.get(&hash, offset, len))
            .await
            .map_err(|error| format!("artifact reader task failed: {error}"))?
    }

    async fn size_async(self: std::sync::Arc<Self>, hash: String) -> Result<usize, String> {
        tokio::task::spawn_blocking(move || self.size(&hash))
            .await
            .map_err(|error| format!("artifact metadata task failed: {error}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::ArtifactStore;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    struct SlowStore;

    impl ArtifactStore for SlowStore {
        fn put(&self, _bytes: &[u8]) -> Result<String, String> {
            std::thread::sleep(Duration::from_millis(150));
            Ok("hash".into())
        }

        fn get(&self, _hash: &str, _offset: usize, _len: Option<usize>) -> Result<Vec<u8>, String> {
            std::thread::sleep(Duration::from_millis(150));
            Ok(vec![1])
        }

        fn size(&self, _hash: &str) -> Result<usize, String> {
            std::thread::sleep(Duration::from_millis(150));
            Ok(1)
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_wrappers_leave_the_runtime_responsive() {
        let store: Arc<dyn ArtifactStore> = Arc::new(SlowStore);
        let started = Instant::now();
        let writer = Arc::clone(&store).put_async(vec![1]);
        let timer = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                started.elapsed() < Duration::from_millis(100),
                "artifact I/O occupied the async runtime thread"
            );
        };
        let (result, ()) = tokio::join!(writer, timer);
        assert_eq!(result.unwrap(), "hash");
    }
}
