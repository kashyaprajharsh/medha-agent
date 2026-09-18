//! Cached models.dev metadata. Missing values remain unknown; prices are
//! indicative for self-hosted routes.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_URL: &str = "https://models.dev/api.json";
/// Re-fetch if the cache is older than this; models.dev updates periodically,
/// not every second, so a cached copy is fine to reuse for a while.
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const CACHE_SCHEMA_VERSION: u32 = 2;

static PROCESS_ENTRIES: tokio::sync::OnceCell<BTreeMap<String, ModelMeta>> =
    tokio::sync::OnceCell::const_new();

/// Whether a model capability is known from an authoritative source.
///
/// Capability discovery must keep "not listed" separate from an explicit
/// negative. A custom endpoint may support an input even when the catalog has
/// no entry for it, so callers must not turn an unknown value into `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
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

impl Default for CapabilityState {
    fn default() -> Self {
        Self::Unknown
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
    #[serde(default, rename = "tool_call")]
    pub tool_calls: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilitySource {
    ProfileOverride,
    ModelsDev,
    Unknown,
}

impl Default for CapabilitySource {
    fn default() -> Self {
        Self::Unknown
    }
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
    #[serde(default)]
    pub capabilities: Option<ModelCapabilities>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cache {
    /// Old caches retain prices but lack capability fields. They must refresh
    /// after upgrading rather than claim that capabilities are unknown for a week.
    #[serde(default)]
    schema_version: u32,
    fetched_at_unix: u64,
    /// lowercased model id → metadata
    entries: BTreeMap<String, ModelMeta>,
}

impl Cache {
    fn discard_ambiguous_legacy_modalities(&mut self) {
        if self.schema_version < 2 {
            // Older serializers wrote missing directions as empty arrays.
            // Their negatives cannot be trusted, including during offline fallback.
            for meta in self.entries.values_mut() {
                if let Some(capabilities) = &mut meta.capabilities {
                    capabilities.modalities = None;
                }
            }
        }
    }

    fn current_schema(&self) -> bool {
        self.schema_version == CACHE_SCHEMA_VERSION
    }
}

#[derive(Deserialize)]
struct Provider {
    #[serde(default)]
    models: BTreeMap<String, ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
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
}

fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".medha").join("models_dev_cache.json"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_disk_cache() -> Option<Cache> {
    let path = cache_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let mut cache: Cache = serde_json::from_str(&text).ok()?;
    cache.discard_ambiguous_legacy_modalities();
    let fresh = now_unix().saturating_sub(cache.fetched_at_unix) < CACHE_TTL.as_secs();
    fresh.then_some(cache)
}

fn save_disk_cache(cache: &Cache) {
    if let Some(path) = cache_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string(cache) {
            let _ = std::fs::write(path, text);
        }
    }
}

async fn fetch_and_flatten(
    client: &reqwest::Client,
) -> Result<BTreeMap<String, ModelMeta>, String> {
    let resp = client
        .get(API_URL)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("models.dev returned {}", resp.status()));
    }
    let providers: BTreeMap<String, Provider> = resp.json().await.map_err(|e| e.to_string())?;
    let mut flat = BTreeMap::new();
    let mut unqualified: BTreeMap<String, Vec<ModelMeta>> = BTreeMap::new();
    for (provider_id, provider) in providers {
        for (id, model) in provider.models {
            let capability_values = ModelCapabilities {
                modalities: model.modalities,
                attachment: model.attachment,
                tool_calls: model.tool_calls,
                reasoning: model.reasoning,
            };
            let meta = ModelMeta {
                context: model.limit.and_then(|l| l.context),
                input_per_mtok: model.cost.as_ref().and_then(|c| c.input),
                output_per_mtok: model.cost.as_ref().and_then(|c| c.output),
                capabilities: capability_values
                    .has_known_value()
                    .then_some(capability_values),
            };
            if meta.context.is_some()
                || meta.input_per_mtok.is_some()
                || meta.capabilities.is_some()
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
    Ok(flat)
}

/// Load the metadata table: fresh disk cache if present, else fetch + re-cache.
async fn entries() -> Option<BTreeMap<String, ModelMeta>> {
    if let Some(entries) = PROCESS_ENTRIES.get() {
        return Some(entries.clone());
    }
    let cached = load_disk_cache();
    if let Some(cache) = cached.as_ref().filter(|cache| cache.current_schema()) {
        let entries = cache.entries.clone();
        let _ = PROCESS_ENTRIES.set(entries.clone());
        return Some(entries);
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let fetched = match fetch_and_flatten(&client).await {
        Ok(entries) => entries,
        Err(_) => {
            // Offline upgrades can still use the old prices/context metadata.
            // Do not rewrite it with the new version: retry refresh next launch.
            let entries = cached?.entries;
            let _ = PROCESS_ENTRIES.set(entries.clone());
            return Some(entries);
        }
    };
    save_disk_cache(&Cache {
        schema_version: CACHE_SCHEMA_VERSION,
        fetched_at_unix: now_unix(),
        entries: fetched.clone(),
    });
    let _ = PROCESS_ENTRIES.set(fetched.clone());
    Some(fetched)
}

/// Look up `model_id`'s context window from models.dev. Returns `None` on
/// network failure or if the model genuinely isn't listed — never a guess.
pub async fn context_window(model_id: &str) -> Option<u32> {
    let entries = entries().await?;
    lookup(model_id, &entries).and_then(|m| m.context)
}

/// Look up `model_id`'s list price (USD per MTok input, output). This is the
/// vendor's list price — for a self-hosted route it's an *indicative* figure
/// only; callers must label it as such. `None` = not listed, never a guess.
pub async fn pricing(model_id: &str) -> Option<(f64, f64)> {
    let entries = entries().await?;
    let meta = lookup(model_id, &entries)?;
    Some((meta.input_per_mtok?, meta.output_per_mtok?))
}

/// Look up capability metadata using only an exact model id. A provider prefix
/// is accepted (`provider/model`); the unqualified form is available only when
/// the catalog contains no same-named model from another provider.
pub async fn capabilities(model_id: &str) -> Option<ModelCapabilities> {
    if model_id.trim().is_empty() {
        return None;
    }
    let entries = entries().await?;
    exact_lookup(model_id, &entries).and_then(|meta| meta.capabilities.clone())
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
                capabilities: None,
            },
        );
        m.insert(
            "claude-opus-4".to_string(),
            ModelMeta {
                context: Some(200_000),
                input_per_mtok: Some(15.0),
                output_per_mtok: Some(75.0),
                capabilities: None,
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
            capabilities: None,
        };
        let specific = ModelMeta {
            context: Some(131_072),
            input_per_mtok: Some(3.0),
            output_per_mtok: Some(4.0),
            capabilities: None,
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
        };
        assert_eq!(known.input_state("image"), CapabilityState::Supported);
        assert_eq!(known.input_state("audio"), CapabilityState::Unsupported);
        assert_eq!(known.output_state("image"), CapabilityState::Unsupported);
        assert_eq!(known.attachment_state(), CapabilityState::Unsupported);
        assert_eq!(known.tool_call_state(), CapabilityState::Supported);
        assert_eq!(known.reasoning_state(), CapabilityState::Unknown);
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
