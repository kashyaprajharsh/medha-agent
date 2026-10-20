//! A fake OpenAI sign-in server on 127.0.0.1, and ID tokens signed with a throwaway test key.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair, RsaPublicKeyComponents},
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Made only to sign test ID tokens; it signs nothing real.
const KEY: &[u8] = include_bytes!("../fixtures/test-only-signing-key.pk8");
pub const KID: &str = "test-key";

pub fn b64(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The published key set, holding the test key under `kid`.
pub fn jwks(kid: &str) -> String {
    let pair = RsaKeyPair::from_pkcs8(KEY).expect("test key");
    let public = RsaPublicKeyComponents::<Vec<u8>>::from(pair.public());
    json!({"keys": [{"kid": kid, "kty": "RSA", "alg": "RS256", "n": b64(&public.n), "e": b64(&public.e)}]})
        .to_string()
}

/// A JWT over `header` and `claims`, signed RS256 with the test key.
pub fn sign(header: &Value, claims: &Value) -> String {
    let signed = format!("{}.{}", b64(header.to_string()), b64(claims.to_string()));
    let pair = RsaKeyPair::from_pkcs8(KEY).expect("test key");
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signed.as_bytes(),
        &mut signature,
    )
    .expect("sign");
    format!("{signed}.{}", b64(signature))
}

/// One sign-in answer from the fake token endpoint.
pub struct Grant<'a> {
    pub client_id: &'a str,
    pub subject: &'a str,
    pub nonce: &'a str,
    pub scope: &'a str,
    pub refresh: &'a str,
    pub expires_in: u64,
}

#[derive(Default)]
struct State {
    /// Answers for the token endpoint, in order: status line and JSON body.
    tokens: VecDeque<(&'static str, Value)>,
    revoke: Option<&'static str>,
    /// Method and path, then the form body.
    seen: Vec<(String, String)>,
}

/// OpenAI's sign-in endpoints at `url`: discovery, keys, token and revoke.
pub struct FakeOpenAi {
    pub url: String,
    state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeOpenAi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeOpenAi {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let state = Arc::new(Mutex::new(State::default()));
        let (base, shared) = (url.clone(), state.clone());
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(answer(socket, base.clone(), shared.clone()));
            }
        });
        Self { url, state, task }
    }

    /// The next token-endpoint answer.
    pub fn token(&self, status: &'static str, body: Value) {
        self.state.lock().unwrap().tokens.push_back((status, body));
    }

    /// A token answer signing in `grant.subject`; its access token is `access-` and the refresh token.
    pub fn grant(&self, grant: Grant<'_>) {
        let claims = json!({"iss": self.url, "aud": grant.client_id, "sub": grant.subject, "email": format!("{}@example.test", grant.subject), "nonce": grant.nonce, "exp": now() + 3600});
        let id_token = sign(&json!({"alg": "RS256", "kid": KID}), &claims);
        self.token(
            "200 OK",
            json!({"access_token": format!("access-{}", grant.refresh), "refresh_token": grant.refresh, "id_token": id_token, "token_type": "Bearer", "expires_in": grant.expires_in, "scope": grant.scope}),
        );
    }

    pub fn revoke_answers(&self, status: &'static str) {
        self.state.lock().unwrap().revoke = Some(status);
    }

    /// Requests seen so far: method and path, and the form body.
    pub fn seen(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().seen.clone()
    }
}

async fn answer(mut socket: tokio::net::TcpStream, base: String, state: Arc<Mutex<State>>) {
    let mut input = Vec::new();
    let (head, body) = loop {
        let mut chunk = [0; 8192];
        let Ok(read) = socket.read(&mut chunk).await else {
            return;
        };
        if read == 0 {
            return;
        }
        input.extend_from_slice(&chunk[..read]);
        let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&input[..end]).into_owned();
        let length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0);
        if input.len() >= end + 4 + length {
            break (
                head,
                String::from_utf8_lossy(&input[end + 4..end + 4 + length]).into_owned(),
            );
        }
    };
    let request = head
        .lines()
        .next()
        .unwrap_or_default()
        .rsplit_once(' ')
        .map_or("", |(start, _)| start)
        .to_owned();
    let (status, reply) = {
        let mut state = state.lock().unwrap();
        state.seen.push((request.clone(), body));
        match request.as_str() {
            "GET /.well-known/openid-configuration" => ("200 OK", json!({"issuer": base, "authorization_endpoint": format!("{base}/api/accounts/authorize"), "token_endpoint": format!("{base}/api/accounts/oauth/token"), "revocation_endpoint": format!("{base}/api/accounts/oauth/revoke"), "jwks_uri": format!("{base}/.well-known/jwks.json")}).to_string()),
            "GET /.well-known/jwks.json" => ("200 OK", jwks(KID)),
            "POST /api/accounts/oauth/token" => state.tokens.pop_front().map_or(("500 Internal Server Error", "{}".into()), |(status, body)| (status, body.to_string())),
            "POST /api/accounts/oauth/revoke" => (state.revoke.unwrap_or("200 OK"), String::new()),
            _ => ("404 Not Found", "{}".into()),
        }
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}
