//! Keeps a signed-in remote server signed in across expiry, restarts and
//! processes.
//!
//! Credentials are read through the host's [`TokenStore`] on every use rather
//! than cached, so a refresh made by another Medha process sharing the keychain
//! is picked up instead of raced, and every refresh rmcp performs — rotated
//! refresh token and issue time included — is written straight back.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use futures::stream::BoxStream;
use oauth2::TokenResponse;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::{
    auth::{
        AuthError, AuthorizationManager, CredentialStore, OAuthTokenResponse, StoredCredentials,
    },
    streamable_http_client::{
        SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use serde::{Deserialize, Serialize};
use sse_stream::Sse;
use tokio::sync::Mutex;

use crate::{
    TokenStore,
    lapse::{Lapse, Needs},
};

type HttpError = StreamableHttpError<reqwest::Error>;

/// The persisted grant. `received_at` anchors `expires_in`; blobs written
/// before it existed load without it and are renewed on their first refusal.
#[derive(Serialize, Deserialize)]
pub(crate) struct StoredTokens {
    client_id: String,
    token: OAuthTokenResponse,
    #[serde(default)]
    granted_scopes: Vec<String>,
    #[serde(default)]
    received_at: Option<u64>,
}

impl StoredTokens {
    pub(crate) fn issued_now(client_id: String, token: OAuthTokenResponse) -> Self {
        Self {
            granted_scopes: scopes_of(&token),
            client_id,
            token,
            received_at: Some(now_epoch_secs()),
        }
    }

    fn into_credentials(self) -> StoredCredentials {
        let granted_scopes = if self.granted_scopes.is_empty() {
            scopes_of(&self.token)
        } else {
            self.granted_scopes
        };
        StoredCredentials::new(
            self.client_id,
            Some(self.token),
            granted_scopes,
            self.received_at,
        )
    }
}

fn scopes_of(token: &OAuthTokenResponse) -> Vec<String> {
    token
        .scopes()
        .map(|scopes| scopes.iter().map(|scope| scope.to_string()).collect())
        .unwrap_or_default()
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// rmcp's credential store, backed by one server's entry in the host store.
#[derive(Clone)]
pub(crate) struct Persisted {
    store: Arc<dyn TokenStore>,
    server: String,
    url: String,
}

impl Persisted {
    pub(crate) fn new(store: Arc<dyn TokenStore>, server: &str, url: &str) -> Self {
        Self {
            store,
            server: server.to_string(),
            url: url.to_string(),
        }
    }

    pub(crate) fn server(&self) -> &str {
        &self.server
    }

    /// Keychain access can block, so it never runs on a runtime worker.
    async fn blocking<T: Send + 'static>(
        &self,
        op: impl FnOnce(&dyn TokenStore, &str, &str) -> T + Send + 'static,
    ) -> Result<T, AuthError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || op(this.store.as_ref(), &this.server, &this.url))
            .await
            .map_err(|error| AuthError::InternalError(error.to_string()))
    }

    /// `None` when absent or unreadable: either way only a sign-in recovers it.
    pub(crate) async fn tokens(&self) -> Option<StoredTokens> {
        let blob = self
            .blocking(|store, server, url| store.load(server, url))
            .await
            .ok()??;
        serde_json::from_str(&blob).ok()
    }
}

#[async_trait::async_trait]
impl CredentialStore for Persisted {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        Ok(self.tokens().await.map(StoredTokens::into_credentials))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let Some(token) = credentials.token_response else {
            return Ok(());
        };
        let stored = StoredTokens {
            client_id: credentials.client_id,
            token,
            granted_scopes: credentials.granted_scopes,
            received_at: credentials.token_received_at,
        };
        let blob = serde_json::to_string(&stored)
            .map_err(|error| AuthError::InternalError(error.to_string()))?;
        self.blocking(move |store, server, url| store.save(server, url, &blob))
            .await
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.blocking(|store, server, url| store.clear(server, url))
            .await
    }
}

