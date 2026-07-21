//! Serializable provider deployment profiles.
//!
//! A profile selects a wire protocol and describes how to reach one model. It
//! never contains the credential itself: callers resolve that separately from
//! the secret store and hand it to [`ProviderClient`](crate::ProviderClient).

use kernel::{Protocol, ReasoningSupport, TokenAccountingMode};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

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
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let base_url = self.base_url.trim();
        if base_url.is_empty() {
            return Err("provider base URL cannot be empty".into());
        }
        let parsed = reqwest::Url::parse(base_url)
            .map_err(|error| format!("invalid provider base URL '{base_url}': {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!(
                "provider base URL must use http or https, not '{}'",
                parsed.scheme()
            ));
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
