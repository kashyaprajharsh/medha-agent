//! Exact input counts from a vLLM server's own tokenizer.

use super::ProviderClient;
use crate::protocol::openai_chat;
use crate::transport::http;
use kernel::{InputTokenCount, PreparedModelRequest, TokenCountError, TokenCountQuality};

impl ProviderClient {
    pub(super) async fn count_vllm(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<InputTokenCount, TokenCountError> {
        let connection = self.connection.lock().unwrap().clone();
        let url = vllm_tokenize_url(&connection.profile.base_url);
        let body = openai_chat::vllm_tokenize_body(request);
        let client = self
            .http_for(&connection.profile)
            .map_err(|error| TokenCountError::Transport(error.to_string()))?;
        let http_request = http::with_profile(
            client.post(&url),
            &connection.profile,
            &connection.credential,
        )
        .map_err(|error| TokenCountError::Decode(error.to_string()))?;
        let resp = http_request
            .json(&body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|error| TokenCountError::Transport(error.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = http::read_error_body(resp).await;
            return Err(TokenCountError::Status(status, body));
        }
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|error| TokenCountError::Decode(error.to_string()))?;
        let tokens = value
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| TokenCountError::Decode("missing integer `count`".into()))?;
        Ok(InputTokenCount {
            tokens,
            quality: TokenCountQuality::Authoritative,
            request_fingerprint: request.request_fingerprint.clone(),
        })
    }
}

/// vLLM serves `/tokenize` at the server root, beside `/v1` rather than under it.
pub(super) fn vllm_tokenize_url(base_url: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(base_url) else {
        return format!("{}/tokenize", base_url.trim_end_matches('/'));
    };
    let path = url.path().trim_end_matches('/');
    let api_root = path.strip_suffix("/chat/completions").unwrap_or(path);
    let server_root = api_root.strip_suffix("/v1").unwrap_or(api_root);
    url.set_path(&format!("{server_root}/tokenize"));
    url.to_string()
}
