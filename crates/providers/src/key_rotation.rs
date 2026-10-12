//! Several keys for one server: which are resting, and which is tried next.
//! A key that failed rests for every chat in this process, not only its own.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// A refused key may be a passing fault at the server, so it is tried again soon.
const REFUSED: Duration = Duration::from_secs(5 * 60);
/// A rate limit, an empty account or anything else keeps a key out for an hour.
const LIMITED: Duration = Duration::from_secs(60 * 60);

/// When each resting key may be used again, by fingerprint: no key is held here.
static RESTING: LazyLock<Mutex<HashMap<[u8; 32], Instant>>> = LazyLock::new(Mutex::default);

fn fingerprint(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

/// How long a key rests after its server answered `status`: what the server
/// asked for when it said, else by what went wrong.
pub(crate) fn rest(status: u16, asked: Option<Duration>) -> Duration {
    asked.unwrap_or(match status {
        401 | 403 => REFUSED,
        _ => LIMITED,
    })
}

pub(crate) fn rest_key(key: &str, wait: Duration) {
    let until = Instant::now() + wait;
    RESTING.lock().unwrap().insert(fingerprint(key), until);
}

pub(crate) fn resting(key: &str) -> bool {
    let resting = RESTING.lock().unwrap();
    resting
        .get(&fingerprint(key))
        .is_some_and(|until| Instant::now() < *until)
}

/// The first of `keys` that is not resting, and its place counted from 1.
pub(crate) fn first_ready(keys: &[String]) -> Option<(usize, &String)> {
    let ready = keys.iter().enumerate().find(|(_, key)| !resting(key));
    ready.map(|(at, key)| (at + 1, key))
}
