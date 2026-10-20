//! Listing the models a server offers, bounded so a bad server cannot hang setup.

use crate::ProviderProfile;
use crate::protocol::openai_chat;
use crate::transport::http;
use kernel::{Protocol, ProviderError};
use serde::Deserialize;

/// A listed model; `context_length` is `None` when the server does not say, never guessed.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub context_length: Option<u32>,
}

/// The models a server lists; an error lets the caller fall back to typing an ID.
pub async fn list_models_for_profile(
    profile: &ProviderProfile,
    credential: &str,
) -> Result<Vec<ModelInfo>, ProviderError> {
    profile.validate().map_err(ProviderError::Decode)?;
    if !matches!(
        profile.protocol,
        Protocol::OpenAiChat | Protocol::GeminiInteractions
    ) {
        return Err(ProviderError::Decode(format!(
            "model discovery is not implemented for {}",
            profile.protocol.as_str()
        )));
    }
    if profile.auth.requires_credential() && credential.trim().is_empty() {
        return Err(ProviderError::Decode(
            "model discovery authentication requires a credential".into(),
        ));
    }
    let url = match profile.protocol {
        Protocol::OpenAiChat => openai_chat::endpoint_url(&profile.base_url, "/models")?,
        Protocol::GeminiInteractions => {
            format!("{}/models", profile.base_url.trim_end_matches('/'))
        }
        Protocol::OpenAiResponses | Protocol::AnthropicMessages => unreachable!(),
    };
    // Bounded timeout: this runs at startup before the UI paints, so a slow or
    // unreachable endpoint must not hang the launch (it falls back gracefully).
    let client = reqwest::Client::builder()
        .user_agent(http::USER_AGENT)
        .timeout(std::time::Duration::from_secs(6))
        // Model discovery carries the credential too — see transport::http::client.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
    let client = http::configured(client, profile)?;
    let client = client
        .build()
        .map_err(|e| ProviderError::Transport(e.to_string()))?;
    let discovery = async {
        match profile.protocol {
            Protocol::OpenAiChat => {
                list_openai_chat_models(&client, &url, profile, credential).await
            }
            Protocol::GeminiInteractions => {
                list_gemini_models(&client, &url, profile, credential).await
            }
            Protocol::OpenAiResponses | Protocol::AnthropicMessages => unreachable!(),
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), discovery)
        .await
        .map_err(|_| ProviderError::ProgressTimeout { waited_secs: 15 })?
}

const DISCOVERY_MAX_BYTES: usize = 8 * 1024 * 1024;
const DISCOVERY_MAX_MODELS: usize = 10_000;
const DISCOVERY_MAX_PAGES: usize = 16;

