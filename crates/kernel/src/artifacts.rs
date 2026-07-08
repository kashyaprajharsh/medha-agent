//! Content-addressed artifact store (§4.2/§4.5). Large tool outputs spill here
//! by hash so they never blow the context window, while staying fully
//! recoverable via the `read_artifact` tool (range reads). The kernel knows
//! only this trait; the file-backed implementation lives in the store crate (P8).

pub trait ArtifactStore: Send + Sync {
    /// Store bytes, returning a content hash (idempotent for identical content).
    fn put(&self, bytes: &[u8]) -> Result<String, String>;
    /// Read a byte range (offset + optional length) of a stored artifact.
    fn get(&self, hash: &str, offset: usize, len: Option<usize>) -> Result<Vec<u8>, String>;
    /// Total size in bytes of a stored artifact.
    fn size(&self, hash: &str) -> Result<usize, String>;
}
