//! OAuth for remote MCP servers: authorization-code + PKCE through the official
//! `rmcp` state machine, a one-shot loopback listener for the redirect, and
//! credentials serialized for Medha's keychain-backed token store.
//!
//! Only an explicit human action reaches [`authorize`] — it may open a browser,
//! so a model-invoked tool never can.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;
use rmcp::transport::auth::{
    AuthClient, OAuthHttpClient, OAuthHttpClientError, OAuthHttpClientFuture,
    OAuthHttpRedirectPolicy, OAuthHttpRequest, OAuthState, OAuthTokenResponse,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    time::timeout,
};

use crate::{Error, UrlSink};

/// What the browser is told once the provider redirects back.
const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Medha</title>\
<body style=\"font:16px system-ui;padding:3rem\">\
<p>Authorization complete — you can close this tab and return to Medha.</p>";
const MAX_OAUTH_BODY: usize = 1024 * 1024;
const MAX_OAUTH_REDIRECTS: usize = 5;

/// Persisted credentials. The refresh token lives here, so a remote server
/// reconnects at launch without a browser.
#[derive(Serialize, Deserialize)]
struct StoredTokens {
    client_id: String,
    token: OAuthTokenResponse,
}

/// Bearer tokens and authorization codes must not cross a plaintext hop.
/// Loopback is exempt so a locally hosted server is still usable.
pub(crate) fn require_secure(url: &str) -> Result<(), Error> {
    let parsed = url::Url::parse(url).map_err(|error| Error::BadUrl(format!("{url}: {error}")))?;
    let loopback = parsed.host_str().is_some_and(|host| {
        host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    });
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        scheme => Err(Error::BadUrl(format!(
            "'{url}' uses {scheme}; remote MCP servers must use https"
        ))),
    }
}

#[derive(Clone)]
struct EndpointPolicy {
    base: url::Url,
}

impl EndpointPolicy {
    fn new(base: &str) -> Result<Self, Error> {
        require_secure(base)?;
        let base =
            url::Url::parse(base).map_err(|error| Error::BadUrl(format!("{base}: {error}")))?;
        Ok(Self { base })
    }

    fn validate(&self, candidate: &str) -> Result<url::Url, Error> {
        let url = url::Url::parse(candidate)
            .map_err(|error| Error::BadUrl(format!("{candidate}: {error}")))?;
        require_secure(url.as_str())?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::BadUrl(
                "OAuth endpoints must not contain embedded credentials".into(),
            ));
        }
        if same_origin(&self.base, &url) {
            // The user explicitly selected the MCP origin. This keeps internal
            // HTTPS and loopback development servers usable while preventing
            // their metadata from pivoting to a different private service.
            return Ok(url);
        }
        let host = url.host_str().unwrap_or_default();
        if disallowed_host(host) {
            return Err(Error::BadUrl(format!(
                "OAuth endpoint '{url}' targets a private, loopback or metadata host"
            )));
        }
        Ok(url)
    }
}

fn same_origin(a: &url::Url, b: &url::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str()
            .zip(b.host_str())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        && a.port_or_known_default() == b.port_or_known_default()
}

fn disallowed_ipv4(addr: Ipv4Addr) -> bool {
    let octets = addr.octets();
    addr.is_private()
        || addr.is_loopback()
        || addr.is_link_local()
        || addr.is_broadcast()
        || addr.is_unspecified()
        || addr.is_multicast()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 198 && matches!(octets[1], 18 | 19))
}

fn disallowed_ipv6(addr: Ipv6Addr) -> bool {
    if let Some(mapped) = addr.to_ipv4_mapped() {
        return disallowed_ipv4(mapped);
    }
    let segments = addr.segments();
    addr.is_loopback()
        || addr.is_unspecified()
        || addr.is_multicast()
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xfe00) == 0xfc00
}

fn disallowed_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost"
        || host.ends_with(".localhost")
        || matches!(
            host.as_str(),
            "metadata" | "metadata.google.internal" | "metadata.azure.internal"
        )
    {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => disallowed_ipv4(ip),
        Ok(IpAddr::V6(ip)) => disallowed_ipv6(ip),
        Err(_) => false,
    }
}

fn client_builder(total: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(total)
}

