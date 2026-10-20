//! An ID token is trusted only with an RS256 signature by one of OpenAI's published keys and
//! claims that match this attempt.

use super::{Error, read_json};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use serde_json::Value;

/// Clock difference allowed between this machine and OpenAI.
const SKEW: u64 = 60;

pub(super) struct Identity {
    pub subject: String,
    pub email: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Keys {
    keys: Vec<Key>,
}

#[derive(Deserialize)]
struct Key {
    kid: Option<String>,
    kty: String,
    n: Option<String>,
    e: Option<String>,
}

pub(super) async fn verify(
    http: &reqwest::Client,
    jwks_uri: &str,
    token: &str,
    issuer: &str,
    audience: &str,
    nonce: &str,
    now: u64,
) -> Result<Identity, Error> {
    let keys: Keys = read_json(http.get(jwks_uri).send().await).await?;
    check(token, &keys, issuer, audience, nonce, now)
}

pub(super) fn check(
    token: &str,
    keys: &Keys,
    issuer: &str,
    audience: &str,
    nonce: &str,
    now: u64,
) -> Result<Identity, Error> {
    let bad = |why: &str| Error::Invalid(format!("the ID token {why}"));
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("is not a JWT"));
    };
    let header = decode(head).ok_or_else(|| bad("has an unreadable header"))?;
    if header["alg"] != "RS256" {
        return Err(bad("is not signed with RS256"));
    }
    let key = keys
        .keys
        .iter()
        .find(|key| key.kty == "RSA" && key.kid.as_deref() == header["kid"].as_str())
        .ok_or_else(|| bad("names a key OpenAI does not publish"))?;
    let part = |value: &Option<String>| {
        value
            .as_deref()
            .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
    };
    let (Some(n), Some(e), Some(signature)) = (
        part(&key.n),
        part(&key.e),
        URL_SAFE_NO_PAD.decode(signature).ok(),
    ) else {
        return Err(bad("or its key cannot be read"));
    };
    let signed = &token[..head.len() + 1 + body.len()];
    RsaPublicKeyComponents { n, e }
        .verify(&RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature)
        .map_err(|_| bad("signature does not match"))?;

    let claims = decode(body).ok_or_else(|| bad("has unreadable claims"))?;
    if claims["iss"] != issuer {
        return Err(bad("comes from another issuer"));
    }
    let for_us = match &claims["aud"] {
        Value::String(single) => single == audience,
        Value::Array(many) => many.iter().any(|one| one == audience),
        _ => false,
    };
    if !for_us {
        return Err(bad("was issued to another client"));
    }
    if claims["exp"].as_u64().is_none_or(|exp| exp + SKEW < now) {
        return Err(bad("has expired"));
    }
    if claims["nonce"] != nonce {
        return Err(bad("belongs to another sign-in"));
    }
    let subject = claims["sub"]
        .as_str()
        .filter(|sub| !sub.is_empty())
        .ok_or_else(|| bad("names no account"))?;
    Ok(Identity {
        subject: subject.to_owned(),
        email: claims["email"].as_str().map(str::to_owned),
    })
}

fn decode(part: &str) -> Option<Value> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).ok()?).ok()
}
