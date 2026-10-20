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
pub(crate) const MAX_RESPONSE_BODY_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const USER_AGENT: &str = concat!("medha/", env!("CARGO_PKG_VERSION"));

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
    let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
    while let Some(chunk) = tokio::time::timeout_at(deadline, stream.next())
        .await
        .map_err(|_| ProviderError::ProgressTimeout {
            waited_secs: READ_TIMEOUT.as_secs(),
        })?
    {
        let chunk = chunk.map_err(|e| {
            if e.is_timeout() {
                ProviderError::ProgressTimeout {
                    waited_secs: READ_TIMEOUT.as_secs(),
                }
            } else {
                ProviderError::Transport(e.without_url().to_string())
            }
        })?;
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

/// How long an open stream may deliver no event before it is treated as lost
/// and reopened, unless the profile sets its own. Keep-alive comments are not
/// events, so a connection that is merely held open still runs out. Long
/// enough for a model that writes a large tool call before sending any of it.
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// This deployment's stream wait: its own setting, or the default.
pub(crate) fn stream_idle(profile: &ProviderProfile) -> Duration {
    profile
        .stream_idle_seconds
        .map_or(STREAM_IDLE_TIMEOUT, Duration::from_secs)
}

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
    builder().build().expect("provider HTTP client")
}

fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        // Medha reads only its own MEDHA_* namespace; an ambient *_PROXY would
        // otherwise reroute model traffic and hand the credential to the proxy.
        .no_proxy()
        // Pin credentials to the configured deployment, including custom
        // authentication headers which redirect implementations may retain.
        .redirect(reqwest::redirect::Policy::none())
}

/// The connection for one server: the usual one, also trusting the
/// certificates its model was saved with.
pub(crate) fn client_for(profile: &ProviderProfile) -> Result<reqwest::Client, ProviderError> {
    configured(builder(), profile)?
        .build()
        .map_err(|_| ProviderError::Transport("could not configure model HTTP client".into()))
}

pub(crate) fn configured(
    mut builder: reqwest::ClientBuilder,
    profile: &ProviderProfile,
) -> Result<reqwest::ClientBuilder, ProviderError> {
    profile.validate().map_err(ProviderError::Decode)?;
    builder = builder
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
    if let Some(proxy) = &profile.proxy {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|_| ProviderError::Decode("invalid model proxy URL".into()))?;
        builder = builder.proxy(proxy);
    }
    if let Some(file) = &profile.ca_cert_file {
        builder = trusting(builder, file)?;
    }
    Ok(builder)
}

/// Trust the certificates in `file` as well as the public ones. A file that
/// cannot be read or holds none is an error: it is never silently ignored.
pub(crate) fn trusting(
    builder: reqwest::ClientBuilder,
    file: &str,
) -> Result<reqwest::ClientBuilder, ProviderError> {
    let unusable = |why: String| ProviderError::Certificate(why);
    let pem = std::fs::read(file).map_err(|error| {
        unusable(format!(
            "The certificate file {file} could not be read: {error}"
        ))
    })?;
    let certificates = reqwest::Certificate::from_pem_bundle(&pem).map_err(|error| {
        unusable(format!(
            "The certificate file {file} is not in PEM form: {error}"
        ))
    })?;
    if certificates.is_empty() {
        return Err(unusable(format!(
            "The certificate file {file} holds no certificate."
        )));
    }
    let trust = |builder: reqwest::ClientBuilder, one| builder.add_root_certificate(one);
    Ok(certificates.into_iter().fold(builder, trust))
}

/// Discovery/catalogue callers select a smaller byte and total-read budget.
pub(crate) async fn bounded_body(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ProviderError> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(ProviderError::Decode(
            "model discovery exceeds its byte limit".into(),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ProviderError::Transport(e.without_url().to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(ProviderError::Decode(
                "model discovery exceeds its byte limit".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Keep a silent header timeout separate from connection failures and broken
/// streams. The upstream may still be processing this request; the kernel must
/// not automatically submit the identical expensive prompt five more times.
pub(crate) async fn send_model_request(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, ProviderError> {
    let started = std::time::Instant::now();
    match request.send().await {
        Ok(response) => {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                status = response.status().as_u16(),
                "model HTTP response headers received"
            );
            Ok(response)
        }
        Err(error) => {
            let timeout = error.is_timeout();
            let connection_failure = error.is_connect();
            tracing::warn!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                timeout,
                connection_failure,
                "model HTTP request failed before response headers"
            );
            if timeout && !connection_failure {
                Err(ProviderError::ResponseTimeout {
                    waited_secs: started.elapsed().as_secs(),
                })
            } else {
                Err(connection_failed(error))
            }
        }
    }
}

/// Said when a server's certificate cannot be verified. Medha trusts the public
/// certificate authorities it was built with and has no setting to add another.
const CERTIFICATE: &str = "Medha could not verify the security certificate of the model's \
    server, so it did not connect. This fails the same way on every try. The certificate may \
    be self-signed, expired, made for another address, or issued by a private authority such \
    as a company proxy. For a server on this computer, use http:// in the model's address.";

/// A connection that failed, as an error. A certificate that cannot be verified
/// is told apart from a server that is away: only the second is worth another try.
pub(crate) fn connection_failed(error: reqwest::Error) -> ProviderError {
    match certificate_fault(&error) {
        Some(why) => ProviderError::Certificate(format!("{CERTIFICATE} Detail: {why}")),
        None if error.is_connect() => ProviderError::NotSent(error.without_url().to_string()),
        None => ProviderError::Transport(error.without_url().to_string()),
    }
}

/// The cause in an error's chain that names a certificate, when there is one.
fn certificate_fault(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut cause = error.source();
    while let Some(inner) = cause {
        let said = inner.to_string();
        if said.to_ascii_lowercase().contains("certificate") {
            return Some(said);
        }
        cause = inner.source();
    }
    None
}

pub(crate) fn stalled_stream(waited: Duration) -> ProviderError {
    ProviderError::StreamStalled {
        waited_secs: waited.as_secs(),
    }
}

/// A read from an open stream failed. The client's own read timeout means no
/// byte arrived at all, which is the same stall seen from the socket.
pub(crate) fn stream_read_failed(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        stalled_stream(READ_TIMEOUT)
    } else {
        ProviderError::Transport(error.without_url().to_string())
    }
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
    kernel::redact::remember(credential);
    for (name, value) in &profile.headers {
        kernel::redact::remember_named(name, value);
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
        AuthKind::Bearer | AuthKind::ChatGptPlan => with_bearer(request, credential),
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
        serde_json::Value::String(text) if text.starts_with("data:image/") => {
            *text = REDACTED.into();
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
#[path = "http_tests.rs"]
mod tests;
