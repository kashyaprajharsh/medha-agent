//! The one provider client: a saved profile's connection, keys and stand-ins.

use crate::protocol::{gemini_interactions, openai_chat};
use crate::transport::http;
use crate::{AuthKind, ProviderProfile, TokenCounter};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use kernel::{
    Block, CompiledContext, ImageInputMode, ImageSupport, InputTokenCount, ModelLimits,
    PreparedModelRequest, Protocol, Provider, ProviderCaps, ProviderError, ReasoningConfig,
    ReasoningEffort, ReasoningSupport, TokenAccountingMode, TokenCountError, TokenCountQuality,
    ToolCallStrategy,
};
use std::sync::Mutex;

pub struct ProviderClient {
    /// Active connection is mutable between turns so an interactive surface can
    /// switch a saved model profile without rebuilding the kernel. A stream
    /// snapshots it before its first await, so one request always uses exactly
    /// one endpoint/model/key tuple.
    connection: Mutex<Connection>,
    http: reqwest::Client,
    caps: ProviderCaps,
    reasoning: Mutex<ReasoningConfig>,
    streaming: std::sync::atomic::AtomicBool,
    /// What servers showed about their models, per protocol/endpoint/model.
    learned: crate::learned::Learned,
    stable_system: Mutex<Option<String>>,
    fallbacks: Mutex<Fallbacks>,
    /// Set once a server could not use the pattern rules in tool descriptions.
    plain_schemas: std::sync::atomic::AtomicBool,
    /// Looks up every key saved for a server, in the order they are tried.
    keys: Mutex<Option<KeySource>>,
    /// Connections that also trust a certificate file, by that file's path.
    trusting: Mutex<std::collections::HashMap<String, reqwest::Client>>,
}

/// Every key saved for the server at this address, first to last.
type KeySource = Box<dyn Fn(&str) -> Vec<String> + Send + Sync>;

#[derive(Clone)]
struct Connection {
    profile: ProviderProfile,
    credential: String,
}

/// Looks up the models that stand in, in the user's order, when one is needed.
type StandIns = Box<dyn Fn() -> Vec<(ProviderProfile, String)> + Send + Sync>;

/// How long a limited model is rested when its server does not say.
const REST: std::time::Duration = std::time::Duration::from_secs(60);

/// The models to try, in the user's order, when their own cannot be used.
#[derive(Default)]
struct Fallbacks {
    source: Option<StandIns>,
    /// While the user's own model is left alone after a limit.
    rested_until: Option<tokio::time::Instant>,
    listed: Vec<Connection>,
    next: usize,
    /// The user's own model and its reasoning setting, held while one stands in.
    own: Option<(Connection, ReasoningConfig)>,
}

impl Fallbacks {
    /// Read the list when the own model first fails this turn, not when the
    /// chat opened, leaving out the model that failed: it never stands in for itself.
    fn read(&mut self, client: &ProviderClient) {
        if self.own.is_some() || self.next != 0 {
            return;
        }
        let failed = client.connection.lock().unwrap().profile.clone();
        let listed = self.source.as_ref().map(|source| source());
        self.listed = listed
            .into_iter()
            .flatten()
            .filter(|(profile, _)| {
                profile.model != failed.model || profile.base_url != failed.base_url
            })
            .map(|(profile, credential)| Connection {
                profile,
                credential,
            })
            .collect();
    }
}