fn follow_client(policy: EndpointPolicy, total: Duration) -> Result<reqwest::Client, Error> {
    client_builder(total)
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_OAUTH_REDIRECTS
                || policy.validate(attempt.url().as_str()).is_err()
            {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(auth_failed)
}

/// Every request emitted by rmcp's OAuth state machine comes through here:
/// discovery, dynamic registration, code exchange and refresh. Endpoint policy
/// therefore applies before any body (including a code or refresh token) leaves.
struct HardenedOAuthClient {
    follow: reqwest::Client,
    stop: reqwest::Client,
    policy: EndpointPolicy,
}

impl HardenedOAuthClient {
    fn new(base: &str, total: Duration) -> Result<Self, Error> {
        let policy = EndpointPolicy::new(base)?;
        let follow = follow_client(policy.clone(), total)?;
        let stop = client_builder(total)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(auth_failed)?;
        Ok(Self {
            follow,
            stop,
            policy,
        })
    }
}

impl OAuthHttpClient for HardenedOAuthClient {
    fn execute(&self, operation: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let OAuthHttpRequest {
                request,
                redirect_policy,
                ..
            } = operation;
            self.policy
                .validate(&request.uri().to_string())
                .map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
            let client = if matches!(redirect_policy, OAuthHttpRedirectPolicy::Follow) {
                &self.follow
            } else {
                &self.stop
            };
            let request = reqwest::Request::try_from(request)
                .map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
            let response = client
                .execute(request)
                .await
                .map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
            let mut builder = oauth2::http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
                if chunk.len() > MAX_OAUTH_BODY.saturating_sub(body.len()) {
                    return Err(OAuthHttpClientError::new(format!(
                        "OAuth response exceeds {MAX_OAUTH_BODY} bytes"
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            builder
                .body(body)
                .map_err(|error| OAuthHttpClientError::new(error.to_string()))
        })
    }
}

fn auth_failed(error: impl std::fmt::Display) -> Error {
    Error::Auth(error.to_string())
}

/// What a remote server asks for when approached without credentials.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Challenge {
    /// Nothing — the server is open, or is not answering an auth challenge.
    Open,
    /// 401 advertising protected-resource metadata: full OAuth discovery works.
    OAuth,
    /// Credentials are required but the scheme is not discoverable OAuth, so a
    /// token has to come from the user.
    Token,
}

/// Ask the server what it wants, so pasting a URL is enough to configure it.
/// Per the MCP spec an unauthorized endpoint answers 401 with
/// `WWW-Authenticate`; a `resource_metadata` parameter means OAuth discovery
/// is available and the browser flow can run unattended.
pub(crate) async fn probe(url: &str, http: Duration) -> Challenge {
    let Ok(policy) = EndpointPolicy::new(url) else {
        return Challenge::Open;
    };
    let Ok(client) = follow_client(policy, http) else {
        return Challenge::Open;
    };
    let Ok(response) = client.get(url).send().await else {
        // Unreachable hosts are a connection problem, not an auth one; let the
        // real connect attempt report it properly.
        return Challenge::Open;
    };
    if !matches!(response.status().as_u16(), 401 | 403) {
        return Challenge::Open;
    }
    let advertises_discovery = response
        .headers()
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.to_ascii_lowercase().contains("resource_metadata"));
    if advertises_discovery {
        Challenge::OAuth
    } else {
        Challenge::Token
    }
}

/// Rebuild an authorized HTTP client from persisted credentials, so a session
/// reconnects — and silently refreshes — without user interaction.
pub(crate) async fn client_from_stored(
    url: &str,
    blob: &str,
    http: Duration,
) -> Result<AuthClient<reqwest::Client>, Error> {
    let stored: StoredTokens = serde_json::from_str(blob)
        .map_err(|error| Error::Auth(format!("stored credentials are unreadable: {error}")))?;
    let oauth_http = Arc::new(HardenedOAuthClient::new(url, http)?);
    let mut state = OAuthState::new_with_oauth_http_client(url, oauth_http)
        .await
        .map_err(auth_failed)?;
    state
        .set_credentials(&stored.client_id, stored.token)
        .await
        .map_err(auth_failed)?;
    let manager = state.into_authorization_manager().ok_or_else(|| {
        Error::Auth("stored credentials did not restore an authorized session".into())
    })?;
    let resource = follow_client(EndpointPolicy::new(url)?, http)?;
    Ok(AuthClient::new(resource, manager))
}

/// Run the interactive flow: discover, open the browser, catch the loopback
/// redirect, exchange the code. Returns credentials for the token store.
pub(crate) async fn authorize(
    url: &str,
    wait: Duration,
    http: Duration,
    announce: &UrlSink,
) -> Result<String, Error> {
    require_secure(url)?;
    // Bind before starting the flow: the port is part of the redirect URI the
    // provider is asked to honour.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| Error::Auth(format!("could not open a callback listener: {error}")))?;
    let port = listener
        .local_addr()
        .map_err(|error| Error::Auth(error.to_string()))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}/callback");

    // Discovery and dynamic client registration both talk to endpoints named by
    // the server's own metadata, and both happen inside these two calls — so
    // the transport policy has to hold for the whole exchange, not just for the
    // browser URL at the end. `rmcp` drives them over `http_client`, which is
    // https-only by construction for a non-loopback host because `url` already
    // passed `require_secure` above; what is checked here is the one endpoint
    // that leaves the client entirely: where the user's browser is sent.
    let policy = EndpointPolicy::new(url)?;
    let oauth_http = Arc::new(HardenedOAuthClient::new(url, http)?);
    let mut state = OAuthState::new_with_oauth_http_client(url, oauth_http)
        .await
        .map_err(auth_failed)?;
    state
        .start_authorization(&[], &redirect, Some("Medha"))
        .await
        .map_err(auth_failed)?;
    let authorize_url = state.get_authorization_url().await.map_err(auth_failed)?;
    // Discovered from the server's own metadata, so it is not trusted input: it
    // decides where a browser is sent and where the grant is presented. Checked
    // before the announce and before the browser opens.
    policy.validate(&authorize_url)?;
    // Announce first: on a headless box the browser open is the part that fails.
    let _ = announce.send(authorize_url.clone());
    open_browser(&authorize_url);

    let callback = timeout(wait, accept_callback(listener))
        .await
        .map_err(|_| Error::Auth("timed out waiting for the browser redirect".into()))??;
    // rmcp validates the CSRF state and the RFC 9207 issuer for us.
    state
        .handle_callback_with_issuer(&callback.code, &callback.state, callback.issuer.as_deref())
        .await
        .map_err(auth_failed)?;
    let (client_id, token) = state.get_credentials().await.map_err(auth_failed)?;
    let token = token.ok_or_else(|| Error::Auth("the provider returned no token".into()))?;
    serde_json::to_string(&StoredTokens { client_id, token }).map_err(auth_failed)
}

