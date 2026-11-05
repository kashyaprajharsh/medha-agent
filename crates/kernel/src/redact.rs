//! Secrets kept out of what the model reads. Text from tools goes to a model
//! provider, so a credential that turns up in a command's output or a file is
//! removed first. Three kinds are recognised, each without guessing at prose:
//! values Medha itself holds, keys in a well-known issued format, and in
//! `KEY=value` lines of env and config files, values that look like
//! credentials under a name that says they are one.

use regex::Regex;
use std::{borrow::Cow, sync::LazyLock};

const MARK: &str = "[secret removed]";
/// Shorter values are too likely to occur by chance to remove everywhere.
const MIN_HELD: usize = 8;

/// Secret values held for removal, bounded: past 4096 values or 1 MiB the least recently seen is forgotten,
/// so a long-running process never stops all outbound work. Seeing a value again makes it recent.
#[derive(Default)]
pub(crate) struct HeldValues {
    /// Longest first, so a value containing another is removed whole.
    values: Vec<String>,
    /// Least recently seen first.
    order: std::collections::VecDeque<String>,
    bytes: usize,
}

impl HeldValues {
    pub(crate) fn hold(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        if let Some(at) = self.order.iter().position(|held| held == value) {
            let seen = self.order.remove(at).unwrap_or_default();
            self.order.push_back(seen);
            return;
        }
        self.bytes += value.len();
        self.order.push_back(value.to_owned());
        self.values.push(value.to_owned());
        while self.order.len() > 4096 || self.bytes > 1024 * 1024 {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.bytes -= oldest.len();
            self.values.retain(|held| *held != oldest);
        }
        self.values
            .sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    }

    pub(crate) fn values(&self) -> &[String] {
        &self.values
    }
}

static KNOWN: std::sync::LazyLock<std::sync::RwLock<HeldValues>> =
    std::sync::LazyLock::new(Default::default);

pub fn remember(value: &str) {
    KNOWN
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .hold(value);
}

pub(crate) fn contains_known(text: &str) -> bool {
    let known = KNOWN
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A placeholder key such as "ollama" or "EMPTY" is an ordinary word, not a secret.
    known
        .values
        .iter()
        .filter(|value| looks_like_credential(value))
        .any(|value| text.contains(value.as_str()))
}

pub(crate) fn outbound_preview(text: &str, file_values: &[String]) -> String {
    let known = KNOWN
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut output = String::new();
    let mut offset = 0;
    while offset < text.len() && output.len() < 2048 {
        if let Some(value) = known
            .values
            .iter()
            .chain(file_values)
            .filter(|value| text[offset..].starts_with(value.as_str()))
            .max_by_key(|value| value.len())
        {
            output.push_str(MARK);
            offset += value.len();
        } else {
            let ch = text[offset..].chars().next().unwrap();
            output.push(ch);
            offset += ch.len_utf8();
        }
    }
    if offset < text.len() {
        output.push_str(" [preview truncated]");
    }
    output
}

pub(crate) fn file_values(text: &str) -> Vec<String> {
    let mut values: Vec<_> = ISSUED
        .find_iter(text)
        .map(|found| found.as_str().to_owned())
        .collect();
    values.extend(
        ASSIGNED
            .captures_iter(text)
            .filter(|found| secret_name(&found[2]) && looks_like_credential(&found[5]))
            .map(|found| found[5].to_owned()),
    );
    values
}

pub fn remember_named(name: &str, value: &str) {
    let authorization = name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("proxy-authorization");
    if secret_name(name) || authorization {
        remember(value);
        if authorization && let Some((_, credential)) = value.split_once(' ') {
            remember(credential.trim());
        }
    }
}

pub fn remember_stored(value: &str) {
    remember(value);
    if let Ok(stored) = serde_json::from_str::<serde_json::Value>(value) {
        for key in ["access_token", "refresh_token", "client_secret"] {
            for source in [&stored, &stored["token"]] {
                if let Some(value) = source[key].as_str() {
                    remember(value);
                }
            }
        }
    }
}

static ISSUED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\bsk-(?:proj-|ant-[a-z0-9]+-|or-v1-)?[A-Za-z0-9_-]{20,}",
        r"|\bgh[pousr]_[A-Za-z0-9]{30,}",
        r"|\bgithub_pat_[A-Za-z0-9_]{30,}",
        r"|\bAKIA[0-9A-Z]{16}\b",
        r"|\bxox[baprs]-[A-Za-z0-9-]{10,}",
        r"|\bAIza[0-9A-Za-z_-]{35}",
        r"|-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
    ))
    .expect("issued key formats")
});

