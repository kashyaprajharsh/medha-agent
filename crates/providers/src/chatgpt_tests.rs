use super::*;
use crate::test_http::{Response, Server};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair, RsaPublicKeyComponents};
use serde_json::{Value, json};
use std::collections::HashMap;

/// A throwaway key made only to sign these tests' ID tokens.
const KEY: &[u8] = include_bytes!("fixtures/test-only-signing-key.pk8");
const PLAN: &str = "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke";

fn b64(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn jwks(kid: &str) -> String {
    let pair = RsaKeyPair::from_pkcs8(KEY).unwrap();
    let public = RsaPublicKeyComponents::<Vec<u8>>::from(pair.public());
    json!({"keys": [{"kid": kid, "kty": "RSA", "alg": "RS256", "n": b64(&public.n), "e": b64(&public.e)}]})
        .to_string()
}

fn sign(header: &Value, claims: &Value) -> String {
    let signed = format!("{}.{}", b64(header.to_string()), b64(claims.to_string()));
    let pair = RsaKeyPair::from_pkcs8(KEY).unwrap();
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signed.as_bytes(),
        &mut signature,
    )
    .unwrap();
    format!("{signed}.{}", b64(signature))
}

fn claims(nonce: &str) -> Value {
    json!({"iss": ISSUER, "aud": "oaiapp_1", "sub": "user-1", "email": "a@b.test", "nonce": nonce, "exp": now() + 3600})
}

fn issuer(base: &str) -> Issuer {
    Issuer {
        issuer: ISSUER.into(),
        authorization_endpoint: format!("{base}/api/accounts/authorize"),
        token_endpoint: format!("{base}/api/accounts/oauth/token"),
        revocation_endpoint: format!("{base}/api/accounts/oauth/revoke"),
        jwks_uri: format!("{base}/.well-known/jwks.json"),
    }
}

