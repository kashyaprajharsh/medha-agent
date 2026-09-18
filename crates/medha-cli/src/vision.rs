//! Auxiliary vision: a second model that reads images the main one cannot.

use anyhow::{Context, Result};
use futures::StreamExt;
use kernel::{Block, CompiledContext, ContentPart, MediaPart, MediaSource, ModelMessage, Role};
use std::{collections::HashMap, sync::Mutex};

const PROMPT: &str = "Describe this image for an assistant that cannot see it. Report the text \
                      verbatim, the layout, and anything that looks wrong or unexpected. State \
                      what is visible, never what it might mean.";

/// Words, not tokens: a cap the surface can explain, applied to the auxiliary
/// model's reply before it becomes part of the main model's context.
const MAX_DESCRIPTION_CHARS: usize = 4_000;

pub struct AuxiliaryVision {
    provider: providers::OpenAiCompat,
    model: String,
    /// Keyed by artifact hash: the same screenshot across many turns costs one
    /// call. Process-scoped; a restart pays for the first look again.
    cache: Mutex<HashMap<String, String>>,
}

impl AuxiliaryVision {
    pub fn new(provider: providers::OpenAiCompat) -> Self {
        Self {
            model: provider.active_model(),
            provider,
            cache: Mutex::new(HashMap::new()),
        }
    }

    async fn ask(&self, mime: &str, bytes: &[u8]) -> Result<String> {
        use base64::Engine;
        use kernel::Provider;
        let context = CompiledContext {
            model: self.model.clone(),
            messages: Vec::new(),
            ordered: Some(vec![ModelMessage {
                role: Role::User,
                parts: vec![
                    ContentPart::Text(kernel::TextPart {
                        text: PROMPT.into(),
                        provider_state: Vec::new(),
                    }),
                    ContentPart::Media(MediaPart {
                        mime_type: mime.to_string(),
                        source: MediaSource::Base64(
                            base64::engine::general_purpose::STANDARD.encode(bytes),
                        ),
                        label: None,
                        provider_state: Vec::new(),
                    }),
                ],
                trust: None,
            }]),
            tools: Vec::new(),
        };
        let mut stream = self
            .provider
            .stream(&context)
            .await
            .context("auxiliary vision request failed")?;
        let mut description = String::new();
        while let Some(block) = stream.next().await {
            if let Block::Text(text) = block.context("auxiliary vision stream failed")? {
                description.push_str(&text);
            }
        }
        let description = description.trim().to_string();
        anyhow::ensure!(
            !description.is_empty(),
            "the auxiliary vision model returned nothing"
        );
        Ok(
            match description.char_indices().nth(MAX_DESCRIPTION_CHARS) {
                Some((cut, _)) => format!("{}…", &description[..cut]),
                None => description,
            },
        )
    }
}

#[async_trait::async_trait]
impl kernel::VisionDescriber for AuxiliaryVision {
    async fn describe(&self, hash: &str, mime: &str, bytes: &[u8]) -> Result<String, String> {
        if let Some(hit) = self.cache.lock().unwrap().get(hash) {
            return Ok(hit.clone());
        }
        let description = self.ask(mime, bytes).await.map_err(|e| format!("{e:#}"))?;
        self.cache
            .lock()
            .unwrap()
            .insert(hash.to_string(), description.clone());
        Ok(description)
    }

    fn model(&self) -> &str {
        &self.model
    }
}

/// Build the auxiliary client from a saved profile and its stored credential.
pub fn connect(config: &crate::config::Config, name: &str) -> Result<providers::OpenAiCompat> {
    let resolved = crate::config::resolve_model(config, name)?;
    Ok(providers::OpenAiCompat::from_profile(
        resolved.provider,
        resolved.credential,
    )?)
}

#[cfg(test)]
#[path = "vision_tests.rs"]
mod tests;
