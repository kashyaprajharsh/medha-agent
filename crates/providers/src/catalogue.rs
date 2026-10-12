//! Shared, secret-free provider metadata and daemon-aware catalogue lifecycle.
//! Provider identity is metadata; credentials stay bound to saved deployments.

use crate::{AuthKind, models_dev::ModelMeta};
use futures::StreamExt;
use kernel::Protocol;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const SCHEMA: u32 = 6;
const API_URL: &str = "https://models.dev/api.json";
const TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const RETRY: Duration = Duration::from_secs(5 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BYTES: usize = 16 * 1024 * 1024;
static CATALOGUE: OnceLock<Arc<Catalogue>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogueModel {
    pub id: String,
    pub name: String,
    pub context_length: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRecord {
    pub id: String,
    pub name: String,
    /// A template is never a credential identifier or a network destination.
    pub url: Option<String>,
    pub parameters: Vec<String>,
    pub protocol: Option<Protocol>,
    pub auth: AuthKind,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub doc: Option<String>,
    pub env: Vec<String>,
    pub npm: Option<String>,
    pub models: Vec<CatalogueModel>,
}

/// An immutable publication. Existing readers remain valid during refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub revision: String,
    pub fetched_at_unix: u64,
    pub providers: Vec<ProviderRecord>,
    pub entries: BTreeMap<String, ModelMeta>,
}

pub(crate) fn revision(
    providers: &[ProviderRecord],
    entries: &BTreeMap<String, ModelMeta>,
) -> String {
    let bytes = serde_json::to_vec(&(providers, entries)).expect("catalogue serialization");
    format!("{:x}", Sha256::digest(bytes))
}

pub fn snapshot() -> Arc<Snapshot> {
    let catalogue = global();
    catalogue.current.read().unwrap().clone()
}

/// The daemon owns refresh on its long-lived executor. Folder handlers and
/// standalone commands may run on short-lived runtimes; reads never spawn a
/// fetch on those runtimes or wait for the network to paint a setup screen.
pub struct Watcher(tokio::task::JoinHandle<()>);
impl Drop for Watcher {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub fn start_background() -> Watcher {
    Watcher(tokio::spawn(async {
        if offline() {
            return;
        }
        let Ok(catalogue) = tokio::task::spawn_blocking(|| global().clone()).await else {
            return;
        };
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let fetched_at = catalogue.current.read().unwrap().fetched_at_unix;
            if now().saturating_sub(fetched_at) >= TTL.as_secs() {
                let _ = catalogue.refresh(false).await;
            }
        }
    }))
}

/// An explicit action refreshes the catalogue; concurrent callers share the
/// same attempt/result. Failure leaves the last usable publication untouched.
pub async fn refresh() -> Result<Arc<Snapshot>, String> {
    if offline() {
        return Err("Provider catalogue refresh is disabled by MEDHA_CATALOGUE_OFFLINE. Bundled/cached metadata remains available.".into());
    }
    global().refresh(true).await
}

fn offline() -> bool {
    std::env::var("MEDHA_CATALOGUE_OFFLINE").is_ok_and(|v| matches!(v.as_str(), "1" | "true"))
}

fn global() -> &'static Arc<Catalogue> {
    CATALOGUE.get_or_init(|| {
        let path = dirs::home_dir().map(|home| home.join(".medha/models_dev_cache.json"));
        Arc::new(Catalogue::new(path, API_URL.into()))
    })
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.' | b'_')
        })
}

pub(crate) fn valid_url(url: &str) -> Result<(), String> {
    if url.len() > 2048
        || url.contains("${")
        || url.trim() != url
        || url.chars().any(char::is_control)
    {
        return Err("Complete the provider endpoint first.".into());
    }
    let profile = crate::ProviderProfile::openai_chat(url, "catalogue", AuthKind::None);
    profile.validate()?;
    if reqwest::Url::parse(url)
        .map_err(|_| "Invalid endpoint.")?
        .query()
        .is_some()
    {
        return Err("Provider endpoints must not contain a query string.".into());
    }
    Ok(())
}

