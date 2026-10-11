//! Cached models.dev metadata. Missing values remain unknown; prices are
//! indicative for self-hosted routes.

use kernel::{Protocol, ReasoningEffort};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Whether a model capability is known from an authoritative source.
///
/// Capability discovery must keep "not listed" separate from an explicit
/// negative. A custom endpoint may support an input even when the catalog has
/// no entry for it, so callers must not turn an unknown value into `false`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

impl CapabilityState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

/// The modality names published by models.dev. Unknown names are retained so
/// the cache can round-trip newer catalog values without a binary update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModalitySet {
    // Missing direction metadata is unknown; an explicitly empty set is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<BTreeSet<String>>,
}

impl ModalitySet {
    fn has(values: &BTreeSet<String>, modality: &str) -> bool {
        values
            .iter()
            .any(|value| value.eq_ignore_ascii_case(modality))
    }

    pub fn input_state(&self, modality: &str) -> CapabilityState {
        Self::state(self.input.as_ref(), modality)
    }

    pub fn output_state(&self, modality: &str) -> CapabilityState {
        Self::state(self.output.as_ref(), modality)
    }

    fn state(values: Option<&BTreeSet<String>>, modality: &str) -> CapabilityState {
        match values {
            None => CapabilityState::Unknown,
            Some(values) if Self::has(values, modality) => CapabilityState::Supported,
            Some(_) => CapabilityState::Unsupported,
        }
    }
}

/// Model-level capability metadata. Every field is optional because a catalog
/// entry may describe only pricing or context limits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    #[serde(default)]
    pub modalities: Option<ModalitySet>,
    #[serde(default)]
    pub attachment: Option<bool>,
    /// Model-specific evidence for where images are accepted. Missing remains
    /// unknown even if another role accepts images.
    #[serde(default)]
    pub user_images: Option<bool>,
    #[serde(default)]
    pub tool_result_images: Option<bool>,
    #[serde(default)]
    pub image_mime_types: Option<BTreeSet<String>>,
    #[serde(default)]
    pub max_image_bytes: Option<u64>,
    #[serde(default)]
    pub max_image_width: Option<u32>,
    #[serde(default)]
    pub max_image_height: Option<u32>,
    #[serde(default, rename = "tool_call")]
    pub tool_calls: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CapabilitySource {
    ProfileOverride,
    ModelsDev,
    #[default]
    Unknown,
}

impl CapabilitySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProfileOverride => "profile override",
            Self::ModelsDev => "models.dev",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityResolution {
    pub capabilities: Option<ModelCapabilities>,
    pub source: CapabilitySource,
}

impl ModelCapabilities {
    pub fn user_image_state(&self) -> CapabilityState {
        self.user_images.map_or_else(
            || self.input_state("image"),
            |known| {
                if known {
                    CapabilityState::Supported
                } else {
                    CapabilityState::Unsupported
                }
            },
        )
    }

    pub fn tool_result_image_state(&self) -> CapabilityState {
        self.tool_result_images
            .map_or(CapabilityState::Unknown, |known| {
                if known {
                    CapabilityState::Supported
                } else {
                    CapabilityState::Unsupported
                }
            })
    }

    pub fn input_state(&self, modality: &str) -> CapabilityState {
        self.modalities
            .as_ref()
            .map_or(CapabilityState::Unknown, |set| set.input_state(modality))
    }

    pub fn output_state(&self, modality: &str) -> CapabilityState {
        self.modalities
            .as_ref()
            .map_or(CapabilityState::Unknown, |set| set.output_state(modality))
    }

    pub fn attachment_state(&self) -> CapabilityState {
        self.attachment.map_or(CapabilityState::Unknown, |value| {
            if value {
                CapabilityState::Supported
            } else {
                CapabilityState::Unsupported
            }
        })
    }

    pub fn tool_call_state(&self) -> CapabilityState {
        self.tool_calls.map_or(CapabilityState::Unknown, |value| {
            if value {
                CapabilityState::Supported
            } else {
                CapabilityState::Unsupported
            }
        })
    }

    pub fn reasoning_state(&self) -> CapabilityState {
        self.reasoning.map_or(CapabilityState::Unknown, |value| {
            if value {
                CapabilityState::Supported
            } else {
                CapabilityState::Unsupported
            }
        })
    }

