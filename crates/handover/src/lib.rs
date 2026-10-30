//! Hands a secret, such as a new bot's token, from a service to the one computer that asked
//! for it. The computer makes a key pair and sends only its public half; the service seals the
//! secret to that key; a proxy or log between them sees only sealed bytes.
//!
//! The service makes its own one-time key pair per secret and agrees a shared key with the
//! computer's (X25519). Both sides derive the cipher key from it with HKDF-SHA256 over both
//! public keys, and the secret travels under ChaCha20-Poly1305, so any change to it is refused.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::aead::{self, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519};
use ring::hkdf;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

/// Names this use of the keys, so a key agreed for something else cannot open a handover.
const PURPOSE: &[u8] = b"medha handover v1";

/// What travels from the service to the computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sealed {
    /// The service's one-time public key.
    pub key: String,
    pub nonce: String,
    pub sealed: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the handover could not be opened")
    }
}

impl std::error::Error for Refused {}

/// The computer's side: kept in memory only, used once.
pub struct Receiver {
    private: EphemeralPrivateKey,
    public: Vec<u8>,
}

impl Receiver {
    pub fn new() -> Result<Self, Refused> {
        let private =
            EphemeralPrivateKey::generate(&X25519, &SystemRandom::new()).map_err(|_| Refused)?;
        let public = private
            .compute_public_key()
            .map_err(|_| Refused)?
            .as_ref()
            .to_vec();
        Ok(Self { private, public })
    }

    /// The public half, to send with the request.
    pub fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.public)
    }

    /// Opens what the service sealed to this key; consumes the key, so it opens one handover.
    pub fn open(self, sealed: &Sealed) -> Result<String, Refused> {
        let theirs = decode(&sealed.key)?;
        let key = agreement::agree_ephemeral(
            self.private,
            &UnparsedPublicKey::new(&X25519, &theirs),
            |shared| cipher(shared, &theirs, &self.public),
        )
        .map_err(|_| Refused)??;
        let nonce =
            Nonce::try_assume_unique_for_key(&decode(&sealed.nonce)?).map_err(|_| Refused)?;
        let mut bytes = decode(&sealed.sealed)?;
        let plain = key
            .open_in_place(nonce, aead::Aad::from(PURPOSE), &mut bytes)
            .map_err(|_| Refused)?;
        String::from_utf8(plain.to_vec()).map_err(|_| Refused)
    }
}

/// The service's side: seals `secret` to the computer's public key.
pub fn seal(public_key: &str, secret: &str) -> Result<Sealed, Refused> {
    let theirs = decode(public_key)?;
    if theirs.len() != 32 {
        return Err(Refused);
    }
    let random = SystemRandom::new();
    let private = EphemeralPrivateKey::generate(&X25519, &random).map_err(|_| Refused)?;
    let ours = private
        .compute_public_key()
        .map_err(|_| Refused)?
        .as_ref()
        .to_vec();
    let key = agreement::agree_ephemeral(
        private,
        &UnparsedPublicKey::new(&X25519, &theirs),
        |shared| cipher(shared, &ours, &theirs),
    )
    .map_err(|_| Refused)??;
    let mut nonce = [0u8; aead::NONCE_LEN];
    random.fill(&mut nonce).map_err(|_| Refused)?;
    let mut bytes = secret.as_bytes().to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        aead::Aad::from(PURPOSE),
        &mut bytes,
    )
    .map_err(|_| Refused)?;
    Ok(Sealed {
        key: URL_SAFE_NO_PAD.encode(ours),
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        sealed: URL_SAFE_NO_PAD.encode(bytes),
    })
}

/// The cipher key both sides reach: HKDF over the shared secret, salted by the service's then
/// the computer's public key.
fn cipher(shared: &[u8], service: &[u8], computer: &[u8]) -> Result<LessSafeKey, Refused> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, &[service, computer].concat());
    let pseudo_random = salt.extract(shared);
    let okm = pseudo_random
        .expand(&[PURPOSE], &CHACHA20_POLY1305)
        .map_err(|_| Refused)?;
    let mut key = [0u8; 32];
    okm.fill(&mut key).map_err(|_| Refused)?;
    Ok(LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &key).map_err(|_| Refused)?,
    ))
}

fn decode(text: &str) -> Result<Vec<u8>, Refused> {
    URL_SAFE_NO_PAD.decode(text).map_err(|_| Refused)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "123456:AAExampleTokenForTheTest";

    #[test]
    fn only_the_computer_that_asked_can_open_the_secret() {
        let asker = Receiver::new().unwrap();
        let sealed = seal(&asker.public_key(), TOKEN).unwrap();
        assert!(
            !sealed.sealed.contains("Example"),
            "the secret travels in the clear"
        );
        let stranger = Receiver::new().unwrap();
        assert_eq!(
            stranger.open(&sealed),
            Err(Refused),
            "another key opened it"
        );
        assert_eq!(asker.open(&sealed).unwrap(), TOKEN);
    }

    #[test]
    fn a_changed_byte_in_any_part_is_refused() {
        for part in ["key", "nonce", "sealed"] {
            let asker = Receiver::new().unwrap();
            let mut sealed = seal(&asker.public_key(), TOKEN).unwrap();
            let text = match part {
                "key" => &mut sealed.key,
                "nonce" => &mut sealed.nonce,
                _ => &mut sealed.sealed,
            };
            let mut bytes = decode(text).unwrap();
            bytes[0] ^= 1;
            *text = URL_SAFE_NO_PAD.encode(bytes);
            assert_eq!(
                asker.open(&sealed),
                Err(Refused),
                "a changed {part} was accepted"
            );
        }
        assert!(seal("not-a-key", TOKEN).is_err());
        assert!(
            seal(&URL_SAFE_NO_PAD.encode([7u8; 16]), TOKEN).is_err(),
            "a short key was accepted"
        );
    }
}
