//! Model metadata lookup via models.dev (§4.4) — a real, externally
//! maintained metadata database (the same source opencode uses), not a
//! hardcoded table baked into the binary. Fetched once, cached to disk, and
//! matched by model id. If a model genuinely isn't in it, we say so and leave
//! the value unknown (P2: never fabricate a number) — the caller then either
//! asks the user to set one explicitly or disables the dependent feature,
//! matching how opencode behaves when metadata can't be resolved for a
//! local/custom model. Carries both context windows and per-MTok list prices
//! (the latter feed the cost meter, P1-12 — indicative for self-hosted routes).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_URL: &str = "https://models.dev/api.json";
/// Re-fetch if the cache is older than this; models.dev updates periodically,
/// not every second, so a cached copy is fine to reuse for a while.
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Everything we retain per model. Old cache files (context-only format) fail
/// to deserialize into this and are simply re-fetched.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ModelMeta {
    pub context: Option<u32>,
    /// USD per million input tokens (models.dev list price).
    pub input_per_mtok: Option<f64>,
    /// USD per million output tokens (models.dev list price).
    pub output_per_mtok: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cache {
    fetched_at_unix: u64,
    /// lowercased model id → metadata
    entries: HashMap<String, ModelMeta>,
}

#[derive(Deserialize)]
struct Provider {
    #[serde(default)]
    models: HashMap<String, ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    #[serde(default)]
    limit: Option<Limit>,
    #[serde(default)]
    cost: Option<Cost>,
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
    let cache: Cache = serde_json::from_str(&text).ok()?;
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

async fn fetch_and_flatten(client: &reqwest::Client) -> Result<HashMap<String, ModelMeta>, String> {
    let resp = client
        .get(API_URL)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("models.dev returned {}", resp.status()));
    }
    let providers: HashMap<String, Provider> = resp.json().await.map_err(|e| e.to_string())?;
    let mut flat = HashMap::new();
    for provider in providers.into_values() {
        for (id, model) in provider.models {
            let meta = ModelMeta {
                context: model.limit.and_then(|l| l.context),
                input_per_mtok: model.cost.as_ref().and_then(|c| c.input),
                output_per_mtok: model.cost.as_ref().and_then(|c| c.output),
            };
            if meta.context.is_some() || meta.input_per_mtok.is_some() {
                flat.insert(id.to_lowercase(), meta);
            }
        }
    }
    Ok(flat)
}

/// Load the metadata table: fresh disk cache if present, else fetch + re-cache.
async fn entries() -> Option<HashMap<String, ModelMeta>> {
    if let Some(cache) = load_disk_cache() {
        return Some(cache.entries);
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let fetched = fetch_and_flatten(&client).await.ok()?;
    save_disk_cache(&Cache {
        fetched_at_unix: now_unix(),
        entries: fetched.clone(),
    });
    Some(fetched)
}

/// Look up `model_id`'s context window from models.dev. Returns `None` on
/// network failure or if the model genuinely isn't listed — never a guess.
pub async fn context_window(model_id: &str) -> Option<u32> {
    lookup(model_id, &entries().await?).and_then(|m| m.context)
}

/// Look up `model_id`'s list price (USD per MTok input, output). This is the
/// vendor's list price — for a self-hosted route it's an *indicative* figure
/// only; callers must label it as such. `None` = not listed, never a guess.
pub async fn pricing(model_id: &str) -> Option<(f64, f64)> {
    let meta = lookup(model_id, &entries().await?)?;
    Some((meta.input_per_mtok?, meta.output_per_mtok?))
}

fn lookup(model_id: &str, entries: &HashMap<String, ModelMeta>) -> Option<ModelMeta> {
    let needle = model_id.to_lowercase();
    // Exact match on the full id (as given, and stripped of a provider prefix).
    if let Some(&meta) = entries.get(&needle) {
        return Some(meta);
    }
    let tail = needle.rsplit('/').next().unwrap_or(&needle);
    if let Some(&meta) = entries.get(tail) {
        return Some(meta);
    }
    // Fuzzy: the known id is a substring of ours, or ours is a substring of it
    // (handles version/quantization suffixes like "-bf16", "-instruct").
    entries
        .iter()
        .find(|(k, _)| needle.contains(k.as_str()) || tail.contains(k.as_str()) || k.contains(tail))
        .map(|(_, v)| *v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_fuzzy_match() {
        let mut m = HashMap::new();
        m.insert(
            "qwen3-32b".to_string(),
            ModelMeta {
                context: Some(131_072),
                input_per_mtok: None,
                output_per_mtok: None,
            },
        );
        m.insert(
            "claude-opus-4".to_string(),
            ModelMeta {
                context: Some(200_000),
                input_per_mtok: Some(15.0),
                output_per_mtok: Some(75.0),
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
}
