//! Sign in with ChatGPT for open-source apps: the OAuth exchange with OpenAI, holding nothing.
//! Only a person's action should reach [`Attempt::begin`]: its link is meant for a browser.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod id_token;

pub const ISSUER: &str = "https://auth.openai.com";
pub const RESOURCE: &str = "https://api.openai.com/v1";
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
pub const CALLBACK_PATH: &str = "/auth/callback";
/// OpenAI's example port; any other free one is allowed.
pub const PREFERRED_PORT: u16 = 1455;
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const FIRST_CLIENT: &str = "dynamic_agent_client";
const AGENT_NAME: &str = "Medha";
const MAX_BODY: usize = 1024 * 1024;
/// Refresh failures that mean the grant is gone, per OpenAI's errors page.
const GONE: &[&str] = &[
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The network or OpenAI failed for now; keep the credentials and try later.
    Temporary(String),
    /// The person declined in the browser.
    Declined,
    /// The grant is gone; only a new sign-in recovers.
    SignInAgain(String),
    /// Something did not match this attempt or OpenAI's rules; nothing was kept.
    Invalid(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Temporary(why) => write!(f, "ChatGPT sign-in could not reach OpenAI: {why}"),
            Self::Declined => f.write_str("ChatGPT sign-in was declined in the browser."),
            Self::SignInAgain(why) => write!(f, "Sign in to ChatGPT again: {why}"),
            Self::Invalid(why) => write!(f, "ChatGPT sign-in failed: {why}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Http = reqwest::Client;

/// The connection sign-in uses: no ambient proxy, no redirects, a bound on every request.
pub fn http() -> Http {
    crate::transport::http::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("sign-in HTTP client")
}

/// OpenAI's published endpoints, all on the issuer's own origin.
#[derive(Debug, Clone, Deserialize)]
pub struct Issuer {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    revocation_endpoint: String,
    jwks_uri: String,
}

impl Issuer {
    pub async fn discover(http: &reqwest::Client, issuer: &str) -> Result<Self, Error> {
        let url = format!("{issuer}/.well-known/openid-configuration");
        let found: Self = read_json(http.get(url).send().await).await?;
        if found.issuer != issuer {
            return Err(Error::Invalid(format!(
                "unexpected issuer {}",
                found.issuer
            )));
        }
        for endpoint in [
            &found.authorization_endpoint,
            &found.token_endpoint,
            &found.revocation_endpoint,
            &found.jwks_uri,
        ] {
            if !same_origin(endpoint, issuer) {
                return Err(Error::Invalid(format!("{endpoint} is not on {issuer}")));
            }
        }
        Ok(found)
    }
}

fn same_origin(url: &str, issuer: &str) -> bool {
    match (reqwest::Url::parse(url), reqwest::Url::parse(issuer)) {
        (Ok(url), Ok(issuer)) => url.origin() == issuer.origin(),
        _ => false,
    }
}

/// A saved registration signing in again: its issued id and the hints that pick its account.
pub struct Returning<'a> {
    pub client_id: &'a str,
    pub id_token_hint: Option<&'a str>,
    pub login_hint: Option<&'a str>,
}

/// What the browser brought back to the callback.
#[derive(Default)]
pub struct Callback<'a> {
    pub state: Option<&'a str>,
    pub code: Option<&'a str>,
    pub error: Option<&'a str>,
    pub client_id: Option<&'a str>,
}

/// Tokens that replace each other together on every refresh.
#[derive(Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
    pub earliest_refresh_at: Option<u64>,
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

impl Tokens {
    pub fn plan_enabled(&self) -> bool {
        self.scopes.iter().any(|scope| scope == PLAN_SCOPE)
    }
}

/// A finished sign-in: whose it is, under which registration, and its tokens.
#[derive(Clone)]
pub struct Grant {
    pub client_id: String,
    pub subject: String,
    pub email: Option<String>,
    pub id_token: String,
    pub tokens: Tokens,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("client_id", &self.client_id)
            .field("subject", &self.subject)
            .field("tokens", &self.tokens)
            .finish_non_exhaustive()
    }
}

/// One sign-in attempt: fresh state, nonce and PKCE, and the exact callback address.
pub struct Attempt {
    state: String,
    nonce: String,
    verifier: String,
    redirect_uri: String,
    /// The saved registration when signing in again; `None` registers a new one.
    client_id: Option<String>,
}

