//! Shared HTTP mechanics for provider protocols.
//!
//! Protocol modules own endpoint paths and wire JSON. This module owns the
//! mechanics which must behave consistently across those protocols: applying
//! credentials, bounding provider error bodies, and redacting diagnostics.

use std::time::Duration;

use futures::StreamExt;
use kernel::ProviderError;

use crate::{AuthKind, ProviderProfile};

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Bound successful non-streaming responses before allocating their full body.
pub(crate) async fn response_text(response: reqwest::Response) -> Result<String, ProviderError> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BODY_BYTES as u64)
    {
        return Err(ProviderError::Decode(
            "provider response exceeds 32 MiB".into(),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ProviderError::Transport(e.to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            return Err(ProviderError::Decode(
                "provider response exceeds 32 MiB".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes)
        .map_err(|_| ProviderError::Decode("provider response is not UTF-8".into()))
}
const REDACTED: &str = "<redacted>";

/// Connection-establishment ceiling. An endpoint that never finishes a
/// TCP/TLS handshake is unreachable, not slow.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Backstop on any single read, the wait for response headers included. Set
/// generously: a queued request legitimately takes minutes to its first byte,
/// and cutting that short turns a busy provider into a failure.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Idle ceiling between body chunks once a stream is flowing. Tighter than
/// [`READ_TIMEOUT`], because a gap here means the connection died rather than
/// that the request is still queued. The clock restarts on every chunk, so a
/// slow stream is never penalised for being long.
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Ceiling on an honoured `Retry-After`. A provider asking for an hour is
/// telling us to stop, not to sleep — past this the caller's own backoff and
/// retry budget decide, so the turn fails while the user is still watching.
const MAX_RETRY_AFTER_MS: f64 = 60_000.0;

/// The client every provider request shares. An unconfigured client waits
/// forever on an endpoint that accepts the connection and then sends nothing,
/// which is indistinguishable from a hang and cannot be retried because it
/// never errors. Deliberately no total-request timeout: that would kill a long
/// stream which is working correctly.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        // Medha reads only its own MEDHA_* namespace; an ambient *_PROXY would
        // otherwise reroute model traffic and hand the credential to the proxy.
        .no_proxy()
        .build()
        .expect("provider HTTP client")
}

/// How a stalled stream settles. `Stream` classifies as transient, so the turn
/// retries instead of surfacing a dead connection as a failed request.
pub(crate) fn stalled_stream() -> ProviderError {
    ProviderError::Stream(format!(
        "stream stalled: no data for {}s",
        STREAM_IDLE_TIMEOUT.as_secs()
    ))
}

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
    // Retry-After is scheduling metadata, not proof that the rejection is
    // transient. Some gateways attach it to 400/401 responses; promoting those
    // to throttling repeats invalid input or credentials and bypasses context
    // compaction. Honour it only for statuses already safe to retry.
    let retry_after = retry_after_status(status)
        .then(|| retry_after(response.headers()))
        .flatten();
    let status = status.as_u16();
    let body = read_error_body(response).await;
    match retry_after {
        Some(retry_after) => Err(ProviderError::Throttled {
            status,
            retry_after,
            body,
        }),
        None => Err(ProviderError::Status(status, body)),
    }
}

/// The wait a response asked for, from `retry-after-ms` or `retry-after`
/// delta-seconds. The HTTP-date form is not read: providers send it for
/// scheduled maintenance rather than rate limits, and honouring a date would
/// mean parking a turn for hours. Absent or unparsable leaves the caller on its
/// own backoff curve.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok();
    let millis = match value("retry-after-ms") {
        Some(ms) => ms,
        None => value("retry-after")? * 1000.0,
    };
    if !millis.is_finite() || millis <= 0.0 {
        return None;
    }
    Some(Duration::from_millis(millis.min(MAX_RETRY_AFTER_MS) as u64))
}

fn retry_after_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
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

    #[tokio::test]
    async fn successful_responses_are_bounded_with_or_without_content_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for announced in [true, false] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(socket.read(&mut request).await.unwrap() > 0);
                let header = if announced {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        MAX_RESPONSE_BODY_BYTES + 1
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".into()
                };
                socket.write_all(header.as_bytes()).await.unwrap();
                if !announced {
                    let chunk = vec![b'x'; 1024 * 1024];
                    for _ in 0..=32 {
                        if socket.write_all(&chunk).await.is_err() {
                            break;
                        }
                    }
                }
            });
            let response = client()
                .get(format!("http://{address}"))
                .send()
                .await
                .unwrap();
            let error = response_text(response).await.unwrap_err();
            assert!(matches!(error, ProviderError::Decode(_)), "{error}");
            assert!(error.to_string().contains("32 MiB"));
            server.await.unwrap();
        }
    }

    #[test]
    fn a_stalled_stream_retries_rather_than_failing_the_turn() {
        let error = stalled_stream();
        assert!(
            error.is_retryable(),
            "a dead connection is transient; failing the turn strands the work"
        );
        assert!(
            !error.is_context_overflow(),
            "compaction cannot fix a stall"
        );
        assert!(error.to_string().contains("90s"), "{error}");
    }

    fn headers(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn retry_after_reads_both_units_and_prefers_milliseconds() {
        assert_eq!(
            retry_after(&headers(&[("retry-after", "3")])),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            retry_after(&headers(&[("retry-after-ms", "1500")])),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            retry_after(&headers(&[
                ("retry-after-ms", "250"),
                ("retry-after", "60")
            ])),
            Some(Duration::from_millis(250)),
            "the finer unit wins when a provider sends both"
        );
        assert_eq!(
            retry_after(&headers(&[("retry-after", "2.5")])),
            Some(Duration::from_millis(2500))
        );
    }

    #[test]
    fn an_unusable_retry_after_leaves_the_caller_on_its_own_curve() {
        assert_eq!(retry_after(&headers(&[])), None);
        assert_eq!(
            retry_after(&headers(&[(
                "retry-after",
                "Wed, 21 Oct 2026 07:28:00 GMT"
            )])),
            None,
            "the HTTP-date form is deliberately not honoured"
        );
        assert_eq!(retry_after(&headers(&[("retry-after", "0")])), None);
        assert_eq!(retry_after(&headers(&[("retry-after", "-5")])), None);
        assert_eq!(retry_after(&headers(&[("retry-after", "soon")])), None);
    }

    #[test]
    fn an_outlandish_retry_after_is_capped_rather_than_parked_on() {
        assert_eq!(
            retry_after(&headers(&[("retry-after", "3600")])),
            Some(Duration::from_millis(MAX_RETRY_AFTER_MS as u64))
        );
    }

    #[test]
    fn retry_after_does_not_make_client_or_auth_errors_transient() {
        assert!(!retry_after_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!retry_after_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(retry_after_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(retry_after_status(reqwest::StatusCode::SERVICE_UNAVAILABLE));
    }

    #[test]
    fn the_shared_client_builds_with_its_ceilings() {
        // Construction is the assertion: `expect` in `client()` would panic on a
        // rejected builder, silently reinstating an unbounded client otherwise.
        let _ = client();
        assert!(STREAM_IDLE_TIMEOUT < READ_TIMEOUT, "idle must bite first");
        assert!(CONNECT_TIMEOUT < STREAM_IDLE_TIMEOUT);
    }

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
