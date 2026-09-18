//! Local image admission shared by every Medha surface.

mod clipboard;
pub mod refs;

pub use clipboard::clipboard;

use anyhow::{Context, Result, ensure};
use kernel::{ArtifactStore, MediaPart, MediaSource};
use std::{io::Read, path::Path, path::PathBuf, sync::Arc};

pub const MAX_PER_MESSAGE: usize = 4;

/// Sent when images arrive with no words. Every surface uses the same line, and
/// shows it, so what the model received is what the user can see was sent.
pub const IMAGE_ONLY_PROMPT: &str = "Describe the attached image(s).";

/// Refused before decoding. Anything under this but over the wire budget is
/// resized rather than rejected.
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;

/// One admitted image: the artifact reference that travels with the message,
/// plus what the surface needs to show the user what they attached.
#[derive(Clone, Debug)]
pub struct Attachment {
    pub part: MediaPart,
    pub label: String,
    /// Where it came from, so a path already staged is not attached twice when
    /// it is still sitting in the composer text. `None` for the clipboard.
    pub source: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    /// What admission changed on the way in, if anything.
    pub note: Option<String>,
}

impl Attachment {
    /// `shot.png  1920×1080 · 240 KB`
    pub fn summary(&self) -> String {
        format!(
            "{}  {}×{} · {}",
            self.label,
            self.width,
            self.height,
            human_size(self.bytes)
        )
    }
}

pub fn human_size(bytes: usize) -> String {
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{} KB", bytes / 1024),
        _ => format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0)),
    }
}

pub async fn ingest(paths: Vec<PathBuf>, store: Arc<dyn ArtifactStore>) -> Result<Vec<Attachment>> {
    ensure!(
        paths.len() <= MAX_PER_MESSAGE,
        "attach at most {MAX_PER_MESSAGE} images per message"
    );
    tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|path| {
                let raw = read_source(&path)?;
                let mut attachment = admit(raw, label_for(&path), &store)
                    .with_context(|| format!("attachment {}", path.display()))?;
                attachment.source = Some(path);
                Ok(attachment)
            })
            .collect()
    })
    .await?
}

/// Normalise, store by content hash, and describe. The bytes are copied here
/// and never read again, so a file that moves or is deleted — a dropped
/// screenshot in a temporary folder — still sends.
pub(crate) fn admit(
    raw: Vec<u8>,
    label: String,
    store: &Arc<dyn ArtifactStore>,
) -> Result<Attachment> {
    let image = media::normalize(raw)?;
    let hash = store.put(&image.bytes).map_err(anyhow::Error::msg)?;
    Ok(Attachment {
        part: MediaPart {
            mime_type: image.mime.into(),
            source: MediaSource::Artifact(hash),
            label: Some(label.clone()),
            provider_state: Vec::new(),
        },
        label,
        source: None,
        width: image.width,
        height: image.height,
        bytes: image.bytes.len(),
        note: image.note,
    })
}

fn read_source(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot open attachment {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "attachment must be a regular file: {}",
        path.display()
    );
    let mut raw = Vec::new();
    file.take(MAX_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut raw)?;
    ensure!(
        raw.len() <= MAX_SOURCE_BYTES,
        "attachment is larger than {}",
        human_size(MAX_SOURCE_BYTES)
    );
    Ok(raw)
}

pub fn label_for(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    match name.char_indices().nth(28) {
        Some((cut, _)) => format!("{}…", &name[..cut]),
        None => name,
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