struct Callback {
    code: String,
    state: String,
    issuer: Option<String>,
}

/// Accept the single redirect the provider sends back. Browsers also fetch
/// `/favicon.ico`, so keep listening until a request actually carries the grant.
async fn accept_callback(listener: TcpListener) -> Result<Callback, Error> {
    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .map_err(|error| Error::Auth(format!("callback listener failed: {error}")))?;
        // The listener is loopback-bound already; refuse anything else outright
        // rather than parsing a grant that came from off-box.
        if !peer.ip().is_loopback() {
            continue;
        }
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        if reader.read_line(&mut request).await.is_err() {
            continue;
        }
        let target = request.split_whitespace().nth(1).unwrap_or_default();
        let query = query_pairs(target);
        let find = |key: &str| {
            query
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };

        let outcome = match (find("code"), find("state"), find("error")) {
            (_, _, Some(error)) => Some(Err(Error::Auth(format!(
                "the provider denied authorization: {error}"
            )))),
            (Some(code), Some(state), None) => Some(Ok(Callback {
                code,
                state,
                issuer: find("iss"),
            })),
            _ => None,
        };

        let body = match &outcome {
            Some(Ok(_)) => DONE_PAGE,
            Some(Err(_)) => {
                "<!doctype html><meta charset=utf-8><p>Authorization failed — \
                 return to Medha for details."
            }
            None => "<!doctype html><meta charset=utf-8><p>Waiting for the authorization redirect…",
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut stream = reader.into_inner();
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;

        if let Some(outcome) = outcome {
            return outcome;
        }
    }
}

/// Percent-decoded query pairs from a raw request target (`/callback?a=b`).
fn query_pairs(target: &str) -> Vec<(String, String)> {
    url::Url::parse(&format!("http://127.0.0.1{target}"))
        .map(|url| {
            url.query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// Best effort — the URL is announced first, so a headless box still works.
///
/// Windows goes through `rundll32` rather than `cmd /C start`: `cmd` re-parses
/// its argument, and `&` separates commands there. Every authorization URL has
/// one between query parameters, so `start` both truncated the URL and ran
/// whatever followed as a command.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let (program, args) = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let (program, args) = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let (program, args) = ("rundll32", vec!["url.dll,FileProtocolHandler", url]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let (program, args): (&str, Vec<&str>) = ("", Vec::new());

    if !program.is_empty() {
        let _ = std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_remote_urls_are_refused() {
        assert!(require_secure("https://mcp.linear.app/mcp").is_ok());
        assert!(require_secure("http://127.0.0.1:8080/mcp").is_ok());
        assert!(require_secure("http://localhost:8080/mcp").is_ok());
        assert!(require_secure("http://mcp.example.com/mcp").is_err());
        assert!(require_secure("ftp://example.com").is_err());
        assert!(require_secure("not a url").is_err());
    }

    #[test]
    fn callback_query_is_decoded() {
        let pairs = query_pairs("/callback?code=a%2Fb&state=xyz&iss=https%3A%2F%2Fissuer");
        assert_eq!(pairs[0], ("code".into(), "a/b".into()));
        assert_eq!(pairs[1], ("state".into(), "xyz".into()));
        assert_eq!(pairs[2], ("iss".into(), "https://issuer".into()));
        assert!(query_pairs("/favicon.ico").is_empty());
    }
}