    fn has_known_value(&self) -> bool {
        self.modalities.is_some()
            || self.attachment.is_some()
            || self.user_images.is_some()
            || self.tool_result_images.is_some()
            || self.image_mime_types.is_some()
            || self.max_image_bytes.is_some()
            || self.max_image_width.is_some()
            || self.max_image_height.is_some()
            || self.tool_calls.is_some()
            || self.reasoning.is_some()
    }
}

/// Everything we retain per model. New fields use defaults so existing cache
/// files remain readable and gain capabilities on the next refresh.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelMeta {
    pub context: Option<u32>,
    /// USD per million input tokens (models.dev list price).
    pub input_per_mtok: Option<f64>,
    /// USD per million output tokens (models.dev list price).
    pub output_per_mtok: Option<f64>,
    /// USD per million prompt tokens served from the provider's cache.
    #[serde(default)]
    pub cached_input_per_mtok: Option<f64>,
    /// USD per million prompt tokens the provider stores in its cache.
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
    #[serde(default)]
    pub capabilities: Option<ModelCapabilities>,
    /// Exact effort values, separate from toggle/budget reasoning controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_efforts: Option<Vec<ReasoningEffort>>,
}

#[derive(Deserialize)]
struct Provider {
    #[serde(default)]
    name: String,
    api: Option<String>,
    npm: Option<String>,
    doc: Option<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    models: BTreeMap<String, ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    limit: Option<Limit>,
    #[serde(default)]
    cost: Option<Cost>,
    #[serde(default)]
    modalities: Option<ModalitySet>,
    #[serde(default)]
    attachment: Option<bool>,
    #[serde(default, rename = "tool_call")]
    tool_calls: Option<bool>,
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    reasoning_options: Vec<ReasoningOption>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ReasoningOption {
    Effort {
        // Public entries can contain null as a default marker. Preserve known
        // strings and ignore other representations without losing the table.
        values: Vec<serde_json::Value>,
    },
    // Toggles, budgets and future controls are not effort strings.
    #[serde(other)]
    Other,
}

impl ModelEntry {
    fn reasoning_efforts(&self) -> Option<Vec<ReasoningEffort>> {
        self.reasoning_options.iter().find_map(|option| {
            let ReasoningOption::Effort { values } = option else {
                return None;
            };
            let mut efforts = Vec::new();
            for value in values {
                let Some(value) = value.as_str() else {
                    continue;
                };
                if let Some(effort) = ReasoningEffort::ALL
                    .iter()
                    .find(|effort| effort.as_str() == value)
                    && !efforts.contains(effort)
                {
                    efforts.push(*effort);
                }
            }
            // A newly introduced effort we cannot encode is unknown, not an
            // explicit statement that this model has no effort control.
            (values.is_empty() || !efforts.is_empty()).then_some(efforts)
        })
    }
}

#[derive(Deserialize)]
struct Limit {
    #[serde(default)]
    context: Option<u32>,
}

#[derive(Deserialize)]
struct Cost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    /// Rate for prompt tokens the provider serves from its own cache. Published
    /// for roughly two thirds of the catalogue; absent means the route does not
    /// price cache reads separately, not that they are free.
    #[serde(default)]
    cache_read: Option<f64>,
    /// Rate for prompt tokens the provider stores in its cache for later.
    #[serde(default)]
    cache_write: Option<f64>,
}

pub(crate) fn parse_catalogue(bytes: &[u8]) -> Result<crate::catalogue::Snapshot, String> {
    use crate::catalogue::{CatalogueModel, ProviderRecord, Snapshot};
    let providers: BTreeMap<String, Provider> = serde_json::from_slice(bytes)
        .map_err(|_| "The model catalogue has an invalid representation.".to_owned())?;
    let model_count = providers.values().map(|p| p.models.len()).sum::<usize>();
    if providers.is_empty() || providers.len() > 512 || model_count == 0 || model_count > 30_000 {
        return Err("The model catalogue has too many providers/models or is empty.".into());
    }
    let mut records = crate::catalogue::local_providers();
    for (id, provider) in &providers {
        if !crate::catalogue::valid_id(id) {
            return Err("Invalid catalogue provider id.".into());
        }
        if records.iter().any(|record| &record.id == id) {
            return Err("Catalogue provider identity conflicts with a local provider.".into());
        }
        let mut record = ProviderRecord::from_metadata(
            id,
            &provider.name,
            provider.api.as_deref(),
            provider.npm.as_deref(),
        );
        record.doc = provider
            .doc
            .clone()
            .filter(|doc| crate::catalogue::valid_url(doc).is_ok());
        record.env = provider
            .env
            .iter()
            .filter(|name| {
                name.len() <= 128
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            })
            .cloned()
            .collect();
        for (id, model) in &provider.models {
            if id.len() > 512 || id.is_empty() || id.chars().any(char::is_control) {
                return Err("Invalid catalogue model id.".into());
            }
            // Explicitly non-text output models cannot run an agent chat.
            if model
                .modalities
                .as_ref()
                .and_then(|m| m.output.as_ref())
                .is_some_and(|m| !m.iter().any(|v| v == "text"))
            {
                continue;
            }
            record.models.push(CatalogueModel {
                id: id.clone(),
                name: model
                    .name
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(256)
                    .collect(),
                context_length: model
                    .limit
                    .as_ref()
                    .and_then(|l| l.context)
                    .filter(|v| *v > 0),
            });
        }
        records.push(record);
    }
    // Local servers stay first; hosted providers have a stable alphabetical order.
    records[4..].sort_by_key(|p| (p.name.to_lowercase(), p.id.clone()));
    let entries = flatten(providers);
    Ok(Snapshot {
        revision: crate::catalogue::revision(&records, &entries),
        fetched_at_unix: 0,
        providers: records,
        entries,
    })
}

