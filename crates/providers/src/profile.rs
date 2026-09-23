//! Serializable provider deployment profiles.
//!
//! A profile selects a wire protocol and describes how to reach one model. It
//! never contains the credential itself: callers resolve that separately from
//! the secret store and hand it to [`ProviderClient`](crate::ProviderClient).

use kernel::{ImageInputMode, Protocol, ReasoningEffort, ReasoningSupport, TokenAccountingMode};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

use crate::models_dev::ModelCapabilities;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    #[default]
    None,
    Bearer,
    XApiKey,
    XGoogApiKey,
}

impl AuthKind {
    pub const fn requires_credential(self) -> bool {
        !matches!(self, Self::None)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer => "bearer",
            Self::XApiKey => "x-api-key",
            Self::XGoogApiKey => "x-goog-api-key",
        }
    }

    pub const fn for_protocol(protocol: Protocol) -> Self {
        match protocol {
            Protocol::OpenAiChat | Protocol::OpenAiResponses => Self::Bearer,
            Protocol::AnthropicMessages => Self::XApiKey,
            Protocol::GeminiInteractions => Self::XGoogApiKey,
        }
    }
}

/// Optional authoritative token-count route declared by the profile. This is
/// independent of the generation protocol; vLLM still uses OpenAI Chat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenCounter {
    #[default]
    None,
    Vllm,
}

/// Chat output-limit spelling for gateways with older compatibility schemas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatTokenLimit {
    #[default]
    Auto,
    MaxTokens,
    MaxCompletionTokens,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderProfile {
    pub protocol: Protocol,
    pub base_url: String,
    pub model: String,
    pub auth: AuthKind,
    /// Non-secret escape hatch for stable provider or gateway requirements.
    /// Authentication headers are rejected here so credentials cannot be
    /// serialized accidentally.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ctx: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub token_counter: TokenCounter,
    #[serde(default, skip_serializing_if = "is_default")]
    pub token_accounting: TokenAccountingMode,
    #[serde(default, skip_serializing_if = "is_default")]
    pub reasoning: ReasoningSupport,
    /// Exact levels supported by this model, when known. Missing means
    /// unverified, not that every endpoint accepts every level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_efforts: Option<Vec<ReasoningEffort>>,
    /// Optional exact per-model capability override. This is intentionally
    /// separate from catalog metadata: a deployment may expose a different
    /// capability set than the public model listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ModelCapabilities>,
    /// Select native pixels, auxiliary text descriptions, or automatic
    /// capability discovery for this exact deployment.
    #[serde(default, skip_serializing_if = "is_default")]
    pub image_input: ImageInputMode,
    #[serde(default, skip_serializing_if = "is_default")]
    pub chat_token_limit: ChatTokenLimit,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    value == &T::default()
}

impl ProviderProfile {
    pub fn openai_chat(
        base_url: impl Into<String>,
        model: impl Into<String>,
        auth: AuthKind,
    ) -> Self {
        Self {
            protocol: Protocol::OpenAiChat,
            base_url: base_url.into(),
            model: model.into(),
            auth,
            headers: BTreeMap::new(),
            max_ctx: None,
            max_output_tokens: None,
            token_counter: TokenCounter::None,
            token_accounting: TokenAccountingMode::Adaptive,
            reasoning: ReasoningSupport::Unknown,
            reasoning_efforts: None,
            capabilities: None,
            image_input: ImageInputMode::Auto,
            chat_token_limit: ChatTokenLimit::Auto,
        }
    }

    pub(crate) fn chat_token_limit_field(&self) -> &'static str {
        match self.chat_token_limit {
            ChatTokenLimit::MaxTokens => "max_tokens",
            ChatTokenLimit::MaxCompletionTokens => "max_completion_tokens",
            ChatTokenLimit::Auto
                if reqwest::Url::parse(&self.base_url)
                    .ok()
                    .is_some_and(|url| url.host_str() == Some("api.openai.com")) =>
            {
                "max_completion_tokens"
            }
            ChatTokenLimit::Auto => "max_tokens",
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let base_url = self.base_url.trim();
        if base_url.is_empty() {
            return Err("provider base URL cannot be empty".into());
        }
        let parsed = reqwest::Url::parse(base_url)
            .map_err(|error| format!("invalid provider base URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!(
                "provider base URL must use http or https, not '{}'",
                parsed.scheme()
            ));
        }
        if parsed.host_str().is_none() {
            return Err("provider base URL must include a host".into());
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(
                "provider base URL must not contain credentials; select an auth kind instead"
                    .into(),
            );
        }
        if parsed.fragment().is_some() {
            return Err("provider base URL must not contain a URL fragment".into());
        }
        if self.model.trim().is_empty() {
            return Err("provider model cannot be empty".into());
        }
        if self.max_ctx == Some(0) {
            return Err("provider context window must be positive".into());
        }
        if self.max_output_tokens == Some(0) {
            return Err("provider output limit must be positive".into());
        }
        if self
            .max_ctx
            .zip(self.max_output_tokens)
            .is_some_and(|(context, output)| output >= u64::from(context))
        {
            return Err(
                "provider output limit must leave room for input within the context window".into(),
            );
        }
        if self.token_counter == TokenCounter::Vllm && self.protocol != Protocol::OpenAiChat {
            return Err("the vLLM counter is only valid with the open-ai-chat protocol".into());
        }
        for (name, value) in &self.headers {
            let header = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| format!("invalid provider header name '{name}': {error}"))?;
            reqwest::header::HeaderValue::from_str(value)
                .map_err(|error| format!("invalid value for provider header '{name}': {error}"))?;
            if is_auth_header(&header) {
                return Err(format!(
                    "provider header '{name}' may contain a credential; select an auth kind instead"
                ));
            }
        }
        Ok(())
    }
}

