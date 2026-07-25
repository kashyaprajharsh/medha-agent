//! OAuth for remote MCP servers: authorization-code + PKCE through the official
//! `rmcp` state machine, a one-shot loopback listener for the redirect, and
//! credentials serialized for Medha's keychain-backed token store.
//!
//! Only an explicit human action reaches [`authorize`] — it may open a browser,
//! so a model-invoked tool never can.

use std::{net::IpAddr, time::Duration};

use rmcp::transport::auth::{AuthClient, OAuthState, OAuthTokenResponse};
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

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

fn auth_failed(error: impl std::fmt::Display) -> Error {
    Error::Auth(error.to_string())
}

/// Rebuild an authorized HTTP client from persisted credentials, so a session
/// reconnects — and silently refreshes — without user interaction.
pub(crate) async fn client_from_stored(
    url: &str,
    blob: &str,
) -> Result<AuthClient<reqwest::Client>, Error> {
    let stored: StoredTokens = serde_json::from_str(blob)
        .map_err(|error| Error::Auth(format!("stored credentials are unreadable: {error}")))?;
    let mut state = OAuthState::new(url, Some(http_client()))
        .await
        .map_err(auth_failed)?;
    state
        .set_credentials(&stored.client_id, stored.token)
        .await
        .map_err(auth_failed)?;
    let manager = state.into_authorization_manager().ok_or_else(|| {
        Error::Auth("stored credentials did not restore an authorized session".into())
    })?;
    Ok(AuthClient::new(http_client(), manager))
}

/// Run the interactive flow: discover, open the browser, catch the loopback
/// redirect, exchange the code. Returns credentials for the token store.
pub(crate) async fn authorize(
    url: &str,
    wait: Duration,
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

    let mut state = OAuthState::new(url, Some(http_client()))
        .await
        .map_err(auth_failed)?;
    state
        .start_authorization(&[], &redirect, Some("Medha"))
        .await
        .map_err(auth_failed)?;
    let authorize_url = state.get_authorization_url().await.map_err(auth_failed)?;
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
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let (program, args) = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let (program, args) = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let (program, args) = ("cmd", vec!["/C", "start", "", url]);
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