fn flatten(providers: BTreeMap<String, Provider>) -> BTreeMap<String, ModelMeta> {
    let mut flat = BTreeMap::new();
    let mut unqualified: BTreeMap<String, Vec<ModelMeta>> = BTreeMap::new();
    for (provider_id, provider) in providers {
        for (id, model) in provider.models {
            let reasoning_efforts = model.reasoning_efforts();
            let capability_values = ModelCapabilities {
                modalities: model.modalities,
                attachment: model.attachment,
                tool_calls: model.tool_calls,
                reasoning: model.reasoning,
                ..ModelCapabilities::default()
            };
            let meta = ModelMeta {
                context: model.limit.and_then(|l| l.context),
                input_per_mtok: model.cost.as_ref().and_then(|c| c.input),
                output_per_mtok: model.cost.as_ref().and_then(|c| c.output),
                cached_input_per_mtok: model.cost.as_ref().and_then(|c| c.cache_read),
                cache_write_per_mtok: model.cost.as_ref().and_then(|c| c.cache_write),
                capabilities: capability_values
                    .has_known_value()
                    .then_some(capability_values),
                reasoning_efforts,
            };
            if meta.context.is_some()
                || meta.input_per_mtok.is_some()
                || meta.capabilities.is_some()
                || meta.reasoning_efforts.is_some()
            {
                let normalized_id = id.to_lowercase();
                flat.insert(
                    format!("{}/{}", provider_id.to_lowercase(), normalized_id),
                    meta.clone(),
                );
                unqualified.entry(normalized_id).or_default().push(meta);
            }
        }
    }
    // An unqualified id is safe only when models.dev contains one such model.
    // Ambiguous ids require the caller to pass provider/model, preventing a
    // capability from one provider being applied to another deployment.
    for (id, candidates) in unqualified {
        if candidates.len() == 1 {
            flat.insert(id, candidates.into_iter().next().expect("one candidate"));
        }
    }
    flat
}

/// Apply exact provider/model effort metadata without persisting
/// it as a user override. Both chat opening and model switching use this path.
/// Explicit profile levels win; unavailable metadata leaves offline defaults
/// to the protocol adapter. This does not infer effort controls from toggles.
pub async fn resolve_reasoning_efforts(profile: &mut crate::ProviderProfile) {
    if profile.reasoning_efforts.is_some() {
        return;
    }
    let data = crate::catalogue::snapshot();
    apply_reasoning_efforts(profile, &data.entries);
}

fn apply_reasoning_efforts(
    profile: &mut crate::ProviderProfile,
    entries: &BTreeMap<String, ModelMeta>,
) {
    if profile.reasoning_efforts.is_some() {
        return;
    }
    if profile.protocol != Protocol::GeminiInteractions {
        if let Some(provider) = &profile.provider_id {
            profile.reasoning_efforts = entries
                .get(&format!("{provider}/{}", profile.model).to_lowercase())
                .and_then(|meta| meta.reasoning_efforts.clone());
        }
        return;
    }
    let id = profile
        .model
        .strip_prefix("models/")
        .unwrap_or(&profile.model);
    let qualified = if id.starts_with("google/") {
        id.to_owned()
    } else {
        format!("google/{id}")
    };
    // An unqualified same-named model from another provider is not evidence
    // about native Google Interactions, even when it is the only catalogue row.
    if let Some(meta) = entries.get(&qualified) {
        profile.reasoning_efforts = meta.reasoning_efforts.clone();
    }
}