fn is_auth_header(name: &reqwest::header::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization" | "x-api-key" | "x-goog-api-key"
    )
}

/// Compatibility decoder for the pre-profile `needs_key` boolean. New files
/// serialize `auth`, while existing files continue to load as Bearer profiles.
impl<'de> Deserialize<'de> for ProviderProfile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawProfile {
            #[serde(default)]
            protocol: Protocol,
            base_url: String,
            model: String,
            #[serde(default)]
            auth: Option<AuthKind>,
            #[serde(default)]
            needs_key: bool,
            #[serde(default)]
            headers: BTreeMap<String, String>,
            #[serde(default)]
            max_ctx: Option<u32>,
            #[serde(default)]
            max_output_tokens: Option<u64>,
            #[serde(default)]
            token_counter: TokenCounter,
            #[serde(default)]
            token_accounting: TokenAccountingMode,
            #[serde(default)]
            reasoning: ReasoningSupport,
            #[serde(default)]
            reasoning_efforts: Option<Vec<ReasoningEffort>>,
            #[serde(default)]
            capabilities: Option<ModelCapabilities>,
            #[serde(default)]
            image_input: ImageInputMode,
            #[serde(default)]
            chat_token_limit: ChatTokenLimit,
        }

        let raw = RawProfile::deserialize(deserializer)?;
        Ok(Self {
            protocol: raw.protocol,
            base_url: raw.base_url,
            model: raw.model,
            auth: raw.auth.unwrap_or(if raw.needs_key {
                AuthKind::Bearer
            } else {
                AuthKind::None
            }),
            headers: raw.headers,
            max_ctx: raw.max_ctx,
            max_output_tokens: raw.max_output_tokens,
            token_counter: raw.token_counter,
            token_accounting: raw.token_accounting,
            reasoning: raw.reasoning,
            reasoning_efforts: raw.reasoning_efforts,
            capabilities: raw.capabilities,
            image_input: raw.image_input,
            chat_token_limit: raw.chat_token_limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_reservation_cannot_consume_the_entire_window() {
        let mut profile =
            ProviderProfile::openai_chat("https://example.test/v1", "model", AuthKind::None);
        profile.max_ctx = Some(8_000);
        assert!(
            profile.validate().is_ok(),
            "server-default output remains valid"
        );
        profile.max_output_tokens = Some(8_000);
        assert!(profile.validate().is_err());
        profile.max_output_tokens = Some(4_000);
        assert!(profile.validate().is_ok());
    }

    #[test]
    fn legacy_needs_key_migrates_to_bearer_auth() {
        let profile: ProviderProfile = serde_json::from_value(serde_json::json!({
            "base_url": "https://example.test/v1",
            "model": "model",
            "needs_key": true
        }))
        .unwrap();
        assert_eq!(profile.protocol, Protocol::OpenAiChat);
        assert_eq!(profile.auth, AuthKind::Bearer);

        let serialized = serde_json::to_value(&profile).unwrap();
        assert_eq!(serialized["auth"], "bearer");
        assert!(serialized.get("needs_key").is_none());
    }

    #[test]
    fn validation_rejects_secret_headers_and_protocol_counter_mismatch() {
        let mut profile =
            ProviderProfile::openai_chat("https://example.test/v1", "model", AuthKind::Bearer);
        profile
            .headers
            .insert("Authorization".into(), "secret".into());
        assert!(profile.validate().is_err());

        profile.headers.clear();
        profile.protocol = Protocol::GeminiInteractions;
        profile.token_counter = TokenCounter::Vllm;
        assert!(profile.validate().is_err());
    }

    #[test]
    fn validation_rejects_credentials_embedded_in_base_url() {
        let profile = ProviderProfile::openai_chat(
            "https://user:secret@example.test/v1",
            "model",
            AuthKind::None,
        );
        assert!(profile.validate().is_err());
    }

    #[test]
    fn capability_override_round_trips_without_becoming_a_secret() {
        let mut profile = ProviderProfile::openai_chat(
            "https://example.test/v1",
            "vision-model",
            AuthKind::Bearer,
        );
        profile.capabilities = Some(ModelCapabilities {
            modalities: Some(crate::models_dev::ModalitySet {
                input: Some(["text".into(), "image".into()].into_iter().collect()),
                output: Some(["text".into()].into_iter().collect()),
            }),
            attachment: Some(true),
            tool_calls: Some(true),
            reasoning: Some(false),
        });
        profile.image_input = ImageInputMode::Native;

        let encoded = serde_json::to_value(&profile).unwrap();
        assert_eq!(encoded["capabilities"]["attachment"], true);
        assert_eq!(encoded["image_input"], "native");
        assert!(encoded.to_string().contains("vision-model"));
        assert!(!encoded.to_string().contains("Bearer"));

        let decoded: ProviderProfile = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.capabilities, profile.capabilities);
        assert_eq!(decoded.image_input, ImageInputMode::Native);
    }
}
