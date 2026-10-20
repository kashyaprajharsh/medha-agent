use super::*;

/// An error whose words say nothing, wrapping the cause that does.
#[derive(Debug)]
struct Wrapped(&'static str, Option<Box<Wrapped>>);
impl std::fmt::Display for Wrapped {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(self.0)
    }
}
impl std::error::Error for Wrapped {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.1.as_deref().map(|inner| inner as _)
    }
}

/// A certificate that cannot be verified is found under the outer words
/// "error sending request", which are the same for a server that is away.
#[test]
fn a_certificate_fault_is_found_beneath_a_plain_connection_error() {
    let wrap = |inner: &'static str| {
        let cause = Wrapped(inner, None);
        let middle = Wrapped("client error (Connect)", Some(Box::new(cause)));
        Wrapped("error sending request", Some(Box::new(middle)))
    };
    let untrusted = wrap("invalid peer certificate: UnknownIssuer");
    assert_eq!(
        certificate_fault(&untrusted).as_deref(),
        Some("invalid peer certificate: UnknownIssuer")
    );
    assert_eq!(
        certificate_fault(&wrap("Connection refused (os error 61)")),
        None
    );
}

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
fn a_stalled_stream_is_reopened_and_waits_as_long_as_its_profile_says() {
    let mut profile =
        ProviderProfile::openai_chat("https://example.test/v1", "model", AuthKind::None);
    assert_eq!(stream_idle(&profile), STREAM_IDLE_TIMEOUT);
    let error = stalled_stream(stream_idle(&profile));
    assert!(error.is_retryable() && error.is_stream_stall());
    assert!(
        !error.is_context_overflow(),
        "compaction cannot fix a stall"
    );
    assert!(error.to_string().contains("180s"), "{error}");

    profile.stream_idle_seconds = Some(600);
    assert!(profile.validate().is_ok());
    assert_eq!(stream_idle(&profile), Duration::from_secs(600));
    for outside in [0, 29, 1801] {
        profile.stream_idle_seconds = Some(outside);
        assert!(profile.validate().is_err(), "{outside}");
    }
    // The other waits stay final: resending cannot help a listing or a
    // reply that was never a stream.
    assert!(!ProviderError::ProgressTimeout { waited_secs: 15 }.is_retryable());
}

#[tokio::test]
async fn a_silent_header_timeout_is_not_an_automatic_retry() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        std::future::pending::<()>().await;
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .read_timeout(Duration::from_millis(30))
        .build()
        .unwrap();
    let error = send_model_request(client.post(format!("http://{address}")))
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::ResponseTimeout { .. }));
    assert!(!error.is_retryable());
    assert!(!error.is_context_overflow());
    server.abort();
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

#[tokio::test]
async fn user_agent_is_sent_and_a_profile_can_override_it() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut agents = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
                assert!(request.len() <= 8192);
            }
            agents.push(
                String::from_utf8(request)
                    .unwrap()
                    .lines()
                    .filter_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("user-agent")
                            .then(|| value.trim().to_owned())
                    })
                    .collect::<Vec<_>>(),
            );
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        }
        agents
    });
    let client = client();
    let mut profile =
        ProviderProfile::openai_chat(format!("http://{address}"), "model", AuthKind::None);
    for custom in [false, true] {
        if custom {
            profile
                .headers
                .insert("User-Agent".into(), "my-deployment-client/1".into());
        }
        with_profile(client.get(&profile.base_url), &profile, "")
            .unwrap()
            .send()
            .await
            .unwrap();
    }
    assert_eq!(
        server.await.unwrap(),
        [
            vec![concat!("medha/", env!("CARGO_PKG_VERSION"))],
            vec!["my-deployment-client/1"]
        ]
    );
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