impl Attempt {
    /// A fresh attempt and the link to open in the browser.
    pub fn begin(
        issuer: &Issuer,
        port: u16,
        host_id: &str,
        returning: Option<Returning<'_>>,
    ) -> Result<(Self, String), Error> {
        let attempt = Self {
            state: random()?,
            nonce: random()?,
            verifier: random()?,
            redirect_uri: format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
            client_id: returning.as_ref().map(|saved| saved.client_id.to_owned()),
        };
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(attempt.verifier.as_bytes()));
        let mut url = reqwest::Url::parse(&issuer.authorization_endpoint)
            .map_err(|error| Error::Invalid(error.to_string()))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", &attempt.redirect_uri)
                .append_pair("scope", SCOPES)
                .append_pair("resource", RESOURCE)
                .append_pair("state", &attempt.state)
                .append_pair("nonce", &attempt.nonce)
                .append_pair("code_challenge_method", "S256")
                .append_pair("code_challenge", &challenge)
                .append_pair("ext_agent_host_id", host_id);
            match &returning {
                Some(saved) => {
                    query.append_pair("client_id", saved.client_id);
                    if let Some(hint) = saved.id_token_hint {
                        query.append_pair("id_token_hint", hint);
                    }
                    if let Some(hint) = saved.login_hint {
                        query.append_pair("login_hint", hint);
                    }
                }
                None => {
                    query
                        .append_pair("client_id", FIRST_CLIENT)
                        .append_pair("agent_name_hint", AGENT_NAME);
                }
            }
        }
        Ok((attempt, url.into()))
    }

    /// Check the callback against this attempt, exchange the code, and verify who signed in.
    pub async fn finish(
        self,
        http: &reqwest::Client,
        issuer: &Issuer,
        callback: Callback<'_>,
    ) -> Result<Grant, Error> {
        if callback.state != Some(self.state.as_str()) {
            return Err(Error::Invalid(
                "the browser answered a different sign-in".into(),
            ));
        }
        match callback.error {
            Some("access_denied") => return Err(Error::Declined),
            Some(error) => return Err(Error::Invalid(format!("OpenAI answered {error}"))),
            None => {}
        }
        let client_id = match (&self.client_id, callback.client_id) {
            (None, Some(issued)) if issued != FIRST_CLIENT => issued.to_owned(),
            (None, _) => {
                return Err(Error::Invalid(
                    "OpenAI did not finish registering Medha".into(),
                ));
            }
            (Some(saved), Some(issued)) if issued != saved => {
                return Err(Error::Invalid(
                    "OpenAI answered for another registration".into(),
                ));
            }
            (Some(saved), _) => saved.clone(),
        };
        let code = callback
            .code
            .ok_or_else(|| Error::Invalid("the browser brought back no code".into()))?;
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("code", code),
            ("code_verifier", self.verifier.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("resource", RESOURCE),
        ];
        let answer: TokenAnswer =
            read_json(http.post(&issuer.token_endpoint).form(&form).send().await)
                .await
                .map_err(|error| match error {
                    Error::SignInAgain(_) => {
                        Error::Invalid("the sign-in code was used or expired; start again".into())
                    }
                    other => other,
                })?;
        let id_token = answer
            .id_token
            .clone()
            .ok_or_else(|| Error::Invalid("OpenAI sent no ID token".into()))?;
        let identity = id_token::verify(
            http,
            &issuer.jwks_uri,
            &id_token,
            &issuer.issuer,
            &client_id,
            &self.nonce,
            now(),
        )
        .await?;
        let tokens = answer.tokens(None)?;
        Ok(Grant {
            client_id,
            subject: identity.subject,
            email: identity.email,
            id_token,
            tokens,
        })
    }
}

/// Renew the access token; the refresh token rotates and must replace the saved one.
pub async fn refresh(
    http: &reqwest::Client,
    issuer: &Issuer,
    client_id: &str,
    refresh_token: &str,
) -> Result<Tokens, Error> {
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
        ("resource", RESOURCE),
    ];
    let answer: TokenAnswer =
        read_json(http.post(&issuer.token_endpoint).form(&form).send().await).await?;
    answer.tokens(Some(refresh_token))
}

/// End the renewable session; an empty 200 is success, also for a token already invalid.
pub async fn revoke(
    http: &reqwest::Client,
    issuer: &Issuer,
    client_id: &str,
    refresh_token: &str,
) -> Result<(), Error> {
    let form = [
        ("token", refresh_token),
        ("token_type_hint", "refresh_token"),
        ("client_id", client_id),
    ];
    let response = http
        .post(&issuer.revocation_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|error| Error::Temporary(error.to_string()))?;
    match response.status() {
        status if status.is_success() => Ok(()),
        status if status.is_server_error() => Err(Error::Temporary(status.to_string())),
        status => Err(Error::Invalid(format!("revocation answered {status}"))),
    }
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: u64,
    scope: Option<String>,
    earliest_refresh_at: Option<serde_json::Value>,
}

impl TokenAnswer {
    /// `kept` is the refresh token to keep when a refresh answer carries no replacement.
    fn tokens(self, kept: Option<&str>) -> Result<Tokens, Error> {
        let refresh_token = self
            .refresh_token
            .or_else(|| kept.map(str::to_owned))
            .ok_or_else(|| Error::Invalid("OpenAI sent no refresh token".into()))?;
        Ok(Tokens {
            access_token: self.access_token,
            refresh_token,
            expires_at: now() + self.expires_in,
            earliest_refresh_at: self.earliest_refresh_at.and_then(|at| at.as_u64()),
            scopes: self
                .scope
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
        })
    }
}

/// A JSON answer, or the error it carries sorted by what recovers it.
async fn read_json<T: serde::de::DeserializeOwned>(
    sent: reqwest::Result<reqwest::Response>,
) -> Result<T, Error> {
    let response = sent.map_err(|error| Error::Temporary(error.to_string()))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| Error::Temporary(error.to_string()))?;
    if body.len() > MAX_BODY {
        return Err(Error::Invalid("OpenAI's answer was too large".into()));
    }
    if status.is_server_error() {
        return Err(Error::Temporary(status.to_string()));
    }
    if !status.is_success() {
        let code = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value["error"].as_str().map(str::to_owned))
            .unwrap_or_else(|| status.to_string());
        return Err(match code.as_str() {
            gone if GONE.contains(&gone) => Error::SignInAgain(code),
            _ => Error::Invalid(code),
        });
    }
    serde_json::from_slice(&body).map_err(|error| Error::Invalid(error.to_string()))
}

/// An opaque id for this install, made once and kept: `urn:uuid:` and a random version 4 UUID.
pub fn new_host_id() -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| Error::Invalid("no secure random source".into()))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn random() -> Result<String, Error> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| Error::Invalid("no secure random source".into()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Unix seconds, as token expiry is kept.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
#[path = "chatgpt_tests.rs"]
mod tests;
