//! OAuth for remote MCP servers: authorization-code + PKCE through the official
//! `rmcp` state machine, a one-shot loopback listener for the redirect, and
//! credentials serialized for Medha's keychain-backed token store.
//!
//! Only an explicit human action reaches [`authorize`] — it may open a browser,
//! so a model-invoked tool never can.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::StreamExt;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, OAuthHttpClient, OAuthHttpClientError,
    OAuthHttpClientFuture, OAuthHttpRedirectPolicy, OAuthHttpRequest, OAuthState,
};
use tokio::time::timeout;

use crate::{
    Error, UrlSink,
    renewal::{Persisted, RenewingClient, StoredTokens},
};

/// What the browser is told once the provider redirects back.
const MAX_OAUTH_BODY: usize = 1024 * 1024;
const MAX_OAUTH_REDIRECTS: usize = 5;

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

fn disallowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => disallowed_ipv4(ip),
        IpAddr::V6(ip) => disallowed_ipv6(ip),
    }
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

/// Every request to a server says who is asking. Some vendors refuse a request
/// that names no client outright, before it reaches their sign-in at all.
const USER_AGENT: &str = concat!("Medha/", env!("CARGO_PKG_VERSION"));

/// The client for a server that needs no sign-in of its own making.
pub(crate) fn named_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .unwrap_or_default()
}

/// Turns a host name into addresses; the system resolver in use, a fixed answer in tests.
type Lookup = Arc<
    dyn Fn(String) -> futures::future::BoxFuture<'static, std::io::Result<Vec<SocketAddr>>>
        + Send
        + Sync,
>;

fn system_lookup() -> Lookup {
    Arc::new(|host| {
        Box::pin(async move { Ok(tokio::net::lookup_host((host.as_str(), 0)).await?.collect()) })
    })
}

/// Resolves every name an OAuth request connects to.
struct AddressGuard {
    /// The host of the server the person chose.
    chosen: String,
    lookup: Lookup,
}

impl AddressGuard {
    fn for_policy(policy: &EndpointPolicy, lookup: Lookup) -> Arc<Self> {
        let chosen = policy
            .base
            .host_str()
            .unwrap_or_default()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        Arc::new(Self { chosen, lookup })
    }
}

