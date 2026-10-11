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
            .filter(|(name, value)| secret_name(name) && looks_like_credential(value))
            .map(|(_, value)| value)
            .collect()
    }

    /// `text` with every secret found in it replaced by a marker.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
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

/// A value that could be a credential: long, one word, and not a number, a
/// path, a variable reference, a switch or a placeholder.
fn looks_like_credential(value: &str) -> bool {
    let value = value.trim_matches(['"', '\'']);
    let kinds = [
        value.bytes().any(|b| b.is_ascii_lowercase()),
        value.bytes().any(|b| b.is_ascii_uppercase()),
        value.bytes().any(|b| b.is_ascii_digit()),
        value.bytes().any(|b| !b.is_ascii_alphanumeric()),
    ];
    value.len() >= 12
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