/// A `NAME=value` or `name: value` line, as env and config files write them.
static ASSIGNED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)^(\s*(?:export\s+)?)([A-Za-z_][A-Za-z0-9_.-]*)(\s*[=:]\s*)(["']?)([^\s"'#]+)"#,
    )
    .expect("assignment lines")
});

/// The secrets this session holds, and the rules for ones it does not.
#[derive(Default, Clone)]
pub struct Secrets {
    held: Vec<String>,
}

impl Secrets {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut held: Vec<String> = values
            .into_iter()
            .inspect(|value| remember(value))
            .map(|value| value.trim().to_owned())
            .filter(|value| value.len() >= MIN_HELD)
            .collect();
        // Longest first, so a value containing another is removed whole.
        held.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        held.dedup();
        Self { held }
    }

    /// Values of this process's environment variables whose names say they
    /// hold a secret: what a command could read of Medha's own environment.
    pub fn in_environment() -> Vec<String> {
        std::env::vars()
            .filter(|(name, _)| secret_name(name))
            .map(|(_, value)| value)
            .collect()
    }

    /// `text` with every secret found in it replaced by a marker.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        let known = KNOWN
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for value in known.values.iter().filter(|value| value.len() >= MIN_HELD) {
            if out.contains(value) {
                out = Cow::Owned(out.replace(value, MARK));
            }
        }
        for value in &self.held {
            if out.contains(value.as_str()) {
                out = Cow::Owned(out.replace(value.as_str(), MARK));
            }
        }
        if let Cow::Owned(issued) = ISSUED.replace_all(&out, MARK) {
            out = Cow::Owned(issued);
        }
        let assigned = ASSIGNED.replace_all(&out, |found: &regex::Captures| {
            let (name, value) = (&found[2], &found[5]);
            if secret_name(name) && looks_like_credential(value) {
                format!("{}{}{}{}{MARK}", &found[1], name, &found[3], &found[4])
            } else {
                found[0].to_owned()
            }
        });
        match assigned {
            Cow::Owned(changed) if changed != *out => Cow::Owned(changed),
            _ => out,
        }
    }
}

/// A name whose words say it holds a secret: `API_KEY`, `db.password`,
/// `GITHUB_TOKEN`. Words are matched whole, so `MAX_TOKENS` and `KEYBOARD`
/// are not, and `PWD` (the working directory) is not a password.
fn secret_name(name: &str) -> bool {
    name.split(['_', '.', '-'])
        .map(str::to_ascii_uppercase)
        .any(|word| {
            matches!(
                word.as_str(),
                "KEY"
                    | "APIKEY"
                    | "TOKEN"
                    | "SECRET"
                    | "PASSWORD"
                    | "PASSWD"
                    | "PASS"
                    | "CREDENTIAL"
                    | "CREDENTIALS"
            )
        })
}

/// Code on the right of `key = …`: a name called or indexed, or a dotted lowercase name.
static CODE_EXPRESSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[A-Za-z_][A-Za-z0-9_.]*(?:\(.*\)|\[.*\])|[a-z_][a-z0-9_]*(?:\.[a-z_][a-z0-9_]*)+)$",
    )
    .expect("code expression")
});

/// A value that could be a credential: long, one word, and not a number, a
/// path, a variable reference, a switch, a placeholder or code.
fn looks_like_credential(value: &str) -> bool {
    let value = value.trim_matches(['"', '\'']);
    let kinds = [
        value.bytes().any(|b| b.is_ascii_lowercase()),
        value.bytes().any(|b| b.is_ascii_uppercase()),
        value.bytes().any(|b| b.is_ascii_digit()),
        value.bytes().any(|b| !b.is_ascii_alphanumeric()),
    ];
    value.len() >= 12
        && !CODE_EXPRESSION.is_match(value)
        && !value.contains(char::is_whitespace)
        && !value.starts_with(['/', '~', '$', '.', '<', '{'])
        && !value.contains("://")
        && !value.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "changeme" | "placeholder"
        )
        && kinds.iter().filter(|kind| **kind).count() >= 2
}

#[cfg(test)]
#[path = "redact_tests.rs"]
mod tests;
