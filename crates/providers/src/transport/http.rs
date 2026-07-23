//! Shared HTTP mechanics for provider protocols.
//!
//! Protocol modules own endpoint paths and wire JSON. This module owns the
//! mechanics which must behave consistently across those protocols: applying
//! credentials, bounding provider error bodies, and redacting diagnostics.

use futures::StreamExt;
use kernel::ProviderError;

use crate::{AuthKind, ProviderProfile};

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const REDACTED: &str = "<redacted>";

/// Add bearer authentication only when a non-empty credential is present.
/// Accepting a pasted `Bearer …` value prevents a malformed double scheme at
/// the final network boundary, independently of the configuration source.
pub(crate) fn with_bearer(
    request: reqwest::RequestBuilder,
    credential: &str,
) -> reqwest::RequestBuilder {
    let credential = credential.trim();
    if credential.is_empty() || credential.eq_ignore_ascii_case("bearer") {
        return request;
    }
    let credential = match credential.split_once(char::is_whitespace) {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => token.trim(),
        _ => credential,
    };
    if credential.is_empty() {
        request
    } else {
        request.bearer_auth(credential)
    }
}

/// Apply a validated profile's non-secret headers and resolved credential.
/// Protocol code never needs to know which authentication header a deployment
/// uses.
pub(crate) fn with_profile(
    mut request: reqwest::RequestBuilder,
    profile: &ProviderProfile,
    credential: &str,
) -> Result<reqwest::RequestBuilder, ProviderError> {
    for (name, value) in &profile.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| ProviderError::Decode(format!("invalid header '{name}': {error}")))?;
        let value = reqwest::header::HeaderValue::from_str(value).map_err(|error| {
            ProviderError::Decode(format!("invalid value for header '{name}': {error}"))
        })?;
        request = request.header(name, value);
    }

    let credential = credential.trim();
    request = match profile.auth {
        AuthKind::None => request,
        AuthKind::Bearer => with_bearer(request, credential),
        AuthKind::XApiKey => request.header("x-api-key", credential),
        AuthKind::XGoogApiKey => request.header("x-goog-api-key", credential),
    };
    Ok(request)
}

/// Return a successful response or capture a bounded, redacted provider error.
/// A malicious or misconfigured upstream must not force Medha to buffer an
/// unbounded error page.
pub(crate) async fn require_success(
    response: reqwest::Response,
) -> Result<reqwest::Response, ProviderError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    Err(ProviderError::Status(
        status.as_u16(),
        read_error_body(response).await,
    ))
}

/// Capture an unsuccessful response body without exceeding the transport cap.
pub(crate) async fn read_error_body(response: reqwest::Response) -> String {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                if bytes.is_empty() {
                    return format!("<failed to read provider error body: {error}>");
                }
                truncated = true;
                break;
            }
        };
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(bytes.len());
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() == MAX_ERROR_BODY_BYTES {
            truncated = true;
            break;
        }
    }

    let mut body = String::from_utf8_lossy(&bytes).into_owned();
    body = redact_text(&body);
    if truncated {
        body.push_str("\n<provider error body truncated>");
    }
    body
}

/// Emit opt-in request diagnostics after recursively masking credential and
/// replay-state fields. Query strings are omitted because some compatible
/// services accept secrets there even though Medha does not create such URLs.
pub(crate) fn debug_json_request(method: &str, url: &str, body: &serde_json::Value) {
    if !std::env::var("MEDHA_DEBUG_HTTP").is_ok_and(|value| value == "1") {
        return;
    }
    let safe_url = reqwest::Url::parse(url)
        .map(|mut parsed| {
            parsed.set_query(None);
            parsed.set_fragment(None);
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.to_string()
        })
        .unwrap_or_else(|_| "<invalid provider URL>".to_string());
    let body = serde_json::to_string_pretty(&redacted_json(body.clone())).unwrap_or_default();
    eprintln!("\n[MEDHA_DEBUG_HTTP] {method} {safe_url}\n{body}\n");
}

fn redact_text(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => serde_json::to_string(&redacted_json(value)).unwrap_or_else(|_| body.into()),
        Err(_) => body.into(),
    }
}

fn redacted_json(mut value: serde_json::Value) -> serde_json::Value {
    match &mut value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if is_sensitive_key(key) {
                    *value = serde_json::Value::String(REDACTED.into());
                } else {
                    *value = redacted_json(std::mem::take(value));
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                *value = redacted_json(std::mem::take(value));
            }
        }
        _ => {}
    }
    value
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', ' '], "_");
    let compact = normalized.replace('_', "");
    matches!(
        compact.as_str(),
        "authorization"
            | "apikey"
            | "accesstoken"
            | "secret"
            | "signature"
            | "thoughtsignature"
            | "encryptedcontent"
            | "providerstate"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_auth_omits_empty_values_and_normalizes_a_pasted_scheme() {
        let client = reqwest::Client::new();
        let no_key = with_bearer(client.get("http://localhost"), "")
            .build()
            .unwrap();
        assert!(
            no_key
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );

        let key = with_bearer(client.get("http://localhost"), "Bearer secret")
            .build()
            .unwrap();
        assert_eq!(
            key.headers()[reqwest::header::AUTHORIZATION],
            "Bearer secret"
        );
    }

    #[test]
    fn profile_applies_declared_auth_and_custom_headers() {
        let client = reqwest::Client::new();
        let mut profile =
            ProviderProfile::openai_chat("https://example.test/v1", "model", AuthKind::XApiKey);
        profile
            .headers
            .insert("anthropic-version".into(), "2023-06-01".into());
        let request = with_profile(client.get("https://example.test"), &profile, "secret")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.headers()["x-api-key"], "secret");
        assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
        assert!(request.headers().get("authorization").is_none());
    }

    #[test]
    fn diagnostic_json_masks_nested_credentials_and_provider_state() {
        let value = serde_json::json!({
            "api_key": "key",
            "nested": [{"thoughtSignature": "not-matched"}, {"thought_signature": "signed"}],
            "content": "safe"
        });
        let redacted = redacted_json(value);
        assert_eq!(redacted["api_key"], REDACTED);
        assert_eq!(redacted["nested"][0]["thoughtSignature"], REDACTED);
        assert_eq!(redacted["nested"][1]["thought_signature"], REDACTED);
        assert_eq!(redacted["content"], "safe");
    }

    #[test]
    fn json_error_bodies_are_redacted() {
        let body = redact_text(r#"{"authorization":"secret","message":"bad request"}"#);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["authorization"], REDACTED);
        assert_eq!(value["message"], "bad request");
    }
}
