//! Whether a server must be sent a model's earlier thinking back.
//!
//! Some models refuse a request whose earlier assistant turns lack their
//! thinking; most servers refuse one that carries it. So it is sent only when
//! the model catalogue says this model wants it under `reasoning_content`, or
//! the person said so for a server the catalogue does not know.

use crate::ProviderProfile;
use crate::catalogue::ProviderRecord;
use crate::models_dev::ModelMeta;
use std::collections::BTreeMap;

/// The one field the chat protocol can carry thinking back in.
const FIELD: &str = "reasoning_content";

/// Whether earlier assistant turns go to this server with their thinking.
pub(crate) fn expected(profile: &ProviderProfile) -> bool {
    let listed = || {
        let data = crate::catalogue::snapshot();
        listed_in(profile, &data.providers, &data.entries)
    };
    decided(profile.reasoning_echo, listed)
}

/// The person's own setting wins; without one, what the catalogue records.
fn decided(set: Option<bool>, listed: impl FnOnce() -> Option<String>) -> bool {
    set.unwrap_or_else(|| listed().as_deref() == Some(FIELD))
}

/// The field the catalogue says this model, on this server, wants its thinking in.
fn listed_in(
    profile: &ProviderProfile,
    providers: &[ProviderRecord],
    entries: &BTreeMap<String, ModelMeta>,
) -> Option<String> {
    // The provider the person picked, else every one this address belongs to.
    let own: Vec<&ProviderRecord> = match &profile.provider_id {
        Some(id) => {
            let picked = providers.iter();
            picked.filter(|p| p.id.eq_ignore_ascii_case(id)).collect()
        }
        None => at_address(profile, providers),
    };
    // A server the catalogue does not know: the model's name is all there is.
    if own.is_empty() {
        return crate::models_dev::thinking_field_in(profile, entries);
    }
    let model = profile.model.strip_prefix("models/");
    let model = model.unwrap_or(&profile.model);
    let lists = |provider: &&&ProviderRecord| {
        let listed = provider.models.iter();
        listed.into_iter().any(|m| m.id.eq_ignore_ascii_case(model))
    };
    match own.iter().find(lists) {
        Some(provider) => meta(provider, model, entries)?.thinking_field.clone(),
        // Not listed yet: as this server's other thinking models, where they agree.
        None => own.iter().find_map(|provider| agreed(provider, entries)),
    }
}

/// What the catalogue holds for one of a provider's own models.
fn meta<'a>(
    provider: &ProviderRecord,
    model: &str,
    entries: &'a BTreeMap<String, ModelMeta>,
) -> Option<&'a ModelMeta> {
    entries.get(&format!("{}/{model}", provider.id).to_lowercase())
}

/// The catalogue's providers whose server is the one this model is reached at.
fn at_address<'a>(
    profile: &ProviderProfile,
    providers: &'a [ProviderRecord],
) -> Vec<&'a ProviderRecord> {
    let host = |url: &str| {
        let url = reqwest::Url::parse(url).ok()?;
        url.host_str().map(str::to_ascii_lowercase)
    };
    let Some(own) = host(&profile.base_url) else {
        return Vec::new();
    };
    let here =
        |provider: &&ProviderRecord| provider.url.as_deref().and_then(host).as_ref() == Some(&own);
    providers.iter().filter(here).collect()
}

/// The one field every thinking model of `provider` wants, when they all want it.
fn agreed(provider: &ProviderRecord, entries: &BTreeMap<String, ModelMeta>) -> Option<String> {
    let thinks = |meta: &&ModelMeta| {
        let supported = crate::CapabilityState::Supported;
        let reasons = meta.capabilities.as_ref();
        meta.thinking_field.is_some() || reasons.is_some_and(|c| c.reasoning_state() == supported)
    };
    let own = provider.models.iter();
    let thinking = own
        .filter_map(|m| meta(provider, &m.id, entries))
        .filter(thinks);
    let fields: Vec<_> = thinking
        .map(|meta| meta.thinking_field.as_deref())
        .collect();
    // One model is not a pattern: two or more must say the same.
    let same = fields.len() > 1 && fields.iter().all(|field| *field == fields[0]);
    fields
        .first()
        .copied()
        .flatten()
        .filter(|_| same)
        .map(str::to_owned)
}

#[cfg(test)]
#[path = "reasoning_echo_tests.rs"]
mod tests;
