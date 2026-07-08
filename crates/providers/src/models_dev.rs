//! Model context-window lookup via models.dev (§4.4) — a real, externally
//! maintained metadata database (the same source opencode uses), not a
//! hardcoded table baked into the binary. Fetched once, cached to disk, and
//! matched by model id. If a model genuinely isn't in it, we say so and leave
//! the window unknown (P2: never fabricate a number) — the caller then either
//! asks the user to set one explicitly or disables compaction, matching how
//! opencode behaves when metadata can't be resolved for a local/custom model.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_URL: &str = "https://models.dev/api.json";
/// Re-fetch if the cache is older than this; models.dev updates periodically,
/// not every second, so a cached copy is fine to reuse for a while.
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cache {
    fetched_at_unix: u64,
    /// lowercased model id → context window (tokens)
    entries: HashMap<String, u32>,
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
}

#[derive(Deserialize)]
struct Limit {
    #[serde(default)]
    context: Option<u32>,
}

fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".medha").join("models_dev_cache.json"))
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
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

async fn fetch_and_flatten(client: &reqwest::Client) -> Result<HashMap<String, u32>, String> {
    let resp = client.get(API_URL).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("models.dev returned {}", resp.status()));
    }
    let providers: HashMap<String, Provider> = resp.json().await.map_err(|e| e.to_string())?;
    let mut flat = HashMap::new();
    for provider in providers.into_values() {
        for (id, model) in provider.models {
            if let Some(ctx) = model.limit.and_then(|l| l.context) {
                flat.insert(id.to_lowercase(), ctx);
            }
        }
    }
    Ok(flat)
}

/// Look up `model_id`'s context window from models.dev. Uses a fresh disk
/// cache if present; otherwise fetches live and re-caches. Returns `None` on
/// network failure or if the model genuinely isn't listed — never a guess.
pub async fn context_window(model_id: &str) -> Option<u32> {
    let entries = if let Some(cache) = load_disk_cache() {
        cache.entries
    } else {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .ok()?;
        let entries = fetch_and_flatten(&client).await.ok()?;
        save_disk_cache(&Cache { fetched_at_unix: now_unix(), entries: entries.clone() });
        entries
    };
    lookup(model_id, &entries)
}

fn lookup(model_id: &str, entries: &HashMap<String, u32>) -> Option<u32> {
    let needle = model_id.to_lowercase();
    // Exact match on the full id (as given, and stripped of a provider prefix).
    if let Some(&ctx) = entries.get(&needle) {
        return Some(ctx);
    }
    let tail = needle.rsplit('/').next().unwrap_or(&needle);
    if let Some(&ctx) = entries.get(tail) {
        return Some(ctx);
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
        m.insert("qwen3-32b".to_string(), 131_072u32);
        m.insert("claude-opus-4".to_string(), 200_000u32);

        assert_eq!(lookup("qwen3-32b", &m), Some(131_072));
        assert_eq!(lookup("nvidia/qwen3-32b-instruct", &m), Some(131_072));
        assert_eq!(lookup("totally-unknown-model", &m), None);
    }
}