fn template_parameters(url: &str) -> Result<Vec<String>, String> {
    let mut tail = url;
    let mut parameters = Vec::new();
    while let Some((_, rest)) = tail.split_once("${") {
        let (name, remaining) = rest.split_once('}').ok_or("Invalid endpoint template.")?;
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err("Invalid endpoint parameter.".into());
        }
        if ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
            .iter()
            .any(|suffix| name == *suffix || name.ends_with(&format!("_{suffix}")))
        {
            return Err("Credentials cannot be endpoint parameters.".into());
        }
        if !parameters.iter().any(|p| p == name) {
            parameters.push(name.to_owned());
        }
        if parameters.len() > 8 {
            return Err("Too many endpoint parameters.".into());
        }
        tail = remaining;
    }
    Ok(parameters)
}

impl ProviderRecord {
    pub(crate) fn from_metadata(
        id: &str,
        name: &str,
        api: Option<&str>,
        npm: Option<&str>,
    ) -> Self {
        // Packages classify metadata. Only mapped protocols become selectable;
        // native SDKs and subscription routes require their own access policy.
        let (protocol, endpoint) = match id {
            "openai" => (
                Some(Protocol::OpenAiChat),
                Some("https://api.openai.com/v1"),
            ),
            "google" => (
                Some(Protocol::GeminiInteractions),
                Some("https://generativelanguage.googleapis.com/v1"),
            ),
            "groq" => (
                Some(Protocol::OpenAiChat),
                Some("https://api.groq.com/openai/v1"),
            ),
            "togetherai" => (
                Some(Protocol::OpenAiChat),
                Some("https://api.together.ai/v1"),
            ),
            "mistral" => (
                Some(Protocol::OpenAiChat),
                Some("https://api.mistral.ai/v1"),
            ),
            "cerebras" => (
                Some(Protocol::OpenAiChat),
                Some("https://api.cerebras.ai/v1"),
            ),
            "xai" => (Some(Protocol::OpenAiChat), Some("https://api.x.ai/v1")),
            "github-copilot" | "github-copilot-enterprise" => (None, api),
            _ => match npm {
                Some(
                    "@ai-sdk/openai-compatible" | "@ai-sdk/openai" | "@openrouter/ai-sdk-provider",
                ) => (Some(Protocol::OpenAiChat), api),
                Some("@ai-sdk/anthropic") => (Some(Protocol::AnthropicMessages), api),
                _ => (None, api),
            },
        };
        let mut record = Self {
            id: id.into(),
            name: if name.trim().is_empty() {
                id.into()
            } else {
                name.chars().filter(|c| !c.is_control()).take(256).collect()
            },
            url: endpoint.map(str::to_owned),
            parameters: Vec::new(),
            protocol,
            auth: protocol.map_or(AuthKind::Bearer, AuthKind::for_protocol),
            available: false,
            unavailable_reason: None,
            doc: None,
            env: Vec::new(),
            npm: npm.map(str::to_owned),
            models: Vec::new(),
        };
        let validation = (|| {
            if !matches!(
                protocol,
                Some(Protocol::OpenAiChat | Protocol::GeminiInteractions)
            ) {
                return Err(
                    "This provider needs a protocol or sign-in method not yet supported by Medha."
                        .into(),
                );
            }
            let url = record
                .url
                .as_deref()
                .ok_or("This provider needs a deployment endpoint.")?;
            record.parameters = template_parameters(url)?;
            // Validate a template using harmless placeholders, never credentials.
            let sample = record
                .parameters
                .iter()
                .map(|key| {
                    (
                        key.clone(),
                        if key.ends_with("BASE_URL") {
                            "https://example.test".into()
                        } else {
                            "example".into()
                        },
                    )
                })
                .collect();
            record.resolve_endpoint(&sample)?;
            Ok::<_, String>(())
        })();
        record.available = validation.is_ok();
        record.unavailable_reason = validation.err();
        record
    }

    /// Template fields are explicit non-secret values. No environment lookup,
    /// guessed account id, credential substitution or URL redirection occurs.
    pub fn resolve_endpoint(&self, values: &BTreeMap<String, String>) -> Result<String, String> {
        if values.keys().any(|name| !self.parameters.contains(name)) {
            return Err("Unknown endpoint parameter.".into());
        }
        let mut url = self
            .url
            .clone()
            .ok_or("This provider needs a deployment endpoint.")?;
        for name in &self.parameters {
            let value = values
                .get(name)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("Enter {name}."))?;
            if name.ends_with("BASE_URL") {
                valid_url(value)?;
                url = url.replace(&format!("${{{name}}}"), value.trim_end_matches('/'));
            } else {
                if matches!(value, "." | "..")
                    || value.len() > 256
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
                {
                    return Err(format!(
                        "{name} must contain only letters, digits, dots, hyphens or underscores."
                    ));
                }
                url = url.replace(&format!("${{{name}}}"), value);
            }
        }
        valid_url(&url)?;
        Ok(url)
    }
}

