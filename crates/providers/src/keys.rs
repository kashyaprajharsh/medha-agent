//! Which provider a pasted key belongs to, read from its opening letters; nothing is sent or kept.

use crate::catalogue::ProviderRecord;

/// Openings several sources agree on (October 2026); a shared opening lists every owner.
const PREFIXES: &[(&str, &[&str])] = &[
    ("sk-or-", &["openrouter"]),
    ("sk-ant-", &["anthropic"]),
    ("sk-proj-", &["openai"]),
    ("sk-svcacct-", &["openai"]),
    ("sk-None-", &["openai"]),
    ("sk-admin-", &["openai"]),
    ("AIza", &["google"]),
    ("AQ.", &["google"]),
    ("gsk_", &["groq"]),
    ("xai-", &["xai"]),
    ("nvapi-", &["nvidia"]),
    ("hf_", &["huggingface"]),
    ("csk-", &["cerebras"]),
    ("fw_", &["fireworks-ai"]),
    ("sk-", &["openai", "deepseek", "moonshotai"]),
];

const ADMIN: &str = "sk-admin-";
pub const ADMIN_WARNING: &str = "This is an OpenAI admin key, which can manage your whole organisation. A project key is safer for Medha.";

#[derive(Debug)]
pub enum Recognised<'a> {
    One(&'a ProviderRecord),
    /// The opening is shared, so the user picks; the order is the table's.
    Several(Vec<&'a ProviderRecord>),
    /// An organisation admin key: it works, but the user is warned before using it.
    Admin(&'a ProviderRecord),
    Unknown,
}

/// The longest matching opening wins; providers missing from `providers` are never suggested.
pub fn recognise<'a>(key: &str, providers: &'a [ProviderRecord]) -> Recognised<'a> {
    let key = key.trim();
    let Some((prefix, owners)) = PREFIXES
        .iter()
        .filter(|(prefix, _)| key.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
    else {
        return Recognised::Unknown;
    };
    let found: Vec<_> = owners
        .iter()
        .filter_map(|id| providers.iter().find(|provider| provider.id == *id))
        .collect();
    if owners.len() > 1 && !found.is_empty() {
        return Recognised::Several(found);
    }
    match found.first() {
        None => Recognised::Unknown,
        Some(provider) if *prefix == ADMIN => Recognised::Admin(provider),
        Some(provider) => Recognised::One(provider),
    }
}

/// When `address` is a known provider's own and the key surely belongs to another: (here, owner).
pub fn owner_elsewhere<'a>(
    key: &str,
    address: &str,
    providers: &'a [ProviderRecord],
) -> Option<(&'a ProviderRecord, &'a ProviderRecord)> {
    let (Recognised::One(owner) | Recognised::Admin(owner)) = recognise(key, providers) else {
        return None;
    };
    let address = comparable(address);
    let here: Vec<_> = providers
        .iter()
        .filter(|provider| {
            provider
                .url
                .as_deref()
                .is_some_and(|url| comparable(url) == address)
        })
        .collect();
    if here.iter().any(|provider| provider.id == owner.id) {
        return None;
    }
    here.first().map(|first| (*first, owner))
}

fn comparable(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

#[cfg(test)]
#[path = "keys_tests.rs"]
mod tests;
