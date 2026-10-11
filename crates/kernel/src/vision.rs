//! Images for a model that cannot see: described by a second model as labelled
//! text, or replaced by a note. An image never stops a turn or passes unremarked.

use crate::{ContentPart, MediaSource, TextPart};

#[async_trait::async_trait]
pub trait VisionDescriber: Send + Sync + 'static {
    /// Describe `bytes` for a model that cannot see them. `hash` identifies the
    /// artifact, so implementations can cache a repeated image.
    async fn describe(&self, hash: &str, mime: &str, bytes: &[u8]) -> Result<String, String>;

    /// Model id behind the description, for the note that carries it.
    fn model(&self) -> &str;
}

/// What became of the images in a request bound for a model that cannot see.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Substituted {
    /// Images replaced by a description.
    pub described: usize,
    /// Images replaced by a note that they could not be shown.
    pub not_shown: usize,
    /// Why the describing model, when there is one, could not describe them.
    pub failure: Option<String>,
}

/// Replace every image with text, in place: a description from `describer`,
/// or a note that it is not shown when there is none or it fails.
pub async fn describe_media(
    context: &mut crate::CompiledContext,
    store: std::sync::Arc<dyn crate::ArtifactStore>,
    describer: Option<&dyn VisionDescriber>,
) -> Substituted {
    let mut messages = context.ordered_messages();
    let mut described: std::collections::HashMap<String, Result<String, String>> =
        std::collections::HashMap::new();
    let mut outcome = Substituted::default();
    for message in &mut messages {
        for part in &mut message.parts {
            let ContentPart::Media(media) = part else {
                continue;
            };
            let read = match (describer, &media.source) {
                (None, _) => None,
                (Some(_), MediaSource::Artifact(hash)) if described.contains_key(hash) => {
                    Some(described[hash].clone())
                }
                (Some(describer), MediaSource::Artifact(hash)) => {
                    let text = match crate::artifacts::read_all(&store, hash).await {
                        Ok(bytes) => describer.describe(hash, &media.mime_type, &bytes).await,
                        Err(error) => Err(error),
                    };
                    described.insert(hash.clone(), text.clone());
                    Some(text)
                }
                (Some(_), _) => Some(Err("the image was not stored".into())),
            };
            let text = match (describer, read) {
                (Some(describer), Some(Ok(text))) => {
                    outcome.described += 1;
                    format!(
                        "[image unavailable to this model. {} described it: {text}]",
                        describer.model()
                    )
                }
                (Some(describer), Some(Err(error))) => {
                    outcome.not_shown += 1;
                    outcome.failure.get_or_insert(error);
                    format!(
                        "[image not shown: this model cannot read images, and {} could not \
                         describe it]",
                        describer.model()
                    )
                }
                _ => {
                    outcome.not_shown += 1;
                    "[image not shown: this model cannot read images, and no vision model is \
                     configured to describe it]"
                        .to_owned()
                }
            };
            *part = ContentPart::Text(TextPart {
                text,
                provider_state: Vec::new(),
            });
        }
    }
    context.ordered = Some(messages);
    outcome
}

#[cfg(test)]
#[path = "vision_tests.rs"]
mod tests;