pub(crate) fn local_providers() -> Vec<ProviderRecord> {
    [
        ("ollama", "Ollama (local)", "http://localhost:11434/v1"),
        ("lm-studio", "LM Studio (local)", "http://localhost:1234/v1"),
        (
            "llama-cpp",
            "llama.cpp server (local)",
            "http://localhost:8080/v1",
        ),
        ("vllm", "vLLM / SGLang (local)", "http://localhost:8000/v1"),
    ]
    .into_iter()
    .map(|(id, name, url)| {
        let mut record =
            ProviderRecord::from_metadata(id, name, Some(url), Some("@ai-sdk/openai-compatible"));
        record.auth = AuthKind::None;
        record
    })
    .collect()
}

struct Catalogue {
    current: RwLock<Arc<Snapshot>>,
    path: Option<PathBuf>,
    url: String,
    flight: tokio::sync::Mutex<()>,
    attempts: AtomicU64,
    last: Mutex<Option<Result<Arc<Snapshot>, String>>>,
    retry_at: Mutex<Option<std::time::Instant>>,
    persistence: Arc<Mutex<()>>,
}

impl Catalogue {
    fn new(path: Option<PathBuf>, url: String) -> Self {
        let mut bundled = crate::models_dev::parse_catalogue(
            include_str!("fixtures/provider-catalogue.json").as_bytes(),
        )
        .expect("bundled catalogue must pass validation");
        // A bundle has no daemon TTL: refresh it in the background on first use.
        bundled.fetched_at_unix = 0;
        if let Some(cache) = path.as_ref().and_then(load_cache) {
            if cache.schema_version == SCHEMA
                && !cache.providers.is_empty()
                && cache.providers.len() <= 516
                && cache.entries.len() <= 60000
                && cache.revision == revision(&cache.providers, &cache.entries)
            {
                bundled = Snapshot {
                    revision: cache.revision,
                    fetched_at_unix: cache.fetched_at_unix,
                    providers: cache.providers,
                    entries: cache.entries,
                };
            } else if cache.schema_version < SCHEMA && cache.entries.len() <= 60000 {
                // Keep old offline context/prices/capabilities while refreshing.
                for (id, mut meta) in cache.entries {
                    // What an older cache could not know, the bundle still says.
                    if let (None, Some(known)) = (&meta.thinking_field, bundled.entries.get(&id)) {
                        meta.thinking_field = known.thinking_field.clone();
                    }
                    bundled.entries.insert(id, meta);
                }
                bundled.revision = revision(&bundled.providers, &bundled.entries);
            }
        }
        Self {
            current: RwLock::new(Arc::new(bundled)),
            path,
            url,
            flight: tokio::sync::Mutex::new(()),
            attempts: AtomicU64::new(0),
            last: Mutex::new(None),
            retry_at: Mutex::new(None),
            persistence: Arc::new(Mutex::new(())),
        }
    }