impl reqwest::dns::Resolve for AddressGuard {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().trim_end_matches('.').to_ascii_lowercase();
        let chosen = host == self.chosen;
        let lookup = Arc::clone(&self.lookup);
        Box::pin(async move {
            let addresses = lookup(host.clone()).await?;
            // The chosen server may live anywhere; any other name must answer with public addresses only, all of them,
            // checked here where the connection is made, so a name cannot pass a check and then point elsewhere.
            if !chosen
                && let Some(inner) = addresses.iter().find(|address| disallowed_ip(address.ip()))
            {
                return Err(format!(
                    "OAuth endpoint host '{host}' resolves to a private, loopback or metadata address ({})",
                    inner.ip()
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn client_builder(total: Duration, guard: &Arc<AddressGuard>) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(total)
        .dns_resolver(Arc::clone(guard))
}

fn follow_client(
    policy: EndpointPolicy,
    total: Duration,
    guard: &Arc<AddressGuard>,
) -> Result<reqwest::Client, Error> {
    client_builder(total, guard)
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
    /// Whether the last answer was a refusal (RFC 6749 §5.2 answers 400 or
    /// 401), so a dead grant can be told from an unreachable endpoint.
    refused: Arc<AtomicBool>,
}

impl HardenedOAuthClient {
    fn new(base: &str, total: Duration) -> Result<Self, Error> {
        Self::with_lookup(base, total, system_lookup())
    }

    fn with_lookup(base: &str, total: Duration, lookup: Lookup) -> Result<Self, Error> {
        let policy = EndpointPolicy::new(base)?;
        let guard = AddressGuard::for_policy(&policy, lookup);
        let follow = follow_client(policy.clone(), total, &guard)?;
        let stop = client_builder(total, &guard)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(auth_failed)?;
        Ok(Self {
            follow,
            stop,
            policy,
            refused: Arc::new(AtomicBool::new(false)),
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
                .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
            let client = if matches!(redirect_policy, OAuthHttpRedirectPolicy::Follow) {
                &self.follow
            } else {
                &self.stop
            };
            let request = reqwest::Request::try_from(request)
                .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
            self.refused.store(false, Ordering::Release);
            let response = client
                .execute(request)
                .await
                .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
            self.refused.store(
                matches!(response.status().as_u16(), 400 | 401),
                Ordering::Release,
            );
            let mut builder = oauth2::http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
                if chunk.len() > MAX_OAUTH_BODY.saturating_sub(body.len()) {
                    return Err(OAuthHttpClientError::from(format!(
                        "OAuth response exceeds {MAX_OAUTH_BODY} bytes"
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            builder
                .body(body)
                .map_err(|error| OAuthHttpClientError::from(error.to_string()))
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
    let guard = AddressGuard::for_policy(&policy, system_lookup());
    let Ok(client) = follow_client(policy, http, &guard) else {
        return Challenge::Open;
    };
    let Ok(mut response) = client.get(url).send().await else {
        // Unreachable hosts are a connection problem, not an auth one; let the
        // real connect attempt report it properly.
        return Challenge::Open;
    };
    // Streamable HTTP servers need not serve GET, and a 405 says nothing about
    // credentials: ask again the way a client really connects.
    if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "medha", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        let Ok(posted) = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .body(initialize.to_string())
            .send()
            .await
        else {
            return Challenge::Open;
        };
        response = posted;
    }
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
/// reconnects — and silently refreshes — without user interaction. The store
/// stays attached: each refresh persists, with the token's real age.
pub(crate) async fn client_from_stored(
    url: &str,
    store: Persisted,
    http: Duration,
) -> Result<RenewingClient, Error> {
    // Unreadable is no better than absent: only a sign-in recovers either.
    if store.tokens().await.is_none() {
        return Err(Error::NeedsAuth(store.server().to_string()));
    }
    let oauth_http = HardenedOAuthClient::new(url, http)?;
    let refused = Arc::clone(&oauth_http.refused);
    let mut manager = AuthorizationManager::new_with_oauth_http_client(url, Arc::new(oauth_http))
        .await
        .map_err(auth_failed)?;
    manager.set_credential_store(store.clone());
    if !manager.initialize_from_store().await.map_err(auth_failed)? {
        return Err(Error::NeedsAuth(store.server().to_string()));
    }
    let policy = EndpointPolicy::new(url)?;
    let guard = AddressGuard::for_policy(&policy, system_lookup());
    let resource = follow_client(policy, http, &guard)?;
    Ok(RenewingClient::new(resource, manager, store, refused))
}

/// Discovers the provider and registers with it, to be redirected back to `host`.
async fn begin(
    url: &str,
    http: &Arc<HardenedOAuthClient>,
    host: &str,
    port: u16,
) -> Result<OAuthState, AuthError> {
    let mut state = OAuthState::new_with_oauth_http_client(url, http.clone()).await?;
    let redirect = format!("http://{host}:{port}/callback");
    state
        .start_authorization(AuthorizationRequest::new(redirect).with_client_name("Medha"))
        .await?;
    Ok(state)
}

/// The address is the better name for this machine: it cannot be pointed
/// elsewhere. Some providers only register the word, so it is the second try.
async fn register(
    url: &str,
    http: &Arc<HardenedOAuthClient>,
    port: u16,
) -> Result<OAuthState, AuthError> {
    match begin(url, http, "127.0.0.1", port).await {
        Err(AuthError::RegistrationFailed(_)) => begin(url, http, "localhost", port).await,
        began => began,
    }
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
    let listener = loopback::bind(None)
        .await
        .map_err(|error| Error::Auth(format!("could not open a callback listener: {error}")))?;
    let port = listener.port();

    // Discovery and registration hit endpoints named by the server's own metadata,
    // so transport policy must hold for the whole exchange. Those go over
    // `http_client` (https-only via `require_secure`); checked here is the one
    // endpoint that leaves the client — where the browser is sent.
    let policy = EndpointPolicy::new(url)?;
    let oauth_http = Arc::new(HardenedOAuthClient::new(url, http)?);
    let mut state = register(url, &oauth_http, port)
        .await
        .map_err(auth_failed)?;
    let authorize_url = state.get_authorization_url().await.map_err(auth_failed)?;
    // Discovered from the server's own metadata, so it is not trusted input: it
    // decides where a browser is sent and where the grant is presented. Checked
    // before the announce and before the browser opens.
    policy.validate(&authorize_url)?;
    // Announce first: on a headless box the browser open is the part that fails.
    let _ = announce.send(authorize_url.clone());
    loopback::open_browser(&authorize_url);

    let expected = loopback::state_of(&authorize_url).unwrap_or_default();
    let redirect = timeout(wait, listener.redirect(&expected))
        .await
        .map_err(|_| Error::Auth("timed out waiting for the browser redirect".into()))?
        .map_err(|error| Error::Auth(format!("callback listener failed: {error}")))?;
    let callback = callback(&redirect)?;
    // rmcp validates the CSRF state and the RFC 9207 issuer for us.
    state
        .handle_callback_with_issuer(&callback.code, &callback.state, callback.issuer.as_deref())
        .await
        .map_err(auth_failed)?;
    let (client_id, token) = state.get_credentials().await.map_err(auth_failed)?;
    let token = token.ok_or_else(|| Error::Auth("the provider returned no token".into()))?;
    serde_json::to_string(&StoredTokens::issued_now(client_id, token)).map_err(auth_failed)
}

struct Callback {
    code: String,
    state: String,
    issuer: Option<String>,
}

/// The grant the provider sent back, or its refusal.
fn callback(redirect: &loopback::Redirect) -> Result<Callback, Error> {
    if let Some(error) = redirect.get("error") {
        return Err(Error::Auth(format!(
            "the provider denied authorization: {error}"
        )));
    }
    let (Some(code), Some(state)) = (redirect.get("code"), redirect.get("state")) else {
        return Err(Error::Auth("the redirect carried no grant".into()));
    };
    Ok(Callback {
        code: code.into(),
        state: state.into(),
        issuer: redirect.get("iss").map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    #[test]
    fn plaintext_remote_urls_are_refused() {
        assert!(require_secure("https://mcp.linear.app/mcp").is_ok());
        assert!(require_secure("http://127.0.0.1:8080/mcp").is_ok());
        assert!(require_secure("http://localhost:8080/mcp").is_ok());
        assert!(require_secure("http://mcp.example.com/mcp").is_err());
        assert!(require_secure("ftp://example.com").is_err());
        assert!(require_secure("not a url").is_err());
    }

    /// A provider that registers a client only when it is redirected to `localhost`.
    async fn provider_that_refuses_the_address() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::AsyncReadExt;
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::clone(&asked);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut chunk = [0u8; 8192];
                // One read is the whole of these small requests once the body has arrived.
                while !String::from_utf8_lossy(&request).contains("\r\n\r\n")
                    || (request.starts_with(b"POST") && !request.ends_with(b"}"))
                {
                    match stream.read(&mut chunk).await {
                        Ok(read) if read > 0 => request.extend_from_slice(&chunk[..read]),
                        _ => break,
                    }
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
                let (status, answer) = if !request.starts_with("POST /register") {
                    ("404 Not Found", String::new())
                } else {
                    seen.lock().unwrap().push(body.to_string());
                    if body.contains("127.0.0.1") {
                        (
                            "400 Bad Request",
                            r#"{"error":"invalid_redirect_uri"}"#.into(),
                        )
                    } else {
                        let registered = r#"{"client_id":"medha-test","redirect_uris":[]}"#;
                        ("201 Created", registered.into())
                    }
                };
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                            answer.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        (format!("http://127.0.0.1:{port}/mcp"), asked)
    }

    /// A name that points at an inner address is refused when Medha connects, not by how the name reads.
    #[tokio::test]
    async fn an_oauth_host_that_resolves_to_an_inner_address_is_never_reached() {
        use tokio::io::AsyncReadExt;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let reached = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&reached);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = [0u8; 4096];
                let read = stream.read(&mut request).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&request[..read]).into_owned();
                seen.lock()
                    .unwrap()
                    .push(head.lines().next().unwrap_or_default().to_string());
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await;
            }
        });
        // Both names point at this machine, as a hostile DNS answer would.
        let lookup: Lookup =
            Arc::new(|_| Box::pin(async { Ok(vec!["127.0.0.1:0".parse().unwrap()]) }));
        let http = HardenedOAuthClient::with_lookup(
            "https://mcp.example.com/mcp",
            Duration::from_secs(5),
            lookup,
        )
        .unwrap();

        let attack = http
            .stop
            .get(format!("http://metadata.attacker.test:{port}/token"))
            .send()
            .await;
        assert!(attack.is_err(), "a name resolving to loopback was reached");
        // The server the person chose may live on this machine, as a development server does.
        let chosen = http
            .stop
            .get(format!("http://mcp.example.com:{port}/mcp"))
            .send()
            .await;
        assert!(chosen.is_ok(), "the chosen server was refused: {chosen:?}");
        assert_eq!(
            *reached.lock().unwrap(),
            ["GET /mcp HTTP/1.1"],
            "only the chosen server was reached"
        );
    }

    /// The whole sign-in, against a server whose discovery names an authorization server that resolves inward:
    /// the sign-in fails and nothing ever connects to the inner address.
    #[tokio::test]
    async fn sign_in_never_follows_discovery_to_an_address_inside_this_machine() {
        use tokio::io::AsyncReadExt;
        // The inner service the attack aims at: it records every connection it gets.
        let inner = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let inner_port = inner.local_addr().unwrap().port();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&connections);
        tokio::spawn(async move {
            while let Ok((_stream, _)) = inner.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        });
        // The MCP server the person added, which answers discovery by naming the attacker's host.
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            server.local_addr().unwrap().port()
        );
        let resource = url.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = server.accept().await {
                let mut request = [0u8; 8192];
                let read = stream.read(&mut request).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&request[..read]).into_owned();
                let path = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let (status, body) = if path.starts_with("/.well-known/oauth-protected-resource") {
                    let metadata = format!(
                        r#"{{"resource":"{resource}","authorization_servers":["https://auth.attacker.test:{inner_port}"]}}"#
                    );
                    ("200 OK", metadata)
                } else {
                    ("404 Not Found", String::new())
                };
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        // A hostile DNS answer: the attacker's host points at this machine. Names asked for are kept.
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let names = Arc::clone(&asked);
        let lookup: Lookup = Arc::new(move |host| {
            names.lock().unwrap().push(host);
            Box::pin(async { Ok(vec!["127.0.0.1:0".parse().unwrap()]) })
        });
        let http = Arc::new(
            HardenedOAuthClient::with_lookup(&url, Duration::from_secs(5), lookup).unwrap(),
        );

        let signed_in = register(&url, &http, 4242).await;
        assert!(
            signed_in.is_err(),
            "sign-in went on through an inner address"
        );
        // The flow did reach the attacker's host, so it was the address check that stopped it.
        assert!(
            asked
                .lock()
                .unwrap()
                .iter()
                .any(|host| host == "auth.attacker.test"),
            "discovery never tried the attacker's host: {:?}",
            asked.lock().unwrap()
        );
        assert_eq!(
            connections.load(Ordering::SeqCst),
            0,
            "the inner address was connected to"
        );
    }

    #[tokio::test]
    async fn a_provider_that_refuses_the_address_is_asked_again_by_name() {
        let (url, asked) = provider_that_refuses_the_address().await;
        let http = Arc::new(HardenedOAuthClient::new(&url, Duration::from_secs(5)).unwrap());
        let state = register(&url, &http, 4242).await.unwrap();
        let sent = state.get_authorization_url().await.unwrap();
        assert!(
            sent.contains("redirect_uri=http%3A%2F%2Flocalhost%3A4242%2Fcallback"),
            "{sent}"
        );
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "the address first, then the name");
        assert!(asked[0].contains("http://127.0.0.1:4242/callback"));
        assert!(asked[1].contains("http://localhost:4242/callback"));
    }
}
