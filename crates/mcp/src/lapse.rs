//! How a live remote connection reports that its credentials no longer get it
//! in, so the manager asks for them instead of staying "ready" while every
//! call fails. Some servers only check credentials per call: the handshake and
//! tool list succeed without any, and the first refusal is on `tools/call`.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use futures::stream::BoxStream;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::Sse;

use crate::{Error, ServerState};

type HttpError = StreamableHttpError<reqwest::Error>;

/// What the connection now needs from a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Needs {
    /// A stored OAuth grant the provider refused.
    SignInAgain,
    /// A server that turned out to want an OAuth sign-in.
    SignIn,
    /// A server that wants a token, or refused the one configured.
    Token,
}

impl Needs {
    pub(crate) fn state(self) -> ServerState {
        match self {
            Needs::SignInAgain | Needs::SignIn => ServerState::NeedsAuth,
            Needs::Token => ServerState::NeedsToken,
        }
    }

    pub(crate) fn detail(self) -> &'static str {
        match self {
            Needs::SignInAgain => "sign-in expired",
            Needs::SignIn => "the server asked for a sign-in",
            Needs::Token => "the server refused the request without a valid token",
        }
    }

    pub(crate) fn error(self, server: String) -> Error {
        match self {
            Needs::SignInAgain | Needs::SignIn => Error::NeedsAuth(server),
            Needs::Token => Error::NeedsToken(server),
        }
    }
}

/// Latched once by a connection's HTTP client; the first cause wins.
#[derive(Debug, Default)]
pub(crate) struct Lapse(AtomicU8);

impl Lapse {
    pub(crate) fn set(&self, needs: Needs) {
        let _ = self
            .0
            .compare_exchange(0, needs as u8 + 1, Ordering::AcqRel, Ordering::Acquire);
    }

    pub(crate) fn get(&self) -> Option<Needs> {
        match self.0.load(Ordering::Acquire) {
            1 => Some(Needs::SignInAgain),
            2 => Some(Needs::SignIn),
            3 => Some(Needs::Token),
            _ => None,
        }
    }
}

/// An unauthenticated or bearer client that notices a server asking for
/// credentials it was never given, or refusing the ones it was.
#[derive(Clone)]
pub(crate) struct ChallengeWatch {
    http: reqwest::Client,
    lapse: Arc<Lapse>,
    /// A configured token was refused: only a new token helps, even if the
    /// server also advertises OAuth.
    bearer: bool,
}

impl ChallengeWatch {
    pub(crate) fn new(http: reqwest::Client, bearer: bool) -> Self {
        Self {
            http,
            lapse: Arc::default(),
            bearer,
        }
    }

    pub(crate) fn lapse(&self) -> Arc<Lapse> {
        Arc::clone(&self.lapse)
    }
}

impl StreamableHttpClient for ChallengeWatch {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let result = self
            .http
            .post_message(uri, message, session_id, auth_header, custom_headers)
            .await;
        if let Err(StreamableHttpError::AuthRequired(challenge)) = &result {
            let discoverable = challenge
                .www_authenticate_header
                .to_ascii_lowercase()
                .contains("resource_metadata");
            self.lapse.set(if discoverable && !self.bearer {
                Needs::SignIn
            } else {
                Needs::Token
            });
        }
        result
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        self.http
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, HttpError> {
        self.http
            .get_stream(uri, session_id, last_event_id, auth_header, custom_headers)
            .await
    }
}