/// Look up `model_id`'s context window from models.dev. Returns `None` on
/// network failure or if the model genuinely isn't listed — never a guess.
pub async fn context_window(model_id: &str) -> Option<u32> {
    let data = crate::catalogue::snapshot();
    lookup(model_id, &data.entries).and_then(|m| m.context)
}

/// Look up `model_id`'s list price (USD per MTok input, output). This is the
/// vendor's list price — for a self-hosted route it's an *indicative* figure
/// only; callers must label it as such. `None` = not listed, never a guess.
/// List price for a route: input, output, and the cache-read rate when the
/// catalogue publishes one.
pub async fn pricing(model_id: &str) -> Option<(f64, f64, Option<f64>)> {
    let data = crate::catalogue::snapshot();
    let meta = lookup(model_id, &data.entries)?;
    Some((
        meta.input_per_mtok?,
        meta.output_per_mtok?,
        meta.cached_input_per_mtok,
    ))
}

/// Look up capability metadata using only an exact model id. A provider prefix
/// is accepted (`provider/model`); the unqualified form is available only when
/// the catalog contains no same-named model from another provider.
pub async fn capabilities(model_id: &str) -> Option<ModelCapabilities> {
    if model_id.trim().is_empty() {
        return None;
    }
    let data = crate::catalogue::snapshot();
    exact_lookup(model_id, &data.entries).and_then(|meta| meta.capabilities.clone())
}

/// Resolve a model's capabilities with a user-supplied exact override taking
/// precedence over the public catalog. The override is borrowed so callers can
/// pass a profile value without cloning unless catalog lookup is needed.
pub async fn resolve_capabilities(
    model_id: &str,
    override_capabilities: Option<&ModelCapabilities>,
) -> CapabilityResolution {
    if let Some(capabilities) = override_capabilities {
        return CapabilityResolution {
            capabilities: Some(capabilities.clone()),
            source: CapabilitySource::ProfileOverride,
        };
    }
    capabilities(model_id).await.map_or_else(
        || CapabilityResolution {
            capabilities: None,
            source: CapabilitySource::Unknown,
        },
        |capabilities| CapabilityResolution {
            capabilities: Some(capabilities),
            source: CapabilitySource::ModelsDev,
        },
    )
}

/// Deployment overrides win. An explicit provider id requires an exact
/// provider/model match; it never borrows a same-named model from another host.
pub async fn resolve_profile_capabilities(
    profile: &crate::ProviderProfile,
) -> CapabilityResolution {
    if profile.provider_id.is_none() {
        return resolve_capabilities(&profile.model, profile.capabilities.as_ref()).await;
    }
    if let Some(capabilities) = &profile.capabilities {
        return CapabilityResolution {
            capabilities: Some(capabilities.clone()),
            source: CapabilitySource::ProfileOverride,
        };
    }
    let data = crate::catalogue::snapshot();
    let capabilities =
        profile_meta(profile, &data.entries, false).and_then(|meta| meta.capabilities.clone());
    CapabilityResolution {
        source: if capabilities.is_some() {
            CapabilitySource::ModelsDev
        } else {
            CapabilitySource::Unknown
        },
        capabilities,
    }
}

pub async fn profile_context_window(profile: &crate::ProviderProfile) -> Option<u32> {
    if let Some(context) = profile.max_ctx {
        return Some(context);
    }
    if profile.provider_id.is_none() {
        return context_window(&profile.model).await;
    }
    let data = crate::catalogue::snapshot();
    profile_meta(profile, &data.entries, true).and_then(|meta| meta.context)
}

/// List price for a profile: input, output, then cache re-read and cache write
/// where the catalogue publishes them.
pub async fn profile_pricing(
    profile: &crate::ProviderProfile,
) -> Option<(f64, f64, Option<f64>, Option<f64>)> {
    let data = crate::catalogue::snapshot();
    let meta = profile_meta(profile, &data.entries, true)?;
    Some((
        meta.input_per_mtok?,
        meta.output_per_mtok?,
        meta.cached_input_per_mtok,
        meta.cache_write_per_mtok,
    ))
}

fn profile_meta<'a>(
    profile: &crate::ProviderProfile,
    entries: &'a BTreeMap<String, ModelMeta>,
    legacy_fuzzy: bool,
) -> Option<&'a ModelMeta> {
    if profile.provider_id.is_some() {
        entries.get(&profile.catalogue_model_id().to_lowercase())
    } else if legacy_fuzzy {
        lookup(&profile.model, entries)
    } else {
        exact_lookup(&profile.model, entries)
    }
}