fn query(url: &str) -> HashMap<String, String> {
    reqwest::Url::parse(url)
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

fn answer(status: &'static str, body: Value) -> Response {
    Response {
        status,
        ..Response::json(body.to_string())
    }
}

/// A first sign-in against a fake OpenAI granting `scope`: the link, the grant, and what was sent.
async fn first_sign_in(scope: &str) -> (HashMap<String, String>, Grant, Vec<(String, Vec<u8>)>) {
    let (attempt, link) = Attempt::begin(
        &issuer("http://unused.test"),
        PREFERRED_PORT,
        "urn:uuid:host-1",
        None,
    )
    .unwrap();
    let link = query(&link);
    let id_token = sign(
        &json!({"alg": "RS256", "kid": "k1"}),
        &claims(&link["nonce"]),
    );
    let server = Server::scripted(vec![
        answer(
            "200 OK",
            json!({"access_token": "a1", "refresh_token": "r1", "id_token": id_token, "token_type": "Bearer", "expires_in": 3600, "scope": scope}),
        ),
        Response::json(jwks("k1")),
    ])
    .await;
    let callback = Callback {
        state: Some(&link["state"]),
        code: Some("code-1"),
        client_id: Some("oaiapp_1"),
        ..Callback::default()
    };
    let grant = attempt
        .finish(&http(), &issuer(&server.url), callback)
        .await
        .unwrap();
    let sent = server.requests.lock().unwrap().clone();
    (link, grant, sent)
}

#[test]
fn an_id_token_is_trusted_only_with_openais_signature_and_this_attempts_claims() {
    let keys: id_token::Keys = serde_json::from_str(&jwks("k1")).unwrap();
    let header = json!({"alg": "RS256", "kid": "k1"});
    let check = |token: &str| {
        id_token::check(token, &keys, ISSUER, "oaiapp_1", "n1", now()).map(|who| who.subject)
    };
    assert_eq!(check(&sign(&header, &claims("n1"))), Ok("user-1".into()));
    let mut listed = claims("n1");
    listed["aud"] = json!(["other", "oaiapp_1"]);
    assert_eq!(check(&sign(&header, &listed)), Ok("user-1".into()));

    let genuine = sign(&header, &claims("n1"));
    let parts: Vec<&str> = genuine.split('.').collect();
    let mut forged = claims("n1");
    forged["sub"] = json!("user-2");
    let tampered = format!("{}.{}.{}", parts[0], b64(forged.to_string()), parts[2]);
    let with = |key: &str, value: Value| {
        let mut changed = claims("n1");
        changed[key] = value;
        sign(&header, &changed)
    };
    let refused = [
        ("claims changed after signing", tampered),
        (
            "a key OpenAI does not publish",
            sign(&json!({"alg": "RS256", "kid": "k2"}), &claims("n1")),
        ),
        (
            "no algorithm",
            sign(&json!({"alg": "none", "kid": "k1"}), &claims("n1")),
        ),
        (
            "a shared-secret algorithm",
            sign(&json!({"alg": "HS256", "kid": "k1"}), &claims("n1")),
        ),
        (
            "another issuer",
            with("iss", json!("https://auth.evil.test")),
        ),
        ("another client", with("aud", json!("oaiapp_2"))),
        ("expired", with("exp", json!(now() - 3600))),
        ("another sign-in's nonce", with("nonce", json!("n2"))),
        ("no account", with("sub", json!(""))),
        ("not a JWT", "a.b".into()),
    ];
    for (why, token) in refused {
        assert!(check(&token).is_err(), "accepted a token with {why}");
    }
}

#[tokio::test]
async fn a_first_sign_in_registers_medha_and_proves_who_signed_in() {
    let (link, grant, sent) = first_sign_in(PLAN).await;
    for (key, value) in [
        ("client_id", "dynamic_agent_client"),
        ("agent_name_hint", "Medha"),
        ("ext_agent_host_id", "urn:uuid:host-1"),
        ("redirect_uri", "http://127.0.0.1:1455/auth/callback"),
        ("scope", SCOPES),
        ("resource", RESOURCE),
        ("response_type", "code"),
        ("code_challenge_method", "S256"),
    ] {
        assert_eq!(link.get(key).map(String::as_str), Some(value), "{key}");
    }
    assert_eq!(
        (grant.client_id.as_str(), grant.subject.as_str()),
        ("oaiapp_1", "user-1")
    );
    assert!(grant.tokens.plan_enabled());
    assert_eq!(grant.tokens.refresh_token, "r1");
    let shown = format!("{grant:?}");
    assert!(!shown.contains("a1") && !shown.contains("r1") && !shown.contains(&grant.id_token));

    // The code goes back with the issued id, the same callback, and the verifier behind the challenge.
    let exchange: HashMap<String, String> = reqwest::Url::parse(&format!(
        "http://x/?{}",
        String::from_utf8_lossy(&sent[0].1)
    ))
    .unwrap()
    .query_pairs()
    .map(|(key, value)| (key.into_owned(), value.into_owned()))
    .collect();
    assert!(sent[0].0.starts_with("POST /api/accounts/oauth/token"));
    assert_eq!(exchange["client_id"], "oaiapp_1");
    assert_eq!(exchange["redirect_uri"], link["redirect_uri"]);
    assert_eq!(exchange["resource"], RESOURCE);
    assert_eq!(
        b64(Sha256::digest(exchange["code_verifier"].as_bytes())),
        link["code_challenge"]
    );
    assert!(sent[1].0.starts_with("GET /.well-known/jwks.json"));
}

#[tokio::test]
async fn without_the_plan_permission_the_sign_in_is_kept_but_plan_use_is_off() {
    let (_, grant, _) = first_sign_in("openid profile email offline_access").await;
    assert_eq!(grant.subject, "user-1");
    assert!(!grant.tokens.plan_enabled());
}

#[tokio::test]
async fn signing_in_again_reuses_the_registration_and_names_the_account() {
    let saved = Returning {
        client_id: "oaiapp_1",
        id_token_hint: Some("old.id.token"),
        login_hint: Some("a@b.test"),
    };
    let (_, link) = Attempt::begin(
        &issuer("http://unused.test"),
        4000,
        "urn:uuid:host-1",
        Some(saved),
    )
    .unwrap();
    let link = query(&link);
    assert_eq!(link["client_id"], "oaiapp_1");
    assert_eq!(link["id_token_hint"], "old.id.token");
    assert_eq!(link["login_hint"], "a@b.test");
    assert_eq!(link["redirect_uri"], "http://127.0.0.1:4000/auth/callback");
    assert!(!link.contains_key("agent_name_hint"));
}

/// A saved registration (if any), the callback built from the attempt's state, and the refusal.
type Mismatch = (Option<Returning<'static>>, fn(&str) -> Callback<'_>, Error);

#[tokio::test]
async fn a_callback_that_does_not_match_this_attempt_is_refused_before_any_request() {
    let server = Server::scripted(Vec::new()).await;
    let returning = || Returning {
        client_id: "oaiapp_1",
        id_token_hint: None,
        login_hint: None,
    };
    let cases: [Mismatch; 5] = [
        (
            None,
            |_| Callback {
                state: Some("other"),
                code: Some("c"),
                client_id: Some("oaiapp_1"),
                ..Callback::default()
            },
            Error::Invalid("the browser answered a different sign-in".into()),
        ),
        (
            None,
            |state| Callback {
                state: Some(state),
                error: Some("access_denied"),
                ..Callback::default()
            },
            Error::Declined,
        ),
        (
            None,
            |state| Callback {
                state: Some(state),
                code: Some("c"),
                ..Callback::default()
            },
            Error::Invalid("OpenAI did not finish registering Medha".into()),
        ),
        (
            None,
            |state| Callback {
                state: Some(state),
                code: Some("c"),
                client_id: Some("dynamic_agent_client"),
                ..Callback::default()
            },
            Error::Invalid("OpenAI did not finish registering Medha".into()),
        ),
        (
            Some(returning()),
            |state| Callback {
                state: Some(state),
                code: Some("c"),
                client_id: Some("oaiapp_2"),
                ..Callback::default()
            },
            Error::Invalid("OpenAI answered for another registration".into()),
        ),
    ];
    for (saved, callback, expected) in cases {
        let (attempt, link) =
            Attempt::begin(&issuer(&server.url), 4000, "urn:uuid:host-1", saved).unwrap();
        let state = query(&link)["state"].clone();
        let result = attempt
            .finish(&http(), &issuer(&server.url), callback(&state))
            .await;
        assert_eq!(result.unwrap_err(), expected);
    }
    assert!(
        server.requests.lock().unwrap().is_empty(),
        "a mismatched callback reached OpenAI"
    );
}

#[tokio::test]
async fn refresh_and_revocation_failures_are_sorted_by_what_recovers_them() {
    let fresh =
        json!({"access_token": "a2", "refresh_token": "r2", "expires_in": 3600, "scope": PLAN});
    let unrotated = json!({"access_token": "a2", "expires_in": 3600, "scope": PLAN});
    let refreshes = [
        (answer("200 OK", fresh), Ok("r2")),
        (answer("200 OK", unrotated), Ok("r1")),
        (
            answer("400 Bad Request", json!({"error": "refresh_token_reused"})),
            Err(Error::SignInAgain("refresh_token_reused".into())),
        ),
        (
            answer("400 Bad Request", json!({"error": "invalid_grant"})),
            Err(Error::SignInAgain("invalid_grant".into())),
        ),
        (
            answer("401 Unauthorized", json!({"error": "invalid_client"})),
            Err(Error::Invalid("invalid_client".into())),
        ),
        (
            answer("503 Service Unavailable", json!({})),
            Err(Error::Temporary("503 Service Unavailable".into())),
        ),
    ];
    for (response, expected) in refreshes {
        let server = Server::scripted(vec![response]).await;
        let renewed = refresh(&http(), &issuer(&server.url), "oaiapp_1", "r1").await;
        assert_eq!(
            renewed
                .map(|tokens| tokens.refresh_token)
                .as_deref()
                .map_err(Clone::clone),
            expected
        );
        let sent = String::from_utf8_lossy(&server.requests.lock().unwrap()[0].1).into_owned();
        assert!(sent.contains("grant_type=refresh_token") && sent.contains("client_id=oaiapp_1"));
    }
    let revocations = [
        (answer("200 OK", json!({})), Ok(())),
        (
            answer("503 Service Unavailable", json!({})),
            Err(Error::Temporary("503 Service Unavailable".into())),
        ),
    ];
    for (response, expected) in revocations {
        let server = Server::scripted(vec![response]).await;
        assert_eq!(
            revoke(&http(), &issuer(&server.url), "oaiapp_1", "r1").await,
            expected
        );
    }
}

#[tokio::test]
async fn discovery_keeps_every_endpoint_on_the_issuers_own_origin() {
    let config = |token: &str| json!({"issuer": "{base}", "authorization_endpoint": "{base}/authorize", "token_endpoint": token, "revocation_endpoint": "{base}/revoke", "jwks_uri": "{base}/jwks"});
    let server = Server::scripted(vec![answer("200 OK", config("{base}/token"))]).await;
    let found = Issuer::discover(&http(), &server.url).await.unwrap();
    assert_eq!(found.token_endpoint, format!("{}/token", server.url));
    for elsewhere in ["https://auth.evil.test/token", "http://unused.test/token"] {
        let server = Server::scripted(vec![answer("200 OK", config(elsewhere))]).await;
        assert!(
            Issuer::discover(&http(), &server.url).await.is_err(),
            "{elsewhere}"
        );
    }
}