async fn list_openai_chat_models(
    client: &reqwest::Client,
    url: &str,
    profile: &ProviderProfile,
    credential: &str,
) -> Result<Vec<ModelInfo>, ProviderError> {
    #[derive(Deserialize)]
    struct ModelsResp {
        data: Vec<ModelEntry>,
    }
    // Servers spell the context window differently, and some (e.g. Groq) report
    // MORE than one of these keys at once. A serde field with `alias`es treats
    // them as one field and errors on "duplicate field" when several are present,
    // so read each independently and coalesce.
    #[derive(Deserialize)]
    struct ModelEntry {
        id: String,
        #[serde(default)]
        context_length: Option<u32>,
        #[serde(default)]
        context_window: Option<u32>,
        #[serde(default)]
        max_model_len: Option<u32>,
    }

    let resp = http::with_profile(client.get(url), profile, credential)?
        .send()
        .await
        .map_err(crate::transport::http::connection_failed)?;
    let body = http::bounded_body(http::require_success(resp).await?, DISCOVERY_MAX_BYTES).await?;
    let parsed: ModelsResp = serde_json::from_slice(&body).map_err(|_| {
        ProviderError::Decode(
            "The server returned an invalid model list; enter a model ID manually.".into(),
        )
    })?;
    if parsed.data.len() > DISCOVERY_MAX_MODELS {
        return Err(ProviderError::Decode(
            "model discovery returned too many models".into(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();
    for model in parsed.data {
        validate_discovered_id(&model.id)?;
        if seen.insert(model.id.clone()) {
            models.push(ModelInfo {
                id: model.id,
                context_length: model
                    .context_length
                    .or(model.context_window)
                    .or(model.max_model_len)
                    .filter(|limit| *limit > 0),
            });
        }
    }
    Ok(models)
}

fn validate_discovered_id(id: &str) -> Result<(), ProviderError> {
    if id.trim().is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return Err(ProviderError::Decode(
            "The server returned an invalid model ID; enter one manually.".into(),
        ));
    }
    Ok(())
}

async fn list_gemini_models(
    client: &reqwest::Client,
    url: &str,
    profile: &ProviderProfile,
    credential: &str,
) -> Result<Vec<ModelInfo>, ProviderError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ModelsResp {
        #[serde(default)]
        models: Vec<ModelEntry>,
        next_page_token: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ModelEntry {
        name: String,
        base_model_id: Option<String>,
        input_token_limit: Option<u32>,
        #[serde(default)]
        supported_generation_methods: Vec<String>,
    }

    let mut page_token: Option<String> = None;
    let mut models = std::collections::BTreeMap::<String, Option<u32>>::new();
    // Google currently permits up to 1000 entries per page. Keep pagination
    // bounded and reject a repeated token instead of looping on a malformed
    // upstream response.
    let mut seen_tokens = std::collections::HashSet::new();
    let mut pages = 0;
    let mut remaining = DISCOVERY_MAX_BYTES;
    let mut received_models = 0usize;
    loop {
        pages += 1;
        if pages > DISCOVERY_MAX_PAGES {
            return Err(ProviderError::Decode(
                "Gemini model discovery exceeded 16 pages".into(),
            ));
        }
        let mut request = client.get(url).query(&[("pageSize", "1000")]);
        if let Some(token) = page_token.as_deref() {
            request = request.query(&[("pageToken", token)]);
        }
        let resp = http::with_profile(request, profile, credential)?
            .send()
            .await
            .map_err(crate::transport::http::connection_failed)?;
        let body = http::bounded_body(http::require_success(resp).await?, remaining).await?;
        remaining -= body.len();
        let page: ModelsResp = serde_json::from_slice(&body).map_err(|_| {
            ProviderError::Decode("The server returned an invalid Gemini model list.".into())
        })?;
        received_models = received_models.saturating_add(page.models.len());
        if received_models > DISCOVERY_MAX_MODELS {
            return Err(ProviderError::Decode(
                "Gemini model discovery returned too many models".into(),
            ));
        }

        for model in page.models {
            // The Models API also returns embedding-only models. When methods
            // are advertised, exclude entries that have no generative action.
            // An empty methods list remains visible because absence is unknown,
            // not proof that Interactions is unsupported.
            let generative = model.supported_generation_methods.is_empty()
                || model.supported_generation_methods.iter().any(|method| {
                    matches!(
                        method.to_ascii_lowercase().as_str(),
                        "generatecontent" | "predict" | "interactions" | "createinteraction"
                    )
                });
            if !generative {
                continue;
            }
            let id = model
                .base_model_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| {
                    model
                        .name
                        .strip_prefix("models/")
                        .unwrap_or(&model.name)
                        .to_string()
                });
            if !id.is_empty() {
                validate_discovered_id(&id)?;
                models
                    .entry(id)
                    .and_modify(|limit| {
                        if limit.is_none() {
                            *limit = model.input_token_limit.filter(|limit| *limit > 0);
                        }
                    })
                    .or_insert(model.input_token_limit.filter(|limit| *limit > 0));
            }
        }

        let Some(token) = page.next_page_token.filter(|token| !token.is_empty()) else {
            break;
        };
        if token.len() > 4096 {
            return Err(ProviderError::Decode(
                "Gemini model discovery returned an oversized page token".into(),
            ));
        }
        if !seen_tokens.insert(token.clone()) {
            return Err(ProviderError::Decode(
                "Gemini model discovery returned a repeated page token".into(),
            ));
        }
        page_token = Some(token);
    }

    Ok(models
        .into_iter()
        .map(|(id, context_length)| ModelInfo { id, context_length })
        .collect())
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;