/// Streamable HTTP client that presents the stored bearer and, when a server
/// refuses it, recovers once: a newer token another holder already saved, else
/// a refresh. `lapse` latches once the grant is provably gone, so the manager
/// can ask for a sign-in instead of reporting a live server that fails.
#[derive(Clone)]
pub(crate) struct RenewingClient {
    http: reqwest::Client,
    manager: Arc<Mutex<AuthorizationManager>>,
    store: Persisted,
    /// Set by the OAuth HTTP client when the last token-endpoint answer was a
    /// refusal, as opposed to the endpoint being unreachable.
    refused: Arc<AtomicBool>,
    lapse: Arc<Lapse>,
}

impl RenewingClient {
    pub(crate) fn new(
        http: reqwest::Client,
        manager: AuthorizationManager,
        store: Persisted,
        refused: Arc<AtomicBool>,
    ) -> Self {
        Self {
            http,
            manager: Arc::new(Mutex::new(manager)),
            store,
            refused,
            lapse: Arc::default(),
        }
    }

    pub(crate) fn lapse(&self) -> Arc<Lapse> {
        Arc::clone(&self.lapse)
    }

    fn ensure_granted(&self) -> Result<(), HttpError> {
        if self.lapse.get().is_some() {
            return Err(AuthError::AuthorizationRequired.into());
        }
        Ok(())
    }

    /// Current bearer; rmcp refreshes it first when it is about to expire.
    async fn bearer(&self) -> Result<String, HttpError> {
        self.ensure_granted()?;
        let manager = self.manager.lock().await;
        match manager.get_access_token().await {
            Ok(token) => Ok(token),
            Err(error) => Err(self.judge(error).await),
        }
    }

    /// Replace a bearer the server refused.
    async fn renew(&self, refused: &str) -> Result<String, HttpError> {
        let manager = self.manager.lock().await;
        // Another call, or another Medha process, may have renewed it already;
        // refreshing again would spend a rotated refresh token for nothing.
        if let Some(current) = self.store.tokens().await
            && current.token.access_token().secret() != refused
        {
            return Ok(current.token.access_token().secret().clone());
        }
        match manager.refresh_token().await {
            Ok(token) => Ok(token.access_token().secret().clone()),
            Err(error) => Err(self.judge(error).await),
        }
    }

    /// rmcp folds an unreachable token endpoint into "authorization required",
    /// so only a provider's explicit refusal, or no refresh token left to try,
    /// counts as a lost grant. Anything else stays retryable. Called with the
    /// manager lock held, so `refused` describes this refresh and no other.
    async fn judge(&self, error: AuthError) -> HttpError {
        if matches!(
            error,
            AuthError::AuthorizationRequired | AuthError::TokenRefreshFailed(_)
        ) {
            let refreshable = self
                .store
                .tokens()
                .await
                .is_some_and(|stored| stored.token.refresh_token().is_some());
            if !refreshable || self.refused.load(Ordering::Acquire) {
                self.lapse.set(Needs::SignInAgain);
                tracing::info!(target: "medha_mcp", server = %self.store.server(), %error, "MCP sign-in lapsed");
            }
        }
        error.into()
    }
}

impl StreamableHttpClient for RenewingClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let bearer = self.bearer().await?;
        let first = self
            .http
            .post_message(
                Arc::clone(&uri),
                message.clone(),
                session_id.clone(),
                Some(bearer.clone()),
                custom_headers.clone(),
            )
            .await;
        let Err(StreamableHttpError::AuthRequired(_)) = first else {
            return first;
        };
        let bearer = self.renew(&bearer).await?;
        let second = self
            .http
            .post_message(uri, message, session_id, Some(bearer), custom_headers)
            .await;
        if matches!(second, Err(StreamableHttpError::AuthRequired(_))) {
            // Refused even when fresh: only a new sign-in can satisfy it.
            self.lapse.set(Needs::SignInAgain);
        }
        second
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        let bearer = self.bearer().await?;
        self.http
            .delete_session(uri, session_id, Some(bearer), custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, HttpError> {
        let bearer = self.bearer().await?;
        self.http
            .get_stream(uri, session_id, last_event_id, Some(bearer), custom_headers)
            .await
    }
}