fn validate_reasoning(
    profile: &ProviderProfile,
    config: &ReasoningConfig,
) -> Result<(), ProviderError> {
    if config == &ReasoningConfig::default() {
        return Ok(());
    }
    if profile.reasoning == ReasoningSupport::Unsupported {
        return Err(ProviderError::Decode(
            "reasoning controls are disabled by this model profile".into(),
        ));
    }
    let effort = if config.enabled == Some(false) {
        Some(ReasoningEffort::None)
    } else {
        config.effort
    };
    if let Some(effort) = effort
        && !profile_reasoning_efforts(profile).contains(&effort)
    {
        return Err(ProviderError::Decode(format!(
            "reasoning effort '{}' is not supported by this protocol/model profile; available: {}",
            effort.as_str(),
            profile_reasoning_efforts(profile)
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    match profile.protocol {
        Protocol::OpenAiChat => {
            // Compatible servers use `reasoning_effort: "none"` to disable.
            if config.enabled == Some(true) && config.effort.is_none() {
                return Err(ProviderError::Decode(
                    "open-ai-chat cannot enable reasoning without an explicit effort".into(),
                ));
            }
        }
        Protocol::GeminiInteractions => {
            if config.enabled == Some(false) {
                return Err(ProviderError::Decode(
                    "gemini-interactions v1 has no portable thinking-disable control; use server default instead"
                        .into(),
                ));
            }
        }
        Protocol::OpenAiResponses | Protocol::AnthropicMessages => {}
    }
    Ok(())
}

fn profile_reasoning_efforts(profile: &ProviderProfile) -> Vec<ReasoningEffort> {
    if profile.reasoning == ReasoningSupport::Unsupported {
        return Vec::new();
    }
    let levels = profile.reasoning_efforts.as_deref().unwrap_or_else(|| {
        if profile.protocol == Protocol::GeminiInteractions {
            gemini_interactions::known_reasoning_efforts(&profile.model).unwrap_or(&[])
        } else {
            ReasoningEffort::ALL
        }
    });
    levels
        .iter()
        .copied()
        .filter(|effort| profile.reasoning_efforts.is_some() || *effort != ReasoningEffort::Ultra)
        .filter(|effort| {
            profile.protocol != Protocol::GeminiInteractions
                || matches!(
                    effort,
                    ReasoningEffort::Minimal
                        | ReasoningEffort::Low
                        | ReasoningEffort::Medium
                        | ReasoningEffort::High
                )
        })
        .collect()
}

/// Neutralize a carried reasoning setting that the target profile can't accept,
/// so a model switch never hard-fails on a protocol/support mismatch. Returns
/// the default ("untouched" — let the server decide) in those cases; otherwise
/// keeps the setting unchanged. An explicit `set_reasoning` stays strict.
fn reconcile_reasoning_for_switch(
    profile: &ProviderProfile,
    config: ReasoningConfig,
) -> ReasoningConfig {
    if validate_reasoning(profile, &config).is_err() {
        ReasoningConfig::default()
    } else {
        config
    }
}

fn protocol_is_implemented(protocol: Protocol) -> bool {
    matches!(
        protocol,
        Protocol::OpenAiChat | Protocol::GeminiInteractions
    )
}

impl ProviderClient {
    /// Construct the inert provider used while the interactive first-run model
    /// setup is open. This is deliberately separate from [`Self::from_profile`]:
    /// persisted and environment-derived profiles must always pass validation.
    pub fn unconfigured() -> Self {
        Self::with_connection(Connection {
            profile: ProviderProfile::openai_chat("", "", AuthKind::None),
            credential: String::new(),
        })
    }

    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into();
        let api_key = api_key.into();
        let model = model.into();
        let auth = if api_key.trim().is_empty() {
            AuthKind::None
        } else {
            AuthKind::Bearer
        };
        Self::with_connection(Connection {
            profile: ProviderProfile::openai_chat(base_url, model, auth),
            credential: api_key,
        })
    }

    fn with_connection(connection: Connection) -> Self {
        let max_ctx = connection.profile.max_ctx;
        let images = Self::profile_image_support(&connection.profile);
        Self {
            connection: Mutex::new(connection),
            http: http::client(),
            caps: ProviderCaps {
                images,
                caching: false,
                // Unknown until discovered/configured; a fabricated value would
                // mislead the context compiler.
                max_ctx,
                // Initial selection only, not an asserted capability. Runtime
                // fallback handles endpoints that lack native calls.
                tool_calls: ToolCallStrategy::Native,
            },
            reasoning: Mutex::new(ReasoningConfig::default()),
            streaming: std::sync::atomic::AtomicBool::new(true),
            learned: crate::learned::Learned::default(),
            stable_system: Mutex::new(None),
            fallbacks: Mutex::default(),
            plain_schemas: std::sync::atomic::AtomicBool::new(false),
            keys: Mutex::new(None),
            trusting: Mutex::default(),
        }
    }

    /// Where to look up the keys saved for a server. With more than one, a key
    /// that fails rests and the next takes over. Asked each time, so a key
    /// added while a chat is open is used by it.
    pub fn with_keys(self, source: impl Fn(&str) -> Vec<String> + Send + Sync + 'static) -> Self {
        *self.keys.lock().unwrap() = Some(Box::new(source));
        self
    }

    /// The connection to use for a server: the shared one, or one that also
    /// trusts the certificate file its model was saved with. Built once per file.
    fn http_for(&self, profile: &ProviderProfile) -> Result<reqwest::Client, ProviderError> {
        if profile.ca_cert_file.is_none() && profile.proxy.is_none() {
            return Ok(self.http.clone());
        }
        let key = serde_json::to_string(&(&profile.ca_cert_file, &profile.proxy))
            .expect("transport settings");
        let mut trusting = self.trusting.lock().unwrap();
        if let Some(known) = trusting.get(&key) {
            return Ok(known.clone());
        }
        let built = http::client_for(profile)?;
        if trusting.len() >= 16 {
            trusting.clear();
        }
        trusting.insert(key, built.clone());
        Ok(built)
    }

    /// The keys saved for the server in use now, in the order they are tried.
    fn saved_keys(&self) -> Vec<String> {
        let server = self.connection.lock().unwrap().profile.base_url.clone();
        let keys = self.keys.lock().unwrap();
        keys.as_ref()
            .map_or_else(Vec::new, |source| source(&server))
    }

    /// Where to look up the models that stand in, in order, when this one cannot
    /// be used. It is asked each time, so a list changed since is the one used.
    pub fn with_fallbacks(
        self,
        source: impl Fn() -> Vec<(ProviderProfile, String)> + Send + Sync + 'static,
    ) -> Self {
        self.fallbacks.lock().unwrap().source = Some(Box::new(source));
        self
    }

    /// The start of the system prompt that every chat shares; a caching server
    /// can then keep it apart from the part that changes per chat or per day.
    pub fn stable_system_start(&self, start: &str) {
        *self.stable_system.lock().unwrap() = Some(start.to_owned());
    }

    /// Keep what servers show about their models in `path`, and start from
    /// what earlier sessions learned there.
    pub fn remembering(mut self, path: std::path::PathBuf) -> Self {
        self.learned = crate::learned::Learned::at(path);
        self
    }

    fn image_identity(profile: &ProviderProfile) -> String {
        format!(
            "{}:{}:{}",
            profile.protocol.as_str(),
            profile.base_url,
            profile.model
        )
    }

    /// What the profile's resolved metadata says about image input. Missing
    /// metadata stays `Unknown` so a custom endpoint is attempted rather than
    /// diverted to a described substitute it never needed.
    fn profile_image_support(profile: &ProviderProfile) -> ImageSupport {
        if !profile.protocol.carries_images() {
            return ImageSupport::Unsupported;
        }
        match profile.image_input {
            ImageInputMode::Native => return ImageSupport::Supported,
            ImageInputMode::Text => return ImageSupport::Unsupported,
            ImageInputMode::Auto => {}
        }
        match profile
            .capabilities
            .as_ref()
            .map(|capabilities| capabilities.user_image_state())
        {
            Some(crate::CapabilityState::Supported) => ImageSupport::Supported,
            Some(crate::CapabilityState::Unsupported) => ImageSupport::Unsupported,
            _ => ImageSupport::Unknown,
        }
    }

    /// Construct the runtime client from an explicit deployment profile while
    /// keeping its credential outside the serializable profile value.
    pub fn from_profile(
        profile: ProviderProfile,
        credential: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        profile.validate().map_err(ProviderError::Decode)?;
        if !protocol_is_implemented(profile.protocol) {
            return Err(ProviderError::Decode(format!(
                "ProviderClient does not implement {}",
                profile.protocol.as_str()
            )));
        }
        let credential = credential.into();
        kernel::redact::remember(&credential);
        if profile.auth.requires_credential() && credential.trim().is_empty() {
            return Err(ProviderError::Decode(format!(
                "{} authentication requires a credential",
                profile.protocol.as_str()
            )));
        }
        Ok(Self::with_connection(Connection {
            profile,
            credential,
        }))
    }

    /// Set the known context window (from discovery, config, or `medha.lock`).
    pub fn with_max_ctx(mut self, tokens: u32) -> Self {
        self.caps.max_ctx = Some(tokens);
        self.connection.lock().unwrap().profile.max_ctx = Some(tokens);
        self
    }

    /// Set the initial reasoning config (from `medha.lock`), before wrapping
    /// in `Arc`. Equivalent to `set_reasoning` but chainable at construction.
    pub fn with_reasoning(self, config: ReasoningConfig) -> Result<Self, ProviderError> {
        self.set_reasoning(config)?;
        Ok(self)
    }

    /// Switch between turns; a running stream keeps the connection it snapshotted.
    pub fn switch_provider_profile(
        &self,
        profile: ProviderProfile,
        credential: impl Into<String>,
    ) -> Result<(), ProviderError> {
        profile.validate().map_err(ProviderError::Decode)?;
        if !protocol_is_implemented(profile.protocol) {
            return Err(ProviderError::Decode(format!(
                "ProviderClient does not implement {}",
                profile.protocol.as_str()
            )));
        }
        let credential = credential.into();
        if profile.auth.requires_credential() && credential.trim().is_empty() {
            return Err(ProviderError::Decode(format!(
                "{} authentication requires a credential",
                profile.protocol.as_str()
            )));
        }
        // A model switch carries the session's reasoning setting into the new
        // profile. When that carried setting is meaningless for the target
        // protocol (e.g. Gemini's "on, no explicit effort" reaching open-ai-chat,
        // which needs a concrete level), fall back to "untouched" — the server's
        // own default — rather than failing the switch. We never fabricate an
        // effort, so cost/latency semantics are never silently changed.
        let reasoning = reconcile_reasoning_for_switch(&profile, self.reasoning());
        validate_reasoning(&profile, &reasoning)?;
        *self.reasoning.lock().unwrap() = reasoning;
        *self.connection.lock().unwrap() = Connection {
            profile,
            credential,
        };
        Ok(())
    }

    pub fn active_model(&self) -> String {
        self.connection.lock().unwrap().profile.model.clone()
    }

    /// Portable OpenAI Chat reasoning control. Template kwargs are deliberately
    /// absent: model-template extensions belong to an explicit custom profile,
    /// never the generic compatible route.
    fn reasoning_effort_value(&self) -> Option<&'static str> {
        if self.reasoning_support() == ReasoningSupport::Unsupported {
            return None;
        }
        let cfg = self.reasoning.lock().unwrap();
        if cfg.enabled == Some(false) {
            return Some("none"); // explicit disable for compatible servers
        }
        cfg.effort.map(ReasoningEffort::as_str)
    }

    async fn stream_gemini_prepared(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        let names = wire_name_map(
            &request
                .context
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>(),
        );
        let connection = self.connection.lock().unwrap().clone();
        if connection.profile.protocol != request.protocol {
            return Err(ProviderError::Decode(format!(
                "prepared {} request cannot be sent through the active {} profile",
                request.protocol.as_str(),
                connection.profile.protocol.as_str()
            )));
        }
        let streaming = request
            .body
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let url = format!(
            "{}/interactions",
            connection.profile.base_url.trim_end_matches('/')
        );
        http::debug_json_request("POST", &url, &request.body);

        let mut post = self.http_for(&connection.profile)?.post(&url);
        for (name, value) in &request.headers {
            post = post.header(name, value);
        }
        let outgoing = http::with_profile(post, &connection.profile, &connection.credential)?
            .json(&request.body);
        let resp = http::send_model_request(outgoing).await?;
        let resp = http::require_success(resp).await?;

        if !streaming {
            let body = http::response_text(resp).await?;
            let blocks = gemini_interactions::parse_interaction(&body, &names)?;
            return Ok(futures::stream::iter(blocks.into_iter().map(Ok)).boxed());
        }

        let byte_stream = resp.bytes_stream();
        let idle = http::stream_idle(&connection.profile);
        let stream = async_stream::stream! {
            let mut sse = crate::transport::sse::SseDecoder::default();
            let mut decoder = gemini_interactions::ResponseDecoder::new(names.clone());
            // Dropped as soon as anything decodes; see the chat path for why
            // this is a buffer rather than a content-type check.
            let mut unread = Some(Vec::new());

            futures::pin_mut!(byte_stream);
            let mut progress_deadline = tokio::time::Instant::now() + idle;
            'chunks: loop {
                let next = tokio::time::timeout_at(progress_deadline, byte_stream.next()).await;
                let chunk = match next {
                    Err(_) if decoder.completed() => break,
                    Err(_) => {
                        yield Err(http::stalled_stream(idle));
                        return;
                    }
                    Ok(None) => break,
                    Ok(Some(chunk)) => chunk,
                };
                match chunk {
                    Err(error) => {
                        yield Err(http::stream_read_failed(error));
                        return;
                    }
                    Ok(bytes) => {
                        if let Some(buffer) = unread.as_mut() {
                            if buffer.len() + bytes.len() <= http::MAX_RESPONSE_BODY_BYTES {
                                buffer.extend_from_slice(&bytes);
                            } else {
                                unread = None;
                            }
                        }
                        let events = match sse.push(&bytes) {
                            Ok(events) => events,
                            Err(error) => { yield Err(error); return; }
                        };
                        for event in events {
                            if event.is_delivery() {
                                progress_deadline = tokio::time::Instant::now() + idle;
                            }
                            match decoder.push(&event) {
                                Ok(blocks) => {
                                    for block in blocks {
                                        unread = None;
                                        yield Ok(block);
                                    }
                                }
                                Err(error) => {
                                    yield Err(error);
                                    return;
                                }
                            }
                            if decoder.completed() { break 'chunks; }
                        }
                    }
                }
            }

            if let Some(event) = sse.finish() {
                match decoder.push(&event) {
                    Ok(blocks) => {
                        for block in blocks {
                            unread = None;
                            yield Ok(block);
                        }
                    }
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                }
            }

            if let Some(buffer) = unread.filter(|buffer| read_whole_body(buffer)) {
                let body = String::from_utf8_lossy(&buffer);
                match gemini_interactions::parse_interaction(&body, &names) {
                    Ok(blocks) => for block in blocks { yield Ok(block); },
                    Err(error) => yield Err(error),
                }
                return;
            }
            if let Err(error) = decoder.finish() { yield Err(error); }
        };
        Ok(stream.boxed())
    }

    async fn count_vllm(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<InputTokenCount, TokenCountError> {
        let connection = self.connection.lock().unwrap().clone();
        let url = vllm_tokenize_url(&connection.profile.base_url);
        let body = vllm_tokenize_body_from_prepared(request);
        let client = self
            .http_for(&connection.profile)
            .map_err(|error| TokenCountError::Transport(error.to_string()))?;
        let http_request = http::with_profile(
            client.post(&url),
            &connection.profile,
            &connection.credential,
        )
        .map_err(|error| TokenCountError::Decode(error.to_string()))?;
        let resp = http_request
            .json(&body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|error| TokenCountError::Transport(error.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = http::read_error_body(resp).await;
            return Err(TokenCountError::Status(status, body));
        }
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|error| TokenCountError::Decode(error.to_string()))?;
        let tokens = value
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| TokenCountError::Decode("missing integer `count`".into()))?;
        Ok(InputTokenCount {
            tokens,
            quality: TokenCountQuality::Authoritative,
            request_fingerprint: request.request_fingerprint.clone(),
        })
    }
}

/// vLLM's `/tokenize` lives at the server root, a sibling of `/v1` — not under
/// it. Derive the root from the (usually `…/v1`) chat base URL.
fn vllm_tokenize_url(base_url: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(base_url) else {
        return format!("{}/tokenize", base_url.trim_end_matches('/'));
    };
    let path = url.path().trim_end_matches('/');
    let api_root = path.strip_suffix("/chat/completions").unwrap_or(path);
    let server_root = api_root.strip_suffix("/v1").unwrap_or(api_root);
    url.set_path(&format!("{server_root}/tokenize"));
    url.to_string()
}

/// vLLM `/tokenize` body: messages rendered through the model's chat template.
/// Keeps every role (system/tool/assistant) and mirrors the same system-hoist
/// as `build_chat_messages`, so the count matches what the chat endpoint would
/// actually send — and a strict template can't reject a mid-array `system`.
fn vllm_tokenize_body_from_prepared(request: &PreparedModelRequest) -> serde_json::Value {
    openai_chat::vllm_tokenize_body(request)
}

#[async_trait]
impl Provider for ProviderClient {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    fn context_window(&self) -> Option<u32> {
        self.connection.lock().unwrap().profile.max_ctx
    }

    fn image_support(&self) -> kernel::ImageSupport {
        let profile = self.connection.lock().unwrap().profile.clone();
        let declared = Self::profile_image_support(&profile);
        // The operator's setting and the protocol's own limit are final.
        if profile.image_input != ImageInputMode::Auto || !profile.protocol.carries_images() {
            return declared;
        }
        // What this server showed outranks what a catalogue says of the model:
        // a vision model can be served with images turned off.
        self.learned
            .images(&Self::image_identity(&profile))
            .unwrap_or(declared)
    }

    fn image_input_mode(&self) -> ImageInputMode {
        self.connection.lock().unwrap().profile.image_input
    }

    fn observe_image_support(&self, support: ImageSupport) {
        let profile = self.connection.lock().unwrap().profile.clone();
        if profile.image_input != ImageInputMode::Auto {
            return;
        }
        self.learned
            .set_images(&Self::image_identity(&profile), support);
    }

    fn context_identity(&self) -> String {
        let connection = self.connection.lock().unwrap();
        format!(
            "{}:{}:{}",
            connection.profile.protocol.as_str(),
            connection.profile.base_url,
            connection.profile.model
        )
    }

    fn protocol(&self) -> Protocol {
        self.connection.lock().unwrap().profile.protocol
    }

    fn model_limits(&self) -> ModelLimits {
        // Avoid recursively locking `connection`.
        let connection = self.connection.lock().unwrap();
        ModelLimits {
            max_input_tokens: None,
            // The profile stores a per-request allowance, not evidence of the
            // model's physical output ceiling. requested_output_tokens carries
            // that policy independently, including for chat preflight.
            max_output_tokens: None,
            max_combined_tokens: connection.profile.max_ctx.map(u64::from),
        }
    }

    fn requested_output_tokens(&self) -> Option<u64> {
        self.connection.lock().unwrap().profile.max_output_tokens
    }

    fn update_context_limit(&self, tokens: u64) {
        if let Ok(tokens) = u32::try_from(tokens) {
            self.connection.lock().unwrap().profile.max_ctx = Some(tokens);
        }
    }

    fn token_accounting_mode(&self) -> TokenAccountingMode {
        self.connection.lock().unwrap().profile.token_accounting
    }

    fn reasoning_support(&self) -> ReasoningSupport {
        self.connection.lock().unwrap().profile.reasoning
    }

    fn reasoning_efforts(&self) -> Vec<ReasoningEffort> {
        profile_reasoning_efforts(&self.connection.lock().unwrap().profile)
    }

    fn set_reasoning(&self, config: ReasoningConfig) -> Result<(), ProviderError> {
        let connection = self.connection.lock().unwrap().clone();
        validate_reasoning(&connection.profile, &config)?;
        *self.reasoning.lock().unwrap() = config;
        Ok(())
    }

    fn reasoning(&self) -> ReasoningConfig {
        self.reasoning.lock().unwrap().clone()
    }

    fn set_streaming(&self, on: bool) {
        self.streaming
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    fn streaming(&self) -> bool {
        self.streaming.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn fall_back(&self, failed: &str, rest: kernel::recovery::Rest) -> Option<(String, String)> {
        let mut fallbacks = self.fallbacks.lock().unwrap();
        let was = self.active_model();
        // Another caller already left the model that failed: the one standing
        // in has not failed, and is not passed over for the next.
        if fallbacks.own.is_some() && !failed.is_empty() && failed != was {
            return Some((failed.to_owned(), was));
        }
        fallbacks.read(self);
        // One that cannot be switched to is passed over for the next.
        while let Some(next) = fallbacks.listed.get(fallbacks.next).cloned() {
            fallbacks.next += 1;
            let before = (self.connection.lock().unwrap().clone(), self.reasoning());
            if self
                .switch_provider_profile(next.profile, next.credential)
                .is_ok()
            {
                // Only the user's own model is rested: a stand-in that fails is passed.
                if let (None, kernel::recovery::Rest::Limited(wait)) = (&fallbacks.own, rest) {
                    let until = tokio::time::Instant::now() + wait.unwrap_or(REST);
                    fallbacks.rested_until = Some(until);
                }
                fallbacks.own.get_or_insert(before);
                return Some((was, self.active_model()));
            }
        }
        None
    }

    fn with_plain_schemas(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<Option<PreparedModelRequest>, ProviderError> {
        let mut body = request.body.clone();
        if crate::schema_rules::strip(&mut body) == 0 {
            return Ok(None);
        }
        let stays = std::sync::atomic::Ordering::Relaxed;
        self.plain_schemas.store(true, stays);
        Ok(Some(request.with_body(body)))
    }

    fn replays_reasoning(&self) -> bool {
        let profile = self.connection.lock().unwrap().profile.clone();
        profile.protocol == Protocol::OpenAiChat && crate::reasoning_echo::expected(&profile)
    }

    fn standing_in(&self) -> Option<(String, String)> {
        let fallbacks = self.fallbacks.lock().unwrap();
        let (own, _) = fallbacks.own.as_ref()?;
        Some((own.profile.model.clone(), self.active_model()))
    }

    fn list_price(&self) -> Option<kernel::Pricing> {
        let profile = self.connection.lock().unwrap().profile.clone();
        let (input, output, cached, written) = crate::models_dev::list_price(&profile)?;
        Some(kernel::Pricing {
            input_per_mtok: input,
            output_per_mtok: output,
            cached_input_per_mtok: cached,
            cache_write_per_mtok: written,
            indicative: true,
        })
    }

    fn stand_in_left(&self) -> bool {
        let mut fallbacks = self.fallbacks.lock().unwrap();
        fallbacks.read(self);
        fallbacks.next < fallbacks.listed.len()
    }

    fn restore_primary(&self) -> Option<(String, String)> {
        let mut fallbacks = self.fallbacks.lock().unwrap();
        let resting = fallbacks
            .rested_until
            .is_some_and(|until| tokio::time::Instant::now() < until);
        if let (true, Some((own, _))) = (resting, &fallbacks.own) {
            return Some((own.profile.model.clone(), self.active_model()));
        }
        fallbacks.next = 0;
        fallbacks.rested_until = None;
        if let Some((own, reasoning)) = fallbacks.own.take() {
            *self.reasoning.lock().unwrap() = reasoning;
            *self.connection.lock().unwrap() = own;
        }
        drop(fallbacks);
        // Each message starts from the first saved key that is not resting.
        let keys = self.saved_keys();
        if let (true, Some((_, key))) = (keys.len() > 1, crate::key_rotation::first_ready(&keys)) {
            self.connection.lock().unwrap().credential = key.clone();
        }
        None
    }

    fn another_key(&self) -> bool {
        if self.fallbacks.lock().unwrap().own.is_some() {
            return false;
        }
        let current = self.connection.lock().unwrap().credential.clone();
        let keys = self.saved_keys();
        keys.iter()
            .any(|key| *key != current && !crate::key_rotation::resting(key))
    }

    fn next_key(&self, status: u16, asked: Option<std::time::Duration>) -> Option<(usize, usize)> {
        // Only the chat's own model has keys of its own to turn to.
        if self.fallbacks.lock().unwrap().own.is_some() {
            return None;
        }
        let keys = self.saved_keys();
        if keys.len() < 2 {
            return None;
        }
        let current = self.connection.lock().unwrap().credential.clone();
        crate::key_rotation::rest_key(&current, crate::key_rotation::rest(status, asked));
        let (at, key) = crate::key_rotation::first_ready(&keys)?;
        self.connection.lock().unwrap().credential = key.clone();
        Some((at, keys.len()))
    }

    fn prepare_request(
        &self,
        ctx: &CompiledContext,
    ) -> Result<PreparedModelRequest, ProviderError> {
        self.prepare_request_in(ctx, &kernel::RequestScope::default())
    }

    fn prepare_request_in(
        &self,
        ctx: &CompiledContext,
        scope: &kernel::RequestScope,
    ) -> Result<PreparedModelRequest, ProviderError> {
        let connection = self.connection.lock().unwrap().clone();
        let model = if ctx.model.is_empty() {
            connection.profile.model.clone()
        } else {
            ctx.model.clone()
        };
        if ctx.ordered_messages().iter().any(|message| {
            message
                .parts
                .iter()
                .any(|part| matches!(part, kernel::ContentPart::Media(_)))
        }) && model == connection.profile.model
            && connection.profile.image_input != ImageInputMode::Native
            && connection
                .profile
                .capabilities
                .as_ref()
                .is_some_and(|caps| {
                    caps.input_state("image") == crate::models_dev::CapabilityState::Unsupported
                })
        {
            return Err(ProviderError::Decode(
                "selected model declares image input unsupported; select a vision-capable model"
                    .into(),
            ));
        }
        let streaming = self.streaming();
        let mut headers = Vec::new();
        let body = match connection.profile.protocol {
            Protocol::OpenAiChat => {
                let mut body = openai_chat::prepare_body(
                    ctx,
                    &model,
                    streaming,
                    self.reasoning_effort_value(),
                    connection.profile.max_output_tokens,
                    crate::reasoning_echo::expected(&connection.profile),
                )?;
                if connection.profile.chat_token_limit_field() == "max_completion_tokens"
                    && let Some(cap) = body.as_object_mut().and_then(|o| o.remove("max_tokens"))
                {
                    body["max_completion_tokens"] = cap;
                }
                if self
                    .plain_schemas
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    crate::schema_rules::strip(&mut body);
                }
                // Last, so the marks sit on the request exactly as it is sent.
                headers = crate::prompt_cache::apply(
                    &mut body,
                    crate::prompt_cache::route(&connection.profile, &model),
                    connection.profile.cache_ttl,
                    scope,
                    self.stable_system.lock().unwrap().as_deref(),
                );
                body
            }
            Protocol::GeminiInteractions => {
                gemini_interactions::prepare_body(
                    ctx,
                    &model,
                    streaming,
                    &self.reasoning(),
                    connection.profile.max_output_tokens,
                )?
                .0
            }
            protocol => {
                return Err(ProviderError::Decode(format!(
                    "ProviderClient does not implement {}",
                    protocol.as_str()
                )));
            }
        };
        let mut prepared =
            PreparedModelRequest::new(connection.profile.protocol, model, body, ctx.clone());
        prepared.headers = headers;
        Ok(prepared)
    }

    async fn count_input_tokens(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<Option<InputTokenCount>, TokenCountError> {
        let counter = self.connection.lock().unwrap().profile.token_counter;
        match counter {
            TokenCounter::None => Ok(None),
            TokenCounter::Vllm => self.count_vllm(request).await.map(Some),
        }
    }

    fn with_output_limit(
        &self,
        request: &PreparedModelRequest,
        max_output_tokens: u64,
    ) -> Result<Option<PreparedModelRequest>, ProviderError> {
        let mut body = request.body.clone();
        let object = body.as_object_mut().ok_or_else(|| {
            ProviderError::Decode("prepared model request is not an object".into())
        })?;
        match request.protocol {
            Protocol::OpenAiChat => {
                let field = if object.contains_key("max_completion_tokens") {
                    "max_completion_tokens"
                } else if object.contains_key("max_tokens") {
                    "max_tokens"
                } else {
                    self.connection
                        .lock()
                        .unwrap()
                        .profile
                        .chat_token_limit_field()
                };
                object.insert(field.into(), serde_json::json!(max_output_tokens));
            }
            Protocol::GeminiInteractions => {
                let generation_config = object
                    .entry("generation_config")
                    .or_insert_with(|| serde_json::json!({}));
                let generation_config = generation_config.as_object_mut().ok_or_else(|| {
                    ProviderError::Decode(
                        "prepared Gemini generation_config is not an object".into(),
                    )
                })?;
                generation_config.insert(
                    "max_output_tokens".into(),
                    serde_json::json!(max_output_tokens),
                );
            }
            Protocol::OpenAiResponses => {
                object.insert(
                    "max_output_tokens".into(),
                    serde_json::json!(max_output_tokens),
                );
            }
            Protocol::AnthropicMessages => {
                object.insert("max_tokens".into(), serde_json::json!(max_output_tokens));
            }
        }
        Ok(Some(request.with_body(body)))
    }

    fn with_request_reasoning(
        &self,
        request: &PreparedModelRequest,
        config: &ReasoningConfig,
    ) -> Result<Option<PreparedModelRequest>, ProviderError> {
        let connection = self.connection.lock().unwrap();
        if request.protocol != connection.profile.protocol {
            return Ok(None);
        }
        validate_reasoning(&connection.profile, config)?;
        let mut body = request.body.clone();
        match request.protocol {
            Protocol::OpenAiChat => {
                let object = body.as_object_mut().ok_or_else(|| {
                    ProviderError::Decode("prepared model request is not an object".into())
                })?;
                let effort = if config.enabled == Some(false) {
                    Some(ReasoningEffort::None)
                } else {
                    config.effort
                };
                match effort {
                    Some(effort) => {
                        object.insert(
                            "reasoning_effort".into(),
                            serde_json::json!(effort.as_str()),
                        );
                    }
                    None => {
                        object.remove("reasoning_effort");
                    }
                }
            }
            Protocol::GeminiInteractions => {
                gemini_interactions::apply_reasoning(&mut body, config)?
            }
            Protocol::OpenAiResponses | Protocol::AnthropicMessages => return Ok(None),
        }
        Ok(Some(request.with_body(body)))
    }

    async fn stream(
        &self,
        ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        let request = self.prepare_request(ctx)?;
        self.stream_prepared(&request).await
    }

    async fn stream_prepared(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        if request.protocol == Protocol::GeminiInteractions {
            return self.stream_gemini_prepared(request).await;
        }
        if request.protocol != Protocol::OpenAiChat {
            return Err(ProviderError::Decode(format!(
                "ProviderClient cannot send {}",
                request.protocol.as_str()
            )));
        }
        let names = wire_name_map(
            &request
                .context
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>(),
        );
        let connection = self.connection.lock().unwrap().clone();
        if connection.profile.protocol != request.protocol {
            return Err(ProviderError::Decode(format!(
                "prepared {} request cannot be sent through the active {} profile",
                request.protocol.as_str(),
                connection.profile.protocol.as_str()
            )));
        }
        let streaming = request
            .body
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let url = openai_chat::endpoint_url(&connection.profile.base_url, "/chat/completions")?;

        // Direct proof of what actually goes over the wire — no guessing, no
        // separate curl experiment needed. Set MEDHA_DEBUG_HTTP=1 to see the
        // exact prepared JSON for every call.
        http::debug_json_request("POST", &url, &request.body);

        let mut post = self.http_for(&connection.profile)?.post(&url);
        for (name, value) in &request.headers {
            post = post.header(name, value);
        }
        let outgoing = http::with_profile(post, &connection.profile, &connection.credential)?
            .json(&request.body);
        let resp = http::send_model_request(outgoing).await?;
        let resp = http::require_success(resp).await?;

        // Non-streaming: one blocking body, parsed and yielded as a single batch
        // of blocks (reasoning → text → tool intents → usage). The kernel loop
        // is identical; only the arrival shape differs.
        if !streaming {
            let body = http::response_text(resp).await?;
            let blocks = openai_chat::parse_completion(&body, &names)?;
            return Ok(futures::stream::iter(blocks.into_iter().map(Ok)).boxed());
        }

        let byte_stream = resp.bytes_stream();
        let idle = http::stream_idle(&connection.profile);
        let s = async_stream::stream! {
            let mut sse = crate::transport::sse::SseDecoder::default();
            let mut decoder = openai_chat::ResponseDecoder::new(names.clone());
            // Held only until the first block is produced. An endpoint that
            // ignored `stream` and answered with one completion decodes to
            // nothing as an event stream, and this is what it is read from.
            // Deciding by content type instead would make every unlabelled
            // stream wait for its last byte before showing its first word.
            let mut unread = Some(Vec::new());

            futures::pin_mut!(byte_stream);
            let mut progress_deadline = tokio::time::Instant::now() + idle;
            'chunks: loop {
                let next = tokio::time::timeout_at(progress_deadline, byte_stream.next()).await;
                let chunk = match next {
                    Err(_) if decoder.completed() => break,
                    Err(_) => {
                        if let Some(b) = decoder.flush_pending() {
                            yield Ok(b);
                        }
                        yield Err(http::stalled_stream(idle));
                        return;
                    }
                    Ok(None) => break,
                    Ok(Some(chunk)) => chunk,
                };
                match chunk {
                    Err(e) => {
                        // Surface what the filter is still holding before the
                        // error — otherwise the reply's tail silently vanishes.
                        if let Some(b) = decoder.flush_pending() {
                            yield Ok(b);
                        }
                        yield Err(http::stream_read_failed(e));
                        return;
                    }
                    Ok(bytes) => {
                        if let Some(buffer) = unread.as_mut() {
                            if buffer.len() + bytes.len() <= http::MAX_RESPONSE_BODY_BYTES {
                                buffer.extend_from_slice(&bytes);
                            } else {
                                unread = None;
                            }
                        }
                        let events = match sse.push(&bytes) {
                            Ok(events) => events,
                            Err(error) => { yield Err(error); return; }
                        };
                        for event in events {
                            if event.is_delivery() {
                                progress_deadline = tokio::time::Instant::now() + idle;
                            }
                            match decoder.push(&event) {
                                Ok(blocks) => {
                                    for b in blocks { unread = None; yield Ok(b); }
                                },
                                Err(e) => { yield Err(e); return; }
                            }
                            if event.data.trim() == "[DONE]" { break 'chunks; }
                        }
                    },
                }
            }

            // Preserve tolerant handling for servers that close without the
            // final SSE blank line.
            if let Some(event) = sse.finish() {
                match decoder.push(&event) {
                    Ok(blocks) => for b in blocks { unread = None; yield Ok(b); },
                    Err(e) => { yield Err(e); return; }
                }
            }

            if let Some(buffer) = unread.filter(|buffer| read_whole_body(buffer)) {
                let body = String::from_utf8_lossy(&buffer);
                match openai_chat::parse_completion(&body, &names) {
                    Ok(blocks) => for block in blocks { yield Ok(block); },
                    Err(error) => yield Err(error),
                }
                return;
            }
            match decoder.finish() {
                Ok(blocks) => for block in blocks { yield Ok(block); },
                Err(error) => yield Err(error),
            }
        };

        Ok(s.boxed())
    }
}

/// Read a non-stream reply whole so its parse error is reported.
fn read_whole_body(buffer: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buffer);
    match text.trim_start().chars().next() {
        Some('{') => true,
        Some(_) => !text
            .lines()
            .any(|line| line.starts_with("data:") || line.starts_with("event:")),
        None => false,
    }
}

/// Wire→canonical map for the tool names exposed this request. Collisions get a
/// trailing `_` so two canonical names can never share a wire name.
fn wire_name_map(canonical: &[String]) -> std::collections::HashMap<String, String> {
    openai_chat::wire_name_map(canonical)
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