fn exact_lookup<'a>(
    model_id: &str,
    entries: &'a BTreeMap<String, ModelMeta>,
) -> Option<&'a ModelMeta> {
    let needle = model_id.trim().to_lowercase();
    if let Some(meta) = entries.get(&needle) {
        return Some(meta);
    }
    let tail = needle.rsplit('/').next().unwrap_or(&needle);
    (tail != needle).then(|| entries.get(tail)).flatten()
}

fn lookup<'a>(model_id: &str, entries: &'a BTreeMap<String, ModelMeta>) -> Option<&'a ModelMeta> {
    let needle = model_id.to_lowercase();
    // Exact match on the full id (as given, and stripped of a provider prefix).
    if let Some(meta) = entries.get(&needle) {
        return Some(meta);
    }
    let tail = needle.rsplit('/').next().unwrap_or(&needle);
    if let Some(meta) = entries.get(tail) {
        return Some(meta);
    }
    entries
        .iter()
        .filter(|(key, _)| {
            needle.contains(key.as_str()) || tail.contains(key.as_str()) || key.contains(tail)
        })
        // `BTreeMap` order makes equal-length fuzzy matches deterministic.
        .max_by_key(|(key, _)| key.len())
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::{Cache, SCHEMA as CACHE_SCHEMA_VERSION};

    /// Both cache prices come from the catalogue. A model that lists neither
    /// has neither: an absent price is never taken to be zero.
    #[test]
    fn cache_prices_are_read_from_the_catalogue_when_it_lists_them() {
        let raw = serde_json::from_value(serde_json::json!({
            "one":{"models":{
                "caching":{"cost":{"input":3.0,"output":15.0,"cache_read":0.3,"cache_write":3.75}},
                "plain":{"cost":{"input":1.0,"output":2.0}}
            }}
        }))
        .unwrap();
        let entries = flatten(raw);
        let prices = |model: &str| {
            let meta = lookup(model, &entries).unwrap();
            (meta.cached_input_per_mtok, meta.cache_write_per_mtok)
        };
        assert_eq!(prices("caching"), (Some(0.3), Some(3.75)));
        assert_eq!(prices("plain"), (None, None));
    }

    #[test]
    fn an_explicit_provider_never_inherits_another_deployments_metadata() {
        let raw = serde_json::from_value(serde_json::json!({
            "one":{"models":{"same":{"limit":{"context":8000},"cost":{"input":1.0,"output":2.0},"tool_call":true,"reasoning_options":[{"type":"effort","values":["low","high"]}]} }},
            "two":{"models":{"same":{"limit":{"context":16000},"cost":{"input":3.0,"output":4.0},"tool_call":false,"reasoning_options":[{"type":"toggle"}]} }}
        })).unwrap();
        let entries = flatten(raw);
        let mut profile = crate::ProviderProfile::openai_chat(
            "https://one.test/v1",
            "same",
            crate::AuthKind::Bearer,
        );
        profile.provider_id = Some("one".into());
        let meta = profile_meta(&profile, &entries, true).unwrap();
        assert_eq!(meta.context, Some(8000));
        assert_eq!(meta.input_per_mtok, Some(1.0));
        assert_eq!(meta.capabilities.as_ref().unwrap().tool_calls, Some(true));
        apply_reasoning_efforts(&mut profile, &entries);
        assert_eq!(
            profile.reasoning_efforts.as_deref(),
            Some([ReasoningEffort::Low, ReasoningEffort::High].as_slice())
        );
        profile.provider_id = Some("unknown".into());
        profile.reasoning_efforts = None;
        assert!(profile_meta(&profile, &entries, true).is_none());
        apply_reasoning_efforts(&mut profile, &entries);
        assert!(profile.reasoning_efforts.is_none());
        profile.provider_id = Some("two".into());
        apply_reasoning_efforts(&mut profile, &entries);
        assert!(
            profile.reasoning_efforts.is_none(),
            "a toggle is not an effort list"
        );
        profile.reasoning_efforts = Some(vec![ReasoningEffort::Medium]);
        profile.provider_id = Some("one".into());
        apply_reasoning_efforts(&mut profile, &entries);
        assert_eq!(
            profile.reasoning_efforts,
            Some(vec![ReasoningEffort::Medium]),
            "intentional deployment overrides win"
        );
    }

    #[test]
    fn null_and_unrecognized_effort_values_do_not_break_other_models_metadata() {
        // Sarvam's current public records include null as the default option;
        // it is not an effort name and must not invalidate the full catalogue.
        let providers = serde_json::from_value(serde_json::json!({
            "sarvam":{"models":{
                "sarvam-30b":{"limit":{"context":32768},"reasoning_options":[{"type":"effort","values":[null,"low","medium","high"]}]},
                "sarvam-105b":{"reasoning":true,"reasoning_options":[{"type":"effort","values":[null,"low","medium","high"]}]}
            }},
            "google":{"models":{"gemini-3.8-flash":{"reasoning_options":[{"type":"effort","values":["low","medium","high"]}]}}}
        })).expect("unrecognized options cannot invalidate unrelated capabilities/prices");
        let entries = flatten(providers);
        for id in [
            "sarvam/sarvam-30b",
            "sarvam/sarvam-105b",
            "google/gemini-3.8-flash",
        ] {
            assert_eq!(
                entries[id].reasoning_efforts.as_deref(),
                Some(
                    [
                        ReasoningEffort::Low,
                        ReasoningEffort::Medium,
                        ReasoningEffort::High
                    ]
                    .as_slice()
                )
            );
        }
        assert_eq!(entries["sarvam/sarvam-30b"].context, Some(32768));
        let future: ModelEntry = serde_json::from_value(serde_json::json!({
            "reasoning_options":[{"type":"effort","values":[0,{"future":true}]}]
        }))
        .unwrap();
        assert!(future.reasoning_efforts().is_none());
    }

    #[test]
    fn catalogue_efforts_round_trip_and_do_not_turn_toggles_or_unknown_levels_into_efforts() {
        let providers = serde_json::from_value(serde_json::json!({
            "google": {"models": {
                "gemini-3.8-flash": {"reasoning":true, "reasoning_options":[{"type":"effort","values":["low","medium","high"]}]},
                "gemini-3.6-flash": {"reasoning_options":[{"type":"effort","values":["minimal","low","medium","high"]}]},
                "toggle-only": {"reasoning":true,"reasoning_options":[{"type":"toggle"}]},
                "future": {"reasoning":true,"reasoning_options":[{"type":"effort","values":["future-level"]}]},
                "mixed": {"reasoning_options":[{"type":"new-control","anything":true},{"type":"effort","values":["low","future-level","high","low"]}]}
            }}
        })).unwrap();
        let entries = flatten(providers);
        assert_eq!(
            entries["google/gemini-3.8-flash"]
                .reasoning_efforts
                .as_deref(),
            Some(
                [
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High
                ]
                .as_slice()
            )
        );
        assert!(
            entries["google/gemini-3.6-flash"]
                .reasoning_efforts
                .as_ref()
                .unwrap()
                .contains(&ReasoningEffort::Minimal)
        );
        assert!(entries["google/toggle-only"].reasoning_efforts.is_none());
        assert!(entries["google/future"].reasoning_efforts.is_none());
        assert_eq!(
            entries["google/mixed"].reasoning_efforts.as_deref(),
            Some([ReasoningEffort::Low, ReasoningEffort::High].as_slice())
        );
        let cache = Cache {
            schema_version: CACHE_SCHEMA_VERSION,
            fetched_at_unix: 0,
            revision: String::new(),
            providers: Vec::new(),
            entries,
        };
        let reloaded: Cache =
            serde_json::from_str(&serde_json::to_string(&cache).unwrap()).unwrap();
        assert_eq!(
            reloaded.entries["google/gemini-3.8-flash"].reasoning_efforts,
            cache.entries["google/gemini-3.8-flash"].reasoning_efforts
        );
        let previous: Cache =
            serde_json::from_str(r#"{"schema_version":3,"fetched_at_unix":0,"entries":{}}"#)
                .unwrap();
        assert!(
            !previous.current_schema(),
            "an upgrade refreshes previously discarded effort metadata"
        );
    }

    #[test]
    fn native_reasoning_metadata_is_exact_and_preserves_explicit_profile_levels() {
        let entries = flatten(serde_json::from_value(serde_json::json!({
            "google": {"models":{"catalogue-model":{"reasoning_options":[{"type":"effort","values":["low","high"]}]}}},
            "another-provider":{"models":{"different-model":{"reasoning_options":[{"type":"effort","values":["minimal"]}]}}}
        })).unwrap());
        let mut profile = crate::ProviderProfile::openai_chat(
            "http://localhost/v1",
            "catalogue-model",
            crate::AuthKind::None,
        );
        apply_reasoning_efforts(&mut profile, &entries);
        assert!(
            profile.reasoning_efforts.is_none(),
            "native Gemini metadata cannot change Chat Completions controls"
        );
        profile.protocol = Protocol::GeminiInteractions;
        apply_reasoning_efforts(&mut profile, &entries);
        assert_eq!(
            profile.reasoning_efforts.as_deref(),
            Some([ReasoningEffort::Low, ReasoningEffort::High].as_slice())
        );
        profile.reasoning_efforts = Some(vec![ReasoningEffort::Minimal]);
        apply_reasoning_efforts(&mut profile, &entries);
        assert_eq!(
            profile.reasoning_efforts.as_deref(),
            Some([ReasoningEffort::Minimal].as_slice())
        );
        profile.reasoning_efforts = None;
        profile.model = "different-model".into();
        apply_reasoning_efforts(&mut profile, &entries);
        assert!(
            profile.reasoning_efforts.is_none(),
            "another provider's unqualified catalogue row is not native Google metadata"
        );
        profile.model = "catalogue-model-preview".into();
        apply_reasoning_efforts(&mut profile, &entries);
        assert!(
            profile.reasoning_efforts.is_none(),
            "do not infer preview/alias effort levels by fuzzy matching"
        );
    }

    #[test]
    fn legacy_metadata_cache_requires_refresh_without_losing_offline_prices() {
        let legacy = r#"{"fetched_at_unix":123,"entries":{"m":{"context":128000,"input_per_mtok":1.0,"output_per_mtok":4.0}}}"#;
        let mut cache: Cache = serde_json::from_str(legacy).unwrap();
        assert!(!cache.current_schema());
        assert_eq!(cache.entries["m"].input_per_mtok, Some(1.0));
        assert!(cache.entries["m"].capabilities.is_none());
        cache.schema_version = CACHE_SCHEMA_VERSION;
        let reloaded: Cache =
            serde_json::from_str(&serde_json::to_string(&cache).unwrap()).unwrap();
        assert!(reloaded.current_schema());
    }

    #[test]
    fn exact_and_fuzzy_match() {
        let mut m = BTreeMap::new();
        m.insert(
            "qwen3-32b".to_string(),
            ModelMeta {
                context: Some(131_072),
                input_per_mtok: None,
                output_per_mtok: None,
                cached_input_per_mtok: None,
                cache_write_per_mtok: None,
                capabilities: None,
                reasoning_efforts: None,
            },
        );
        m.insert(
            "claude-opus-4".to_string(),
            ModelMeta {
                context: Some(200_000),
                input_per_mtok: Some(15.0),
                output_per_mtok: Some(75.0),
                cached_input_per_mtok: None,
                cache_write_per_mtok: None,
                capabilities: None,
                reasoning_efforts: None,
            },
        );

        assert_eq!(
            lookup("qwen3-32b", &m).and_then(|x| x.context),
            Some(131_072)
        );
        assert_eq!(
            lookup("nvidia/qwen3-32b-instruct", &m).and_then(|x| x.context),
            Some(131_072)
        );
        assert!(lookup("totally-unknown-model", &m).is_none());
        // Pricing present only when models.dev lists both sides.
        let opus = lookup("claude-opus-4", &m).unwrap();
        assert_eq!(opus.input_per_mtok, Some(15.0));
        assert_eq!(opus.output_per_mtok, Some(75.0));
    }

    #[test]
    fn ambiguous_fuzzy_match_is_deterministic_and_prefers_the_most_specific_id() {
        let generic = ModelMeta {
            context: Some(8_192),
            input_per_mtok: Some(1.0),
            output_per_mtok: Some(2.0),
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
            capabilities: None,
            reasoning_efforts: None,
        };
        let specific = ModelMeta {
            context: Some(131_072),
            input_per_mtok: Some(3.0),
            output_per_mtok: Some(4.0),
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
            capabilities: None,
            reasoning_efforts: None,
        };
        let entries = BTreeMap::from([
            ("qwen3".to_string(), generic),
            ("qwen3-32b".to_string(), specific.clone()),
        ]);

        for _ in 0..32 {
            let matched = lookup("vendor/qwen3-32b-instruct", &entries).unwrap();
            assert_eq!(matched.context, specific.context);
            assert_eq!(matched.input_per_mtok, specific.input_per_mtok);
            assert_eq!(matched.output_per_mtok, specific.output_per_mtok);
        }
    }

    #[test]
    fn capability_states_keep_unknown_separate_from_unsupported() {
        let unknown = ModelCapabilities::default();
        assert_eq!(unknown.input_state("image"), CapabilityState::Unknown);
        assert_eq!(unknown.attachment_state(), CapabilityState::Unknown);

        let known = ModelCapabilities {
            modalities: Some(ModalitySet {
                input: Some(BTreeSet::from(["text".into(), "image".into()])),
                output: Some(BTreeSet::from(["text".into()])),
            }),
            attachment: Some(false),
            tool_calls: Some(true),
            reasoning: None,
            ..ModelCapabilities::default()
        };
        assert_eq!(known.input_state("image"), CapabilityState::Supported);
        assert_eq!(known.input_state("audio"), CapabilityState::Unsupported);
        assert_eq!(known.output_state("image"), CapabilityState::Unsupported);
        assert_eq!(known.attachment_state(), CapabilityState::Unsupported);
        assert_eq!(known.tool_call_state(), CapabilityState::Supported);
        assert_eq!(known.reasoning_state(), CapabilityState::Unknown);
        assert_eq!(known.user_image_state(), CapabilityState::Supported);
        assert_eq!(known.tool_result_image_state(), CapabilityState::Unknown);
    }

    #[test]
    fn partial_modality_metadata_preserves_unknown_directions_on_round_trip() {
        for (json, input, output) in [
            (r#"{}"#, CapabilityState::Unknown, CapabilityState::Unknown),
            (
                r#"{"output":["image"]}"#,
                CapabilityState::Unknown,
                CapabilityState::Supported,
            ),
            (
                r#"{"input":["image"]}"#,
                CapabilityState::Supported,
                CapabilityState::Unknown,
            ),
            (
                r#"{"input":[],"output":[]}"#,
                CapabilityState::Unsupported,
                CapabilityState::Unsupported,
            ),
        ] {
            let modalities: ModalitySet = serde_json::from_str(json).unwrap();
            let encoded = serde_json::to_string(&modalities).unwrap();
            let reloaded: ModalitySet = serde_json::from_str(&encoded).unwrap();
            assert_eq!(reloaded.input_state("image"), input, "{json}");
            assert_eq!(reloaded.output_state("image"), output, "{json}");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&encoded).unwrap(),
                serde_json::from_str::<serde_json::Value>(json).unwrap()
            );
        }
    }

    #[test]
    fn legacy_cache_drops_ambiguous_modalities_but_preserves_other_metadata() {
        let json = r#"{"schema_version":1,"fetched_at_unix":123,"entries":{"m":{"context":128000,"input_per_mtok":1.0,"output_per_mtok":4.0,"capabilities":{"modalities":{"input":[],"output":[]},"tool_call":true}}}}"#;
        let mut cache: Cache = serde_json::from_str(json).unwrap();
        cache.discard_ambiguous_legacy_modalities();
        let meta = &cache.entries["m"];
        assert_eq!(meta.context, Some(128000));
        assert_eq!(meta.input_per_mtok, Some(1.0));
        assert_eq!(meta.output_per_mtok, Some(4.0));
        let capabilities = meta.capabilities.as_ref().unwrap();
        assert_eq!(capabilities.input_state("image"), CapabilityState::Unknown);
        assert_eq!(capabilities.tool_call_state(), CapabilityState::Supported);
        assert!(!cache.current_schema());

        let mut current: Cache = serde_json::from_str(json).unwrap();
        current.schema_version = CACHE_SCHEMA_VERSION;
        current.discard_ambiguous_legacy_modalities();
        assert_eq!(
            current.entries["m"]
                .capabilities
                .as_ref()
                .unwrap()
                .input_state("image"),
            CapabilityState::Unsupported
        );
    }

    #[test]
    fn exact_capability_lookup_never_uses_fuzzy_model_names() {
        let capabilities = ModelCapabilities {
            modalities: Some(ModalitySet {
                input: Some(BTreeSet::from(["image".into()])),
                output: Some(BTreeSet::from(["text".into()])),
            }),
            ..ModelCapabilities::default()
        };
        let entries = BTreeMap::from([(
            "vendor/qwen3-32b".to_string(),
            ModelMeta {
                capabilities: Some(capabilities.clone()),
                ..ModelMeta::default()
            },
        )]);

        assert!(exact_lookup("vendor/qwen3-32b-instruct", &entries).is_none());
        assert_eq!(
            exact_lookup("vendor/qwen3-32b", &entries).and_then(|meta| meta.capabilities.clone()),
            Some(capabilities)
        );
    }
}