    async fn refresh(&self, explicit: bool) -> Result<Arc<Snapshot>, String> {
        let before = self.attempts.load(Ordering::Acquire);
        let _flight =
            tokio::time::timeout(FETCH_TIMEOUT + Duration::from_secs(2), self.flight.lock())
                .await
                .map_err(|_| "Catalogue refresh is busy. Try again shortly.".to_owned())?;
        if self.attempts.load(Ordering::Acquire) != before {
            return self
                .last
                .lock()
                .unwrap()
                .as_ref()
                .expect("completed attempt")
                .clone();
        }
        if !explicit
            && self
                .retry_at
                .lock()
                .unwrap()
                .is_some_and(|at| at > std::time::Instant::now())
        {
            return Ok(self.current.read().unwrap().clone());
        }
        let fetched = tokio::time::timeout(FETCH_TIMEOUT, fetch(&self.url))
            .await
            .map_err(|_| "Catalogue refresh timed out.".to_owned())
            .and_then(|result| result);
        let result = match fetched {
            Ok(mut fetched) => {
                fetched.fetched_at_unix = now();
                let published = Arc::new(fetched);
                *self.current.write().unwrap() = published.clone();
                if let Some(path) = &self.path {
                    let (path, data) = (path.clone(), published.clone());
                    let persistence = self.persistence.clone();
                    // A disk failure does not discard valid in-memory metadata.
                    let writer = tokio::task::spawn_blocking(move || {
                        if let Ok(_guard) = persistence.try_lock() {
                            save_cache(path, &data);
                        }
                    });
                    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
                }
                *self.retry_at.lock().unwrap() = None;
                Ok(published)
            }
            Err(error) => {
                *self.retry_at.lock().unwrap() = Some(std::time::Instant::now() + RETRY);
                Err(error)
            }
        };
        *self.last.lock().unwrap() = Some(result.clone());
        self.attempts.fetch_add(1, Ordering::Release);
        result
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Cache {
    #[serde(default)]
    pub schema_version: u32,
    pub fetched_at_unix: u64,
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub providers: Vec<ProviderRecord>,
    pub entries: BTreeMap<String, ModelMeta>,
}

impl Cache {
    #[cfg(test)]
    pub(crate) fn current_schema(&self) -> bool {
        self.schema_version == SCHEMA
    }

    pub(crate) fn discard_ambiguous_legacy_modalities(&mut self) {
        if self.schema_version < 2 {
            for meta in self.entries.values_mut() {
                if let Some(caps) = &mut meta.capabilities {
                    caps.modalities = None;
                }
            }
        }
    }
}

fn load_cache(path: &PathBuf) -> Option<Cache> {
    use std::io::Read;
    if !std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_BYTES as u64) {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_BYTES {
        return None;
    }
    let mut cache: Cache = serde_json::from_slice(&bytes).ok()?;
    cache.discard_ambiguous_legacy_modalities();
    Some(cache)
}

fn save_cache(path: PathBuf, data: &Snapshot) {
    use std::io::Write;
    #[derive(Serialize)]
    struct Saved<'a> {
        schema_version: u32,
        #[serde(flatten)]
        snapshot: &'a Snapshot,
    }
    struct Bounded<'a> {
        file: &'a mut std::fs::File,
        remaining: usize,
    }
    impl Write for Bounded<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Catalogue cache exceeds its byte limit.",
                ));
            }
            let written = self.file.write(bytes)?;
            self.remaining -= written;
            Ok(written)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let Some(parent) = path.parent() else {
        return;
    };
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(parent)?;
        // Cross-process cache publication never holds a lock during a fetch.
        // Contention skips this optional write rather than waiting on another daemon.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(parent.join("models_dev_cache.lock"))?;
        if lock.try_lock().is_err() {
            return Ok(());
        }
        if load_cache(&path).is_some_and(|old| old.fetched_at_unix > data.fetched_at_unix) {
            return Ok(());
        }
        let temp = parent.join(format!(
            ".models-dev-{}-{}.tmp",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        struct Temp(PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _remove = Temp(temp.clone());
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        // Serialize the shared publication by reference rather than duplicating
        // every model, and never publish a cache that its reader would refuse.
        serde_json::to_writer(
            Bounded {
                file: &mut file,
                remaining: MAX_BYTES,
            },
            &Saved {
                schema_version: SCHEMA,
                snapshot: data,
            },
        )?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(temp, &path)?;
        Ok(())
    })();
    if let Err(error) = result {
        tracing::warn!(%error, "could not publish catalogue disk cache");
    }
}

async fn fetch(url: &str) -> Result<Snapshot, String> {
    let client = reqwest::Client::builder()
        .user_agent(crate::transport::http::USER_AGENT)
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Could not create catalogue client.".to_owned())?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "Could not reach the model catalogue.".to_owned())?;
    if !response.status().is_success() {
        return Err(format!("Model catalogue returned {}.", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_BYTES as u64)
    {
        return Err("Model catalogue exceeds 16 MiB.".into());
    }
    let mut chunks = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| "Model catalogue download was interrupted.".to_owned())?;
        if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
            return Err("Model catalogue exceeds 16 MiB.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    tokio::task::spawn_blocking(move || crate::models_dev::parse_catalogue(&bytes))
        .await
        .map_err(|_| "Model catalogue parser failed.".to_owned())?
}

#[cfg(test)]
#[path = "catalogue_tests.rs"]
mod tests;
