//! Reading images for routes that cannot carry them.
//!
//! When the selected model has no image input, the alternative to refusing the
//! turn is a second model that does: it describes the image once and the
//! description travels as text. The description is lossy and is labelled as
//! such in the message, so neither the model nor the user mistakes it for the
//! image itself.

use crate::{ContentPart, MediaSource, TextPart};

#[async_trait::async_trait]
pub trait VisionDescriber: Send + Sync + 'static {
    /// Describe `bytes` for a model that cannot see them. `hash` identifies the
    /// artifact, so implementations can cache a repeated image.
    async fn describe(&self, hash: &str, mime: &str, bytes: &[u8]) -> Result<String, String>;

    /// Model id behind the description, for the note that carries it.
    fn model(&self) -> &str;
}

/// Replace every image with a description from `describer`, in place. Returns
/// the number of images described.
pub async fn describe_media(
    context: &mut crate::CompiledContext,
    store: std::sync::Arc<dyn crate::ArtifactStore>,
    describer: &dyn VisionDescriber,
) -> Result<usize, String> {
    let mut messages = context.ordered_messages();
    let mut described: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut count = 0;
    for message in &mut messages {
        for part in &mut message.parts {
            let ContentPart::Media(media) = part else {
                continue;
            };
            let MediaSource::Artifact(hash) = &media.source else {
                return Err("image must be stored before it can be described".into());
            };
            if !described.contains_key(hash) {
                let bytes = crate::artifacts::read_all(&store, hash).await?;
                let text = describer.describe(hash, &media.mime_type, &bytes).await?;
                described.insert(hash.clone(), text);
            }
            *part = ContentPart::Text(TextPart {
                text: format!(
                    "[image unavailable to this model. {} described it: {}]",
                    describer.model(),
                    described[hash]
                ),
                provider_state: Vec::new(),
            });
            count += 1;
        }
    }
    context.ordered = Some(messages);
    Ok(count)
}

/// No auxiliary model configured: images cannot be read, and the turn says so
/// rather than dropping them.
pub struct NoVision;

#[async_trait::async_trait]
impl VisionDescriber for NoVision {
    async fn describe(&self, _: &str, _: &str, _: &[u8]) -> Result<String, String> {
        Err(
            "this model cannot accept images, and no auxiliary vision model is configured; \
             switch to a vision-capable model, or name one under [auxiliary] vision in \
             config.toml"
                .into(),
        )
    }

    fn model(&self) -> &str {
        "none"
    }
}

#[cfg(test)]
#[path = "vision_tests.rs"]
mod tests;
