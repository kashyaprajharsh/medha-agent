//! Who may talk to the agent. A stranger gets a one-time code; only the owner, at this
//! machine, turns it into access with `medha pairing approve`.

use crate::store;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// No look-alike characters (0/O, 1/I), so a code read off a phone is typed right.
const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LENGTH: usize = 8;
const CODE_LIFETIME: u64 = 60 * 60;
const ONE_REQUEST_PER: u64 = 10 * 60;
const PENDING_PER_PLATFORM: usize = 3;
const FAILURES_BEFORE_LOCKOUT: u32 = 5;
const LOCKOUT: u64 = 60 * 60;

#[derive(Default, Serialize, Deserialize)]
struct Book {
    #[serde(default)]
    approved: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pending: Vec<Request>,
    /// Last code request per `platform:user`.
    #[serde(default)]
    asked: BTreeMap<String, u64>,
    #[serde(default)]
    failures: BTreeMap<String, u32>,
    #[serde(default)]
    locked_until: BTreeMap<String, u64>,
}

/// Only a salted hash of a code is kept, so reading the file reveals no code.
#[derive(Clone, Serialize, Deserialize)]
struct Request {
    platform: String,
    user: String,
    name: String,
    salt: String,
    hash: String,
    created: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Person {
    pub platform: String,
    pub user: String,
    pub name: String,
}

/// The person let in, or why nobody was.
pub type Approval = std::result::Result<Person, &'static str>;

#[derive(Debug, PartialEq, Eq)]
pub struct Waiting {
    pub platform: String,
    pub user: String,
    pub name: String,
    pub minutes: u64,
}

pub struct Pairing {
    path: PathBuf,
}

impl Pairing {
    pub fn open() -> Result<Self> {
        Ok(Self::at(store::home()?.join("pairing.json")))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn is_approved(&self, platform: &str, user: &str) -> Result<bool> {
        let book: Book = store::read(&self.path)?;
        Ok(book
            .approved
            .get(platform)
            .is_some_and(|users| users.contains_key(user)))
    }

    /// A new code for this person, or `None` when they asked too recently, too many are
    /// waiting on this platform, or the platform is locked after wrong codes.
    pub fn request(&self, platform: &str, user: &str, name: &str) -> Result<Option<String>> {
        self.request_at(platform, user, name, now())
    }

    fn request_at(
        &self,
        platform: &str,
        user: &str,
        name: &str,
        now: u64,
    ) -> Result<Option<String>> {
        let code = code()?;
        let salt = hex(&random::<16>()?);
        store::change(&self.path, |book: &mut Book| {
            book.expire(now);
            let asker = format!("{platform}:{user}");
            // Asking again replaces the person's own code, so it never counts against them.
            let waiting = book
                .pending
                .iter()
                .filter(|request| request.platform == platform && request.user != user);
            if book.locked(platform, now)
                || book
                    .asked
                    .get(&asker)
                    .is_some_and(|at| now < at + ONE_REQUEST_PER)
                || waiting.count() >= PENDING_PER_PLATFORM
            {
                return None;
            }
            book.pending
                .retain(|request| !(request.platform == platform && request.user == user));
            book.pending.push(Request {
                platform: platform.into(),
                user: user.into(),
                name: name.into(),
                hash: digest(&salt, &code),
                salt,
                created: now,
            });
            book.asked.insert(asker, now);
            Some(code)
        })
    }

    /// Lets in the person who was sent `code`. Five wrong codes in a row lock the platform
    /// for an hour, so a code cannot be guessed.
    pub fn approve(&self, platform: &str, code: &str) -> Result<Approval> {
        self.approve_at(platform, code, now())
    }

    fn approve_at(&self, platform: &str, code: &str, now: u64) -> Result<Approval> {
        let code: String = code.split_whitespace().collect::<String>().to_uppercase();
        store::change(&self.path, |book: &mut Book| {
            book.expire(now);
            if book.locked(platform, now) {
                return Err(
                    "Too many wrong codes. Pairing on this platform is locked for an hour.",
                );
            }
            let found = book.pending.iter().position(|request| {
                request.platform == platform && same(&digest(&request.salt, &code), &request.hash)
            });
            let Some(found) = found else {
                let failures = book.failures.entry(platform.into()).or_default();
                *failures += 1;
                if *failures >= FAILURES_BEFORE_LOCKOUT {
                    book.failures.remove(platform);
                    book.locked_until.insert(platform.into(), now + LOCKOUT);
                }
                return Err("No such code. Codes last one hour.");
            };
            let request = book.pending.remove(found);
            book.failures.remove(platform);
            book.approved
                .entry(request.platform.clone())
                .or_default()
                .insert(request.user.clone(), request.name.clone());
            Ok(Person {
                platform: request.platform,
                user: request.user,
                name: request.name,
            })
        })
    }

    /// Lets in a person the owner confirmed at this computer, with no code.
    pub fn admit(&self, platform: &str, user: &str, name: &str) -> Result<()> {
        store::change(&self.path, |book: &mut Book| {
            book.approved
                .entry(platform.to_owned())
                .or_default()
                .insert(user.to_owned(), name.to_owned());
        })
    }

    pub fn revoke(&self, platform: &str, user: &str) -> Result<bool> {
        store::change(&self.path, |book: &mut Book| {
            book.approved
                .get_mut(platform)
                .is_some_and(|users| users.remove(user).is_some())
        })
    }

    pub fn approved(&self) -> Result<Vec<Person>> {
        let book: Book = store::read(&self.path)?;
        Ok(book
            .approved
            .into_iter()
            .flat_map(|(platform, users)| {
                users.into_iter().map(move |(user, name)| Person {
                    platform: platform.clone(),
                    user,
                    name,
                })
            })
            .collect())
    }

    pub fn waiting(&self) -> Result<Vec<Waiting>> {
        self.waiting_at(now())
    }

    fn waiting_at(&self, now: u64) -> Result<Vec<Waiting>> {
        store::change(&self.path, |book: &mut Book| {
            book.expire(now);
            book.pending
                .iter()
                .map(|request| Waiting {
                    platform: request.platform.clone(),
                    user: request.user.clone(),
                    name: request.name.clone(),
                    minutes: now.saturating_sub(request.created) / 60,
                })
                .collect()
        })
    }

    pub fn clear_waiting(&self) -> Result<usize> {
        store::change(&self.path, |book: &mut Book| {
            std::mem::take(&mut book.pending).len()
        })
    }
}

impl Book {
    fn expire(&mut self, now: u64) {
        self.pending
            .retain(|request| now < request.created + CODE_LIFETIME);
        self.asked.retain(|_, at| now < *at + ONE_REQUEST_PER);
        self.locked_until.retain(|_, until| now < *until);
    }

    fn locked(&self, platform: &str, now: u64) -> bool {
        self.locked_until
            .get(platform)
            .is_some_and(|until| now < *until)
    }
}

fn code() -> Result<String> {
    // 256 is a multiple of 32, so taking the remainder keeps every character equally likely.
    Ok(random::<CODE_LENGTH>()?
        .iter()
        .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char)
        .collect())
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!("no randomness: {error}"))?;
    Ok(bytes)
}

fn digest(salt: &str, code: &str) -> String {
    hex(&Sha256::digest(format!("{salt}{code}")))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Compares in time that does not depend on where the first difference is.
fn same(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
#[path = "pairing_tests.rs"]
mod tests;
