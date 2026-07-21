//! The baseline, open-first adapter: the OpenAI-compatible Chat Completions
//! API. Point `base_url` at any compatible server (local or hosted) and it
//! works with zero new code. Translates to/from the canonical `Block` so the
//! kernel stays vendor-neutral (§4.4).

use crate::protocol::{gemini_interactions, openai_chat};
use crate::transport::http;
use crate::{AuthKind, ProviderProfile};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use kernel::{
    Block, CompiledContext, InputTokenCount, ModelLimits, PreparedModelRequest, Protocol, Provider,
    ProviderCaps, ProviderError, ReasoningConfig, ReasoningEffort, ReasoningSupport,
    TokenAccountingMode, TokenCountError, TokenCountQuality, ToolCallStrategy,
};
#[cfg(test)]
use kernel::{Message, Role};
use serde::Deserialize;
use std::sync::Mutex;

/// Compatibility export retained for existing configuration code.
pub use crate::profile::TokenCounter as OpenAiTokenCounter;

pub struct ProviderClient {
    /// Active connection is mutable between turns so an interactive surface can
    /// switch a saved model profile without rebuilding the kernel. A stream
    /// snapshots it before its first await, so one request always uses exactly
    /// one endpoint/model/key tuple.
    connection: Mutex<Connection>,
    http: reqwest::Client,
    caps: ProviderCaps,
    /// Runtime-mutable (config at startup, `/think` slash command live) —
    /// interior mutability since `Provider` methods take `&self` (shared
    /// behind `Arc`, §4.4).
    reasoning: Mutex<ReasoningConfig>,
    /// SSE streaming on/off (`/stream` slash command). Off → one blocking
    /// request, whole response yielded at once. Same interior-mutability reason
    /// as `reasoning`.
    streaming: std::sync::atomic::AtomicBool,
}

/// Compatibility name retained while callers migrate to the protocol/profile
/// client introduced by the provider refactor.
pub type OpenAiCompat = ProviderClient;

#[derive(Clone)]
struct Connection {
    profile: ProviderProfile,
    credential: String,
}

/// Normalise a reasoning request before validation/storage: an explicit
/// "reasoning on" with no chosen effort defaults to `Minimal`. This matches
/// Gemini's own guidance (its latest models can't disable thinking; `minimal`
/// is the lowest level, mapping to `thinking_level: minimal`) and — critically —
/// makes the canonical config portable. `open-ai-chat` requires an explicit
/// effort whenever reasoning is on, so carrying a concrete level lets a
/// Gemini → openai-chat model switch succeed instead of failing validation.
/// `Auto` (`enabled: None`) is left untouched: it still omits controls entirely
/// so the server/model default applies.
fn normalize_reasoning(mut config: ReasoningConfig) -> ReasoningConfig {
    if config.enabled == Some(true) && config.effort.is_none() {
        config.effort = Some(ReasoningEffort::Minimal);
    }
    config
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
    match profile.protocol {
        Protocol::OpenAiChat => {
            // Disable is portable here: compatible servers (vLLM/SGLang) accept
            // `reasoning_effort: "none"` (LLM_REFACTOR translation matrix).
            // Enabling still needs a concrete level, which normalize supplies.
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
        Self {
            connection: Mutex::new(connection),
            http: reqwest::Client::new(),
            caps: ProviderCaps {
                vision: false,
                caching: false,
                // Unknown until discovered/configured — never a fabricated
                // constant (it would mislead the context compiler, §4.3).
                max_ctx,
                // Initial selection only, not an asserted capability. The
                // tool-calling ladder (§4.4) owns the runtime contract: it
                // attempts the selected strategy and downgrades on failure so a
                // schema-valid intent (or a structured parse failure) always
                // reaches the kernel (P1/P10). Discovery or config may pin a
                // lower rung up front for endpoints known to lack native calls.
                tool_calls: ToolCallStrategy::Native,
            },
            reasoning: Mutex::new(ReasoningConfig::default()),
            streaming: std::sync::atomic::AtomicBool::new(true),
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

    pub fn with_max_output_tokens(self, tokens: u64) -> Self {
        self.connection.lock().unwrap().profile.max_output_tokens = Some(tokens);
        self
    }

    pub fn with_token_counter(self, counter: OpenAiTokenCounter) -> Self {
        self.connection.lock().unwrap().profile.token_counter = counter;
        self
    }

    pub fn with_token_accounting(self, mode: TokenAccountingMode) -> Self {
        self.connection.lock().unwrap().profile.token_accounting = mode;
        self
    }

    /// Pin the initial tool-calling strategy (from discovery or config) for
    /// endpoints whose capability is known ahead of time.
    pub fn with_tool_calls(mut self, strategy: ToolCallStrategy) -> Self {
        self.caps.tool_calls = strategy;
        self
    }

    /// Set the initial reasoning config (from `medha.lock`), before wrapping
    /// in `Arc`. Equivalent to `set_reasoning` but chainable at construction.
    pub fn with_reasoning(self, config: ReasoningConfig) -> Result<Self, ProviderError> {
        self.set_reasoning(config)?;
        Ok(self)
    }

    /// Switch the active OpenAI-compatible connection. Safe callers invoke
    /// this only between turns; [`Provider::stream`] snapshots the connection
    /// before I/O so an already-running request is never retargeted midstream.
    pub fn switch_connection(
        &self,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) {
        self.switch_connection_with_context(base_url, api_key, model, None);
    }

    /// Same as [`Self::switch_connection`], with the context limit discovered
    /// or configured for the selected profile. This value feeds the kernel's
    /// context compiler on the very next turn.
    pub fn switch_connection_with_context(
        &self,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        max_ctx: Option<u32>,
    ) {
        self.switch_connection_profile(
            base_url,
            api_key,
            model,
            max_ctx,
            None,
            OpenAiTokenCounter::None,
            TokenAccountingMode::Adaptive,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn switch_connection_profile(
        &self,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        max_ctx: Option<u32>,
        max_output_tokens: Option<u64>,
        token_counter: OpenAiTokenCounter,
        accounting_mode: TokenAccountingMode,
    ) {
        let api_key = api_key.into();
        let auth = if api_key.trim().is_empty() {
            AuthKind::None
        } else {
            AuthKind::Bearer
        };
        let mut profile = ProviderProfile::openai_chat(base_url, model, auth);
        profile.max_ctx = max_ctx;
        profile.max_output_tokens = max_output_tokens;
        profile.token_counter = token_counter;
        profile.token_accounting = accounting_mode;
        // Compatibility API retains its infallible signature. Values supplied
        // through current callers are already validated by config resolution.
        *self.connection.lock().unwrap() = Connection {
            profile,
            credential: api_key,
        };
    }

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
        // Normalise the carried-over reasoning before validating against the new
        // profile: a Gemini session leaves "reasoning on" with no explicit effort,
        // which openai-chat rejects. Defaulting to Minimal here lets the switch
        // succeed and keeps the stored config valid for the new protocol.
        let reasoning = normalize_reasoning(self.reasoning());
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
        cfg.effort.map(|effort| match effort {
            // Compatible servers (vLLM/SGLang) accept only none/low/medium/high;
            // "minimal" is OpenAI-only. Floor Minimal to "low" on this route.
            ReasoningEffort::Minimal | ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        })
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

        let resp = http::with_profile(
            self.http.post(&url),
            &connection.profile,
            &connection.credential,
        )?
        .json(&request.body)
        .send()
        .await
        .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let resp = http::require_success(resp).await?;

        if !streaming {
            let body = resp
                .text()
                .await
                .map_err(|error| ProviderError::Transport(error.to_string()))?;
            let blocks = gemini_interactions::parse_interaction(&body, &names)?;
            return Ok(futures::stream::iter(blocks.into_iter().map(Ok)).boxed());
        }

        let byte_stream = resp.bytes_stream();
        let stream = async_stream::stream! {
            let mut sse = crate::transport::sse::SseDecoder::default();
            let mut decoder = gemini_interactions::ResponseDecoder::new(names);

            futures::pin_mut!(byte_stream);
            while let Some(chunk) = byte_stream.next().await {
                match chunk {
                    Err(error) => {
                        yield Err(ProviderError::Transport(error.to_string()));
                        return;
                    }
                    Ok(bytes) => {
                        for event in sse.push(&bytes) {
                            match decoder.push(&event) {
                                Ok(blocks) => {
                                    for block in blocks {
                                        yield Ok(block);
                                    }
                                }
                                Err(error) => {
                                    yield Err(error);
                                    return;
                                }
                            }
                        }
                    }
                }
            }

            if let Some(event) = sse.finish() {
                match decoder.push(&event) {
                    Ok(blocks) => {
                        for block in blocks {
                            yield Ok(block);
                        }
                    }
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                }
            }

            if let Err(error) = decoder.finish() {
                yield Err(error);
            }
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
        let http_request = http::with_profile(
            self.http.post(&url),
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

#[cfg(test)]
use openai_chat::{
    ThinkTagFilter, finalize_tool_calls, parse_completion, process_sse_event, repair_args,
    sniff_target,
};

/// A model advertised by an endpoint, with whatever capability metadata the
/// server chose to expose. `context_length` is `None` when the endpoint doesn't
/// report it — never guessed (§4.3/§4.4).
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub context_length: Option<u32>,
}

/// Discover the models an OpenAI-compatible endpoint serves via `GET /v1/models`
/// (§4.4). Lets the setup wizard show a *picker* instead of asking the user to
/// type a model id blind, and captures the context window when the server
/// reports it. Servers spell that field differently, so we accept the common
/// spellings; absence stays `None`. Errors if the endpoint doesn't implement
/// `/models` so callers can fall back to manual entry.
pub async fn list_models(base_url: &str, api_key: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    let auth = if api_key.trim().is_empty() {
        AuthKind::None
    } else {
        AuthKind::Bearer
    };
    let profile = ProviderProfile::openai_chat(base_url, "model-discovery", auth);
    list_models_for_profile(&profile, api_key).await
}

pub async fn list_models_for_profile(
    profile: &ProviderProfile,
    credential: &str,
) -> Result<Vec<ModelInfo>, ProviderError> {
    profile.validate().map_err(ProviderError::Decode)?;
    if !matches!(
        profile.protocol,
        Protocol::OpenAiChat | Protocol::GeminiInteractions
    ) {
        return Err(ProviderError::Decode(format!(
            "model discovery is not implemented for {}",
            profile.protocol.as_str()
        )));
    }
    if profile.auth.requires_credential() && credential.trim().is_empty() {
        return Err(ProviderError::Decode(
            "model discovery authentication requires a credential".into(),
        ));
    }
    let url = format!("{}/models", profile.base_url.trim_end_matches('/'));
    // Bounded timeout: this runs at startup before the UI paints, so a slow or
    // unreachable endpoint must not hang the launch (it falls back gracefully).
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(|e| ProviderError::Transport(e.to_string()))?;
    match profile.protocol {
        Protocol::OpenAiChat => list_openai_chat_models(&client, &url, profile, credential).await,
        Protocol::GeminiInteractions => {
            list_gemini_models(&client, &url, profile, credential).await
        }
        Protocol::OpenAiResponses | Protocol::AnthropicMessages => unreachable!(),
    }
}

async fn list_openai_chat_models(
    client: &reqwest::Client,
    url: &str,
    profile: &ProviderProfile,
    credential: &str,
) -> Result<Vec<ModelInfo>, ProviderError> {
    #[derive(Deserialize)]
    struct ModelsResp {
        data: Vec<ModelEntry>,
    }
    #[derive(Deserialize)]
    struct ModelEntry {
        id: String,
        // Different servers expose the window under different keys; map them all.
        #[serde(default, alias = "max_model_len", alias = "context_window")]
        context_length: Option<u32>,
    }

    let resp = http::with_profile(client.get(url), profile, credential)?
        .send()
        .await
        .map_err(|error| ProviderError::Transport(error.to_string()))?;
    let parsed: ModelsResp = http::require_success(resp)
        .await?
        .json()
        .await
        .map_err(|error| ProviderError::Decode(error.to_string()))?;
    Ok(parsed
        .data
        .into_iter()
        .map(|model| ModelInfo {
            id: model.id,
            context_length: model.context_length,
        })
        .collect())
}

async fn list_gemini_models(
    client: &reqwest::Client,
    url: &str,
    profile: &ProviderProfile,
    credential: &str,
) -> Result<Vec<ModelInfo>, ProviderError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ModelsResp {
        #[serde(default)]
        models: Vec<ModelEntry>,
        next_page_token: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ModelEntry {
        name: String,
        base_model_id: Option<String>,
        input_token_limit: Option<u32>,
        #[serde(default)]
        supported_generation_methods: Vec<String>,
    }

    let mut page_token: Option<String> = None;
    let mut models = std::collections::BTreeMap::<String, Option<u32>>::new();
    // Google currently permits up to 1000 entries per page. Keep pagination
    // bounded and reject a repeated token instead of looping on a malformed
    // upstream response.
    let mut seen_tokens = std::collections::HashSet::new();
    loop {
        let mut request = client.get(url).query(&[("pageSize", "1000")]);
        if let Some(token) = page_token.as_deref() {
            request = request.query(&[("pageToken", token)]);
        }
        let resp = http::with_profile(request, profile, credential)?
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let page: ModelsResp = http::require_success(resp)
            .await?
            .json()
            .await
            .map_err(|error| ProviderError::Decode(error.to_string()))?;

        for model in page.models {
            // The Models API also returns embedding-only models. When methods
            // are advertised, exclude entries that have no generative action.
            // An empty methods list remains visible because absence is unknown,
            // not proof that Interactions is unsupported.
            let generative = model.supported_generation_methods.is_empty()
                || model.supported_generation_methods.iter().any(|method| {
                    matches!(
                        method.to_ascii_lowercase().as_str(),
                        "generatecontent" | "predict" | "interactions" | "createinteraction"
                    )
                });
            if !generative {
                continue;
            }
            let id = model
                .base_model_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| {
                    model
                        .name
                        .strip_prefix("models/")
                        .unwrap_or(&model.name)
                        .to_string()
                });
            if !id.is_empty() {
                models
                    .entry(id)
                    .and_modify(|limit| {
                        if limit.is_none() {
                            *limit = model.input_token_limit;
                        }
                    })
                    .or_insert(model.input_token_limit);
            }
        }

        let Some(token) = page.next_page_token.filter(|token| !token.is_empty()) else {
            break;
        };
        if !seen_tokens.insert(token.clone()) {
            return Err(ProviderError::Decode(
                "Gemini model discovery returned a repeated page token".into(),
            ));
        }
        page_token = Some(token);
    }

    Ok(models
        .into_iter()
        .map(|(id, context_length)| ModelInfo { id, context_length })
        .collect())
}

/// vLLM's `/tokenize` lives at the server root, a sibling of `/v1` — not under
/// it. Derive the root from the (usually `…/v1`) chat base URL.
fn vllm_tokenize_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    format!("{root}/tokenize")
}

/// vLLM `/tokenize` body: messages rendered through the model's chat template.
/// Keeps every role (system/tool/assistant) and mirrors the same system-hoist
/// as `build_chat_messages`, so the count matches what the chat endpoint would
/// actually send — and a strict template can't reject a mid-array `system`.
fn vllm_tokenize_body_from_prepared(request: &PreparedModelRequest) -> serde_json::Value {
    openai_chat::vllm_tokenize_body(request)
}

#[cfg(test)]
fn build_chat_messages(messages: &[Message]) -> Vec<openai_chat::ChatMessage> {
    openai_chat::build_chat_messages(messages, &std::collections::HashMap::new())
}

#[async_trait]
impl Provider for ProviderClient {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    fn context_window(&self) -> Option<u32> {
        self.connection.lock().unwrap().profile.max_ctx
    }

    fn protocol(&self) -> Protocol {
        self.connection.lock().unwrap().profile.protocol
    }

    fn model_limits(&self) -> ModelLimits {
        // Read both mutable limits under one guard. Calling `context_window()`
        // from this struct literal used to lock `connection` a second time
        // while the temporary guard for `max_output_tokens` was still alive,
        // deadlocking bare `medha` before the TUI could open.
        let connection = self.connection.lock().unwrap();
        ModelLimits {
            max_input_tokens: None,
            max_output_tokens: connection.profile.max_output_tokens,
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

    fn set_reasoning(&self, config: ReasoningConfig) -> Result<(), ProviderError> {
        let config = normalize_reasoning(config);
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

    fn prepare_request(
        &self,
        ctx: &CompiledContext,
    ) -> Result<PreparedModelRequest, ProviderError> {
        let connection = self.connection.lock().unwrap().clone();
        let model = if ctx.model.is_empty() {
            connection.profile.model.clone()
        } else {
            ctx.model.clone()
        };
        let streaming = self.streaming();
        let body = match connection.profile.protocol {
            Protocol::OpenAiChat => openai_chat::prepare_body(
                ctx,
                &model,
                streaming,
                self.reasoning_effort_value(),
                connection.profile.max_output_tokens,
            )?,
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
        Ok(PreparedModelRequest::new(
            connection.profile.protocol,
            model,
            body,
            ctx.clone(),
        ))
    }

    async fn count_input_tokens(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<Option<InputTokenCount>, TokenCountError> {
        let counter = self.connection.lock().unwrap().profile.token_counter;
        match counter {
            OpenAiTokenCounter::None => Ok(None),
            OpenAiTokenCounter::Vllm => self.count_vllm(request).await.map(Some),
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
                object.insert("max_tokens".into(), serde_json::json!(max_output_tokens));
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
        let url = format!(
            "{}/chat/completions",
            connection.profile.base_url.trim_end_matches('/')
        );

        // Direct proof of what actually goes over the wire — no guessing, no
        // separate curl experiment needed. Set MEDHA_DEBUG_HTTP=1 to see the
        // exact prepared JSON for every call.
        http::debug_json_request("POST", &url, &request.body);

        let resp = http::with_profile(
            self.http.post(&url),
            &connection.profile,
            &connection.credential,
        )?
        .json(&request.body)
        .send()
        .await
        .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let resp = http::require_success(resp).await?;

        // Non-streaming: one blocking body, parsed and yielded as a single batch
        // of blocks (reasoning → text → tool intents → usage). The kernel loop
        // is identical; only the arrival shape differs.
        if !streaming {
            let body = resp
                .text()
                .await
                .map_err(|e| ProviderError::Transport(e.to_string()))?;
            let blocks = openai_chat::parse_completion(&body, &names)?;
            return Ok(futures::stream::iter(blocks.into_iter().map(Ok)).boxed());
        }

        let byte_stream = resp.bytes_stream();
        let s = async_stream::stream! {
            let mut sse = crate::transport::sse::SseDecoder::default();
            let mut decoder = openai_chat::ResponseDecoder::new(names);

            futures::pin_mut!(byte_stream);
            while let Some(chunk) = byte_stream.next().await {
                match chunk {
                    Err(e) => {
                        // Surface what the filter is still holding before the
                        // error — otherwise the reply's tail silently vanishes.
                        if let Some(b) = decoder.flush_pending() {
                            yield Ok(b);
                        }
                        yield Err(ProviderError::Transport(e.to_string()));
                        return;
                    }
                    Ok(bytes) => for event in sse.push(&bytes) {
                    match decoder.push(&event) {
                        Ok(blocks) => for b in blocks { yield Ok(b); },
                        Err(e) => { yield Err(e); return; }
                    }
                    },
                }
            }

            // Preserve tolerant handling for servers that close without the
            // final SSE blank line.
            if let Some(event) = sse.finish() {
                match decoder.push(&event) {
                    Ok(blocks) => for b in blocks { yield Ok(b); },
                    Err(e) => { yield Err(e); return; }
                }
            }

            for block in decoder.finish() {
                yield Ok(block);
            }
        };

        Ok(s.boxed())
    }
}

/// Canonical tool name → the name sent on the wire. Strict OpenAI-compatible
/// validators (NVIDIA API among them) enforce `[a-zA-Z0-9_-]+` for function
/// names, so the dotted canonical names (`fs.edit`) are mapped (`fs_edit`) at
/// this boundary — the rest of the system never sees wire names.
#[cfg(test)]
fn wire_tool_name(name: &str) -> String {
    openai_chat::wire_tool_name(name)
}

/// Wire→canonical map for the tool names exposed this request. Collisions get a
/// trailing `_` so two canonical names can never share a wire name.
fn wire_name_map(canonical: &[String]) -> std::collections::HashMap<String, String> {
    openai_chat::wire_name_map(canonical)
}

#[cfg(test)]
fn process_sse_record(
    record: &str,
    accum: &mut std::collections::BTreeMap<u32, (String, String, String)>,
    think_filter: &mut ThinkTagFilter,
    target_announced: &mut std::collections::HashSet<u32>,
) -> Result<Vec<Block>, ProviderError> {
    let Some(event) = crate::transport::sse::decode_record(record.as_bytes()) else {
        return Ok(Vec::new());
    };
    process_sse_event(&event, accum, think_filter, target_announced)
}

#[cfg(test)]
mod repair_args_tests {
    use super::repair_args;
    use serde_json::json;

    #[test]
    fn well_formed_args_pass_through_untouched() {
        let args = json!({"path": "src/main.rs", "limit": 5});
        assert_eq!(repair_args(args.clone()), args);
    }

    #[test]
    fn double_encoded_args_are_unwrapped() {
        // The whole object arrived as a JSON string — the exact shape behind
        // the "expected string 'path'" failures.
        let args = json!("{\"path\": \"/w/PROGRESS.md\"}");
        assert_eq!(repair_args(args), json!({"path": "/w/PROGRESS.md"}));
    }

    #[test]
    fn envelope_keys_are_unwrapped_object_or_stringified() {
        assert_eq!(
            repair_args(json!({"arguments": {"path": "a.md"}})),
            json!({"path": "a.md"})
        );
        assert_eq!(
            repair_args(json!({"input": "{\"path\": \"a.md\"}"})),
            json!({"path": "a.md"})
        );
    }

    #[test]
    fn plain_strings_and_real_single_keys_are_not_mangled() {
        // A non-JSON string stays as-is (still fails honestly downstream).
        assert_eq!(repair_args(json!("just text")), json!("just text"));
        // A single key that isn't an envelope name is a real argument.
        assert_eq!(
            repair_args(json!({"path": "arguments.md"})),
            json!({"path": "arguments.md"})
        );
        // An envelope-named key holding a plain string is a real argument too.
        assert_eq!(
            repair_args(json!({"input": "hello"})),
            json!({"input": "hello"})
        );
    }
}

#[cfg(test)]
mod sniff_tests {
    use super::sniff_target;

    #[test]
    fn extracts_path_from_partial_args_before_content_finishes() {
        // Path is complete but the huge `content` is still streaming — we still get it.
        assert_eq!(
            sniff_target(r#"{"path":"medha.html","content":"<!doctype html"#),
            Some("medha.html".to_string())
        );
    }

    #[test]
    fn none_until_closing_quote_arrives() {
        assert_eq!(sniff_target(r#"{"file_path": "src/mai"#), None);
        assert_eq!(
            sniff_target(r#"{"file_path":"src/main.rs"}"#),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn command_and_unknown_keys() {
        assert_eq!(
            sniff_target(r#"{"command":"cargo build"}"#),
            Some("cargo build".to_string())
        );
        assert_eq!(sniff_target(r#"{"pattern":"foo"}"#), None);
    }

    #[test]
    fn escaped_quotes_do_not_cut_the_value_short() {
        // K19: an escaped quote inside the value is content, not the closer.
        assert_eq!(
            sniff_target(r#"{"command":"echo \"hello world\" > f.txt"}"#),
            Some(r#"echo "hello world" > f.txt"#.to_string())
        );
        // Escaped backslash before the real closing quote still closes correctly.
        assert_eq!(
            sniff_target(r#"{"path":"dir\\file.rs"}"#),
            Some(r"dir\file.rs".to_string())
        );
        // Still-open value ending mid-escape is incomplete, not a hit.
        assert_eq!(sniff_target(r#"{"command":"echo \"unfinished"#), None);
    }
}

#[cfg(test)]
mod sse_tests {
    use super::*;
    use std::collections::{BTreeMap, HashSet};

    fn drive(record: &str) -> Vec<Block> {
        let mut accum = BTreeMap::new();
        let mut tf = ThinkTagFilter::default();
        let mut announced = HashSet::new();
        let mut blocks = process_sse_record(record, &mut accum, &mut tf, &mut announced).unwrap();
        // The think-tag filter holds a tail back pending a possible `<think>`;
        // flush at end-of-stream (as the real driver does) to emit it.
        if let Some(b) = tf.flush() {
            blocks.push(b);
        }
        blocks
    }

    /// Drive several SSE content records through the real parsing path (record
    /// → delta → think filter), flushing at end-of-stream like the driver does.
    fn drive_many(deltas: &[&str]) -> Vec<Block> {
        let mut accum = BTreeMap::new();
        let mut tf = ThinkTagFilter::default();
        let mut announced = HashSet::new();
        let mut blocks = Vec::new();
        for d in deltas {
            let record = format!(
                "data: {}\n",
                serde_json::json!({"choices":[{"delta":{"content": d}}]})
            );
            blocks
                .extend(process_sse_record(&record, &mut accum, &mut tf, &mut announced).unwrap());
        }
        if let Some(b) = tf.flush() {
            blocks.push(b);
        }
        blocks
    }

    fn join(blocks: &[Block]) -> (String, String) {
        let mut text = String::new();
        let mut reasoning = String::new();
        for b in blocks {
            match b {
                Block::Text(t) => text.push_str(t),
                Block::Reasoning(r) => reasoning.push_str(r),
                _ => {}
            }
        }
        (text, reasoning)
    }

    #[test]
    fn answer_mentioning_think_tags_streams_fully_visible() {
        // End-to-end regression for the reported bug: the model streams an
        // answer that DOCUMENTS think tags ("Shape 2: `<think>` tags"), with
        // the tag split across SSE deltas exactly as a real stream does. The
        // whole reply must arrive as visible text; nothing may be rerouted
        // into hidden reasoning.
        let full = "Reasoning support (Shape 1: `reasoning_content` field; Shape 2: `<think>` tags)\n- Tool call strategies: `Native`, `Guided` (planned)\n- `models.dev` integration for pricing";
        let deltas = [
            "Reasoning support (Shape 1: `reasoning_content` field; Shape 2: `<th",
            "ink>` tags)\n- Tool call strategies: `Nati",
            "ve`, `Guided` (planned)\n- `models.dev` integration for pricing",
        ];
        let (text, reasoning) = join(&drive_many(&deltas));
        assert_eq!(
            reasoning, "",
            "no part of the answer may become hidden reasoning"
        );
        assert_eq!(text, full, "the full answer must stay visible");
    }

    #[test]
    fn genuine_leading_thinking_still_separates_from_answer() {
        // The same path with REAL inline thinking: block stripped into the
        // reasoning lane, answer intact.
        let deltas = ["<think>weigh the", " options</think>", "The answer is 42."];
        let (text, reasoning) = join(&drive_many(&deltas));
        assert_eq!(reasoning, "weigh the options");
        assert_eq!(text, "The answer is 42.");
    }

    #[test]
    fn crlf_record_is_parsed_not_dropped() {
        // A CRLF-delimited content frame must yield its text (previously the
        // whole response was silently dropped on CRLF servers).
        let blocks = drive("data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\r\n");
        let text: String = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "hello",
            "CRLF record must be parsed, not dropped: {blocks:?}"
        );
    }

    // ── K13: a tool call the server never gave an id gets a synthesized one ─────
    #[test]
    fn missing_tool_call_id_is_synthesized() {
        let mut accum = BTreeMap::new();
        // index 0, a name, no id (empty) — as a lax gateway streams it.
        accum.insert(
            0u32,
            (
                String::new(),
                "fs.read".to_string(),
                r#"{"path":"a"}"#.to_string(),
            ),
        );
        let intents = finalize_tool_calls(accum, &std::collections::HashMap::new());
        assert_eq!(intents.len(), 1);
        assert_eq!(
            intents[0].id, "call_0",
            "empty id must be synthesized, not left blank"
        );
        assert_eq!(intents[0].tool, "fs.read");
        assert_eq!(intents[0].args["path"], "a");
    }

    #[test]
    fn present_tool_call_id_is_preserved() {
        let mut accum = BTreeMap::new();
        accum.insert(
            0u32,
            ("real-id".to_string(), "fs.read".to_string(), String::new()),
        );
        let intents = finalize_tool_calls(accum, &std::collections::HashMap::new());
        assert_eq!(intents[0].id, "real-id");
        assert_eq!(
            intents[0].args,
            serde_json::json!({}),
            "empty args → empty object"
        );
    }
}

#[cfg(test)]
mod think_tag_tests {
    use super::*;

    fn collect(blocks: &[Block]) -> (String, String) {
        let mut text = String::new();
        let mut reasoning = String::new();
        for b in blocks {
            match b {
                Block::Text(t) => text.push_str(t),
                Block::Reasoning(r) => reasoning.push_str(r),
                _ => {}
            }
        }
        (text, reasoning)
    }

    #[test]
    fn strips_a_leading_think_block() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("<think>hmm let me consider</think> after");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, " after");
        assert_eq!(reasoning, "hmm let me consider");
    }

    #[test]
    fn leading_tag_split_across_chunks_still_matches() {
        let mut f = ThinkTagFilter::default();
        let mut all = Vec::new();
        // Whitespace before the tag is fine; "<think>"/"</think>" split mid-tag.
        all.extend(f.feed("  <thi"));
        all.extend(f.feed("nk>reasoning here</th"));
        all.extend(f.feed("ink> end"));
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "   end");
        assert_eq!(reasoning, "reasoning here");
    }

    #[test]
    fn literal_think_mid_answer_is_content_not_a_tag() {
        // The reported bug: the model writes documentation ABOUT `<think>`
        // tags mid-answer — everything after got hidden as collapsed
        // reasoning and the user saw their reply "cut off".
        let mut f = ThinkTagFilter::default();
        let mut all = Vec::new();
        all.extend(f.feed("Reasoning support (Shape 2: `<think>"));
        all.extend(f.feed("` tags). The rest of the answer must stay visible."));
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(
            reasoning, "",
            "mid-answer literal must not open a think block"
        );
        assert_eq!(
            text,
            "Reasoning support (Shape 2: `<think>` tags). The rest of the answer must stay visible."
        );
    }

    #[test]
    fn unclosed_literal_tag_at_answer_end_is_not_swallowed() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("use <think");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "use <think");
        assert_eq!(reasoning, "");
    }

    #[test]
    fn mid_response_block_at_line_start_is_reasoning() {
        // Chat templates emit think blocks on their own line — one arriving
        // mid-response after a newline is real thinking, not content.
        let mut f = ThinkTagFilter::default();
        let mut all = Vec::new();
        all.extend(f.feed("para one\n\n<think>plan the "));
        all.extend(f.feed("next step</think>\npara two"));
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "para one\n\n\npara two");
        assert_eq!(reasoning, "plan the next step");
    }

    #[test]
    fn literal_tag_pair_mid_line_stays_content() {
        // Both tags mentioned inline (docs/code sample) — nothing is stripped.
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("wrap reasoning in `<think>` and `</think>` in templates");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(
            text,
            "wrap reasoning in `<think>` and `</think>` in templates"
        );
        assert_eq!(reasoning, "");
    }

    #[test]
    fn back_to_back_leading_blocks_both_strip() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("<think>a</think><think>b</think>hello");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "hello");
        assert_eq!(reasoning, "ab");
    }

    #[test]
    fn no_tags_passes_through_as_text() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("just a normal reply, no thinking here");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "just a normal reply, no thinking here");
        assert!(reasoning.is_empty());
    }
}

#[cfg(test)]
mod count_tokens_tests {
    use super::*;
    use kernel::{BlastRadius, ToolCategory, ToolSpec};

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "read a file".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            blast_radius: BlastRadius::Read,
            category: ToolCategory::Read,
            icon: "r".into(),
        }
    }

    #[test]
    fn tokenize_url_is_server_root_sibling_of_v1() {
        assert_eq!(
            vllm_tokenize_url("https://h.example/v1"),
            "https://h.example/tokenize"
        );
        assert_eq!(
            vllm_tokenize_url("https://h.example/v1/"),
            "https://h.example/tokenize"
        );
        // No /v1 suffix → append at the given root.
        assert_eq!(
            vllm_tokenize_url("https://h.example"),
            "https://h.example/tokenize"
        );
    }

    #[test]
    fn vllm_body_is_derived_from_the_complete_prepared_chat_request() {
        let msgs = vec![
            Message::system("s"),
            Message::user("u"),
            Message::tool_result("c1", "t"),
        ];
        let provider = OpenAiCompat::new("http://x/v1", "", "m");
        let prepared = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: msgs,
                ordered: None,
                tools: vec![tool("fs.read")],
            })
            .unwrap();
        let body = vllm_tokenize_body_from_prepared(&prepared);
        assert_eq!(body["add_generation_prompt"], serde_json::json!(true));
        assert!(body.get("stream").is_none());
        assert_eq!(body["tools"][0]["function"]["name"], "fs_read");
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[2]["role"], "tool");
    }

    #[test]
    fn vllm_body_hoists_a_mid_array_system_like_the_chat_path() {
        let msgs = vec![
            Message::system("SYS"),
            Message::user("u"),
            Message::system("earlier summary"), // mid-array (compaction)
            Message::new(Role::Assistant, "ok"),
        ];
        let provider = OpenAiCompat::new("http://x/v1", "", "m");
        let prepared = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: msgs,
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        let body = vllm_tokenize_body_from_prepared(&prepared);
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[0]["content"], "SYS\n\nearlier summary");
        assert_eq!(arr[1]["role"], "user");
        assert_eq!(arr[2]["role"], "assistant");
    }

    #[test]
    fn generic_openai_chat_does_not_probe_a_vendor_count_route() {
        let provider = OpenAiCompat::new("http://127.0.0.1:1/v1", "", "m");
        let prepared = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::user("hello")],
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        let result = futures::executor::block_on(provider.count_input_tokens(&prepared)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn fingerprint_changes_when_the_prepared_tool_schema_changes() {
        let provider = OpenAiCompat::new("http://x/v1", "", "m");
        let make = |tools| {
            provider
                .prepare_request(&CompiledContext {
                    model: String::new(),
                    messages: vec![Message::user("hello")],
                    ordered: None,
                    tools,
                })
                .unwrap()
        };
        let without = make(Vec::new());
        let with = make(vec![tool("fs.read")]);
        assert_ne!(without.request_fingerprint, with.request_fingerprint);
    }

    #[tokio::test]
    async fn declared_vllm_counter_posts_the_full_prepared_input() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::sync::oneshot;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before headers");
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            assert!(headers.starts_with("POST /tokenize HTTP/1.1"), "{headers}");
            let content_length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())
                        .flatten()
                })
                .expect("content-length");
            while bytes.len() - header_end < content_length {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before body");
                bytes.extend_from_slice(&chunk[..read]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
            tx.send(body).unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 13\r\nconnection: close\r\n\r\n{\"count\":321}",
                )
                .await
                .unwrap();
        });

        let provider = OpenAiCompat::new(format!("http://{address}/v1"), "", "m")
            .with_token_counter(OpenAiTokenCounter::Vllm);
        let prepared = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::system("s"), Message::user("hello")],
                ordered: None,
                tools: vec![tool("fs.read")],
            })
            .unwrap();
        let count = provider
            .count_input_tokens(&prepared)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(count.tokens, 321);
        assert_eq!(count.quality, TokenCountQuality::Authoritative);
        assert_eq!(count.request_fingerprint, prepared.request_fingerprint);
        let body = rx.await.unwrap();
        assert_eq!(body["tools"][0]["function"]["name"], "fs_read");
        assert_eq!(body["add_generation_prompt"], true);
        assert!(body.get("stream").is_none());
        server.await.unwrap();
    }

    #[test]
    fn build_chat_hoists_and_merges_system_to_the_front() {
        // Regression: vLLM rejects a `system` message that isn't first, and
        // compaction can insert a summary as a mid-array system message. All
        // system content must merge into one leading message, rest kept in order.
        let msgs = vec![
            Message::system("SYS PROMPT"),
            Message::user("do X"),
            Message::system("earlier conversation summary"), // mid-array (compaction)
            Message::new(Role::Assistant, "ok"),
            Message::tool_result("c1", "out"),
        ];
        let built = build_chat_messages(&msgs);
        assert_eq!(built[0].role, "system");
        assert!(built[0].content.starts_with("SYS PROMPT"));
        assert!(built[0].content.contains("earlier conversation summary"));
        assert_eq!(built.iter().filter(|m| m.role == "system").count(), 1);
        // non-system messages keep their exact order (tool pairing untouched)
        assert_eq!(built[1].role, "user");
        assert_eq!(built[2].role, "assistant");
        assert_eq!(built[3].role, "tool");
    }
}

#[cfg(test)]
mod wire_tool_name_tests {
    use super::*;
    use kernel::ToolIntent;

    fn is_strict_valid(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    #[test]
    fn dotted_names_become_strict_valid() {
        for n in ["fs.edit", "shell.exec", "memory.write", "web.fetch"] {
            let wire = wire_tool_name(n);
            assert!(is_strict_valid(&wire), "{n} -> {wire} still invalid");
        }
        assert_eq!(wire_tool_name("fs.edit"), "fs_edit");
        // Already-valid names pass through unchanged.
        assert_eq!(wire_tool_name("read_artifact"), "read_artifact");
    }

    #[test]
    fn map_round_trips_wire_back_to_canonical() {
        let canonical = vec!["fs.edit".to_string(), "shell.exec".to_string()];
        let map = wire_name_map(&canonical);
        assert_eq!(map.get("fs_edit").unwrap(), "fs.edit");
        assert_eq!(map.get("shell_exec").unwrap(), "shell.exec");
    }

    #[test]
    fn colliding_wire_names_stay_distinct() {
        // `fs.edit` and `fs-edit` both sanitize toward `fs_edit`/`fs-edit`;
        // force a real collision with two dot variants.
        let canonical = vec!["a.b".to_string(), "a_b".to_string()];
        let map = wire_name_map(&canonical);
        assert_eq!(map.len(), 2, "collision must not drop a tool");
        let mut targets: Vec<_> = map.values().cloned().collect();
        targets.sort();
        assert_eq!(targets, vec!["a.b", "a_b"]);
    }

    #[test]
    fn finalize_maps_wire_call_back_to_dotted_tool() {
        let mut accum = std::collections::BTreeMap::new();
        accum.insert(
            0u32,
            (
                "id".to_string(),
                "fs_edit".to_string(),
                r#"{"path":"x"}"#.to_string(),
            ),
        );
        let map = wire_name_map(&["fs.edit".to_string()]);
        let intents: Vec<ToolIntent> = finalize_tool_calls(accum, &map);
        assert_eq!(
            intents[0].tool, "fs.edit",
            "kernel must see the canonical name"
        );
    }

    #[test]
    fn parse_completion_yields_reasoning_text_tools_and_usage_in_order() {
        // The exact shape the Crystal gateway returns non-streamed: separate
        // reasoning_content, plus a wire-named tool call.
        let body = r#"{
          "choices": [{"message": {
            "content": "The answer is 391.",
            "reasoning_content": "17*23 = 17*(20+3) = 340+51 = 391",
            "tool_calls": [{"id":"c1","function":{"name":"fs_read","arguments":"{\"path\":\"a\"}"}}]
          }}],
          "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
        }"#;
        let map = wire_name_map(&["fs.read".to_string()]);
        let blocks = parse_completion(body, &map).unwrap();
        // Reasoning first, then any text, then the tool intent, then usage last.
        assert!(matches!(&blocks[0], Block::Reasoning(r) if r.contains("391")));
        let text: String = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(text.contains("391"), "text was: {text:?}");
        let intent = blocks
            .iter()
            .find_map(|b| match b {
                Block::ToolIntent(it) => Some(it),
                _ => None,
            })
            .expect("a tool intent");
        assert_eq!(intent.tool, "fs.read", "wire name mapped back to canonical");
        assert_eq!(intent.args["path"], "a");
        assert!(matches!(blocks.last(), Some(Block::Usage(u)) if u.total_tokens == 30));
    }

    #[test]
    fn parse_completion_splits_inline_think_tags_when_no_reasoning_field() {
        let body = r#"{"choices":[{"message":{"content":"<think>weighing it</think>Done."}}]}"#;
        let blocks = parse_completion(body, &std::collections::HashMap::new()).unwrap();
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, Block::Reasoning(r) if r.contains("weighing")))
        );
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, Block::Text(t) if t.contains("Done")))
        );
    }

    #[test]
    fn parse_completion_surfaces_an_error_body() {
        let body = r#"{"error":{"message":"rate limited","type":"rate_limit"}}"#;
        assert!(parse_completion(body, &std::collections::HashMap::new()).is_err());
    }

    #[test]
    fn assistant_history_tool_calls_are_sanitized() {
        let intent = ToolIntent {
            id: "c1".into(),
            tool: "fs.edit".into(),
            args: serde_json::json!({}),
        };
        let msgs = vec![Message::assistant_calls("", vec![intent])];
        let built = build_chat_messages(&msgs);
        assert_eq!(
            built[0].tool_calls[0].function.name, "fs_edit",
            "history name must be wire-valid"
        );
    }
}

#[cfg(test)]
mod gemini_client_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn native_model_discovery_uses_v1_google_auth_and_normalizes_ids() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before headers");
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&bytes);
            assert!(
                headers.starts_with("GET /v1/models?pageSize=1000 HTTP/1.1"),
                "{headers}"
            );
            assert!(
                headers
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("x-goog-api-key: discovery-secret")),
                "{headers}"
            );
            let body = serde_json::json!({
                "models": [
                    {
                        "name": "models/gemini-3.5-flash",
                        "baseModelId": "gemini-3.5-flash",
                        "inputTokenLimit": 1_048_576,
                        "supportedGenerationMethods": ["generateContent", "countTokens"]
                    },
                    {
                        "name": "models/text-embedding-004",
                        "baseModelId": "text-embedding-004",
                        "inputTokenLimit": 2_048,
                        "supportedGenerationMethods": ["embedContent"]
                    }
                ]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut profile = ProviderProfile::openai_chat(
            format!("http://{address}/v1"),
            "model-discovery",
            AuthKind::XGoogApiKey,
        );
        profile.protocol = Protocol::GeminiInteractions;
        let models = list_models_for_profile(&profile, "discovery-secret")
            .await
            .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.5-flash");
        assert_eq!(models[0].context_length, Some(1_048_576));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_stream_posts_v1_interactions_with_google_auth_and_decodes_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before headers");
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
            assert!(
                headers.starts_with("POST /v1/interactions HTTP/1.1"),
                "{headers}"
            );
            assert!(
                headers
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("x-goog-api-key: test-secret")),
                "{headers}"
            );
            assert!(
                !headers
                    .lines()
                    .any(|line| line.to_ascii_lowercase().starts_with("authorization:")),
                "{headers}"
            );
            let content_length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())
                        .flatten()
                })
                .expect("content-length");
            while bytes.len() - header_end < content_length {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before body");
                bytes.extend_from_slice(&chunk[..read]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
            tx.send(body).unwrap();

            let events = concat!(
                "event: step.start\n",
                "data: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"thought\",\"summary\":[]}}\n\n",
                "event: step.delta\n",
                "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"thought_summary\",\"content\":{\"type\":\"text\",\"text\":\"checking\"}}}\n\n",
                "event: step.delta\n",
                "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"thought_signature\",\"signature\":\"opaque-signed-state\"}}\n\n",
                "event: step.stop\n",
                "data: {\"event_type\":\"step.stop\",\"index\":0}\n\n",
                "event: step.start\n",
                "data: {\"event_type\":\"step.start\",\"index\":1,\"step\":{\"type\":\"model_output\",\"content\":[]}}\n\n",
                "event: step.delta\n",
                "data: {\"event_type\":\"step.delta\",\"index\":1,\"delta\":{\"type\":\"text\",\"text\":\"hello\"}}\n\n",
                "event: step.stop\n",
                "data: {\"event_type\":\"step.stop\",\"index\":1}\n\n",
                "event: interaction.completed\n",
                "data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"status\":\"completed\",\"usage\":{\"total_input_tokens\":3,\"total_output_tokens\":4,\"total_thought_tokens\":2,\"total_tokens\":9}}}\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                events.len(),
                events
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut profile = ProviderProfile::openai_chat(
            format!("http://{address}/v1"),
            "gemini-model",
            AuthKind::XGoogApiKey,
        );
        profile.protocol = Protocol::GeminiInteractions;
        profile.reasoning = ReasoningSupport::Effort;
        let provider = ProviderClient::from_profile(profile, "test-secret")
            .unwrap()
            .with_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Minimal),
            })
            .unwrap();
        let request = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::user("hello")],
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        assert_eq!(request.protocol, Protocol::GeminiInteractions);
        assert_eq!(request.body["store"], false);
        assert_eq!(
            request.body["generation_config"]["thinking_level"],
            "minimal"
        );

        let blocks: Vec<Block> = provider
            .stream_prepared(&request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::Text(text) if text == "hello"))
        );
        let completed = blocks
            .iter()
            .find_map(|block| match block {
                Block::CompletedMessage(message) => Some(message),
                _ => None,
            })
            .expect("canonical completed message");
        assert!(completed.parts.iter().any(|part| matches!(
            part,
            kernel::ContentPart::Reasoning(reasoning)
                if reasoning.provider_state.iter().any(|state| {
                    state.protocol == Protocol::GeminiInteractions
                        && state.value == serde_json::json!("opaque-signed-state")
                })
        )));
        assert!(matches!(blocks.last(), Some(Block::Usage(usage)) if usage.total_tokens == 9));

        let body = rx.await.unwrap();
        assert_eq!(body["model"], "gemini-model");
        assert_eq!(body["store"], false);
        server.await.unwrap();
    }

    #[test]
    fn gemini_profile_switch_is_atomic_and_output_retry_rewrites_generation_config() {
        let provider = OpenAiCompat::new("http://chat.test/v1", "", "chat-model");
        let mut profile = ProviderProfile::openai_chat(
            "https://generativelanguage.googleapis.com/v1",
            "gemini-model",
            AuthKind::XGoogApiKey,
        );
        profile.protocol = Protocol::GeminiInteractions;
        provider.switch_provider_profile(profile, "secret").unwrap();
        assert_eq!(provider.protocol(), Protocol::GeminiInteractions);
        assert_eq!(provider.active_model(), "gemini-model");

        let request = provider
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::user("hello")],
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        let retried = provider.with_output_limit(&request, 512).unwrap().unwrap();
        assert_eq!(retried.body["generation_config"]["max_output_tokens"], 512);
        assert_ne!(request.request_fingerprint, retried.request_fingerprint);
    }
}

#[cfg(test)]
mod reasoning_request_tests {
    use super::*;

    #[test]
    fn reading_model_limits_never_relocks_the_connection() {
        let provider = std::sync::Arc::new(
            OpenAiCompat::new("http://x", "", "m")
                .with_max_ctx(32_768)
                .with_max_output_tokens(4_096),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(provider.model_limits()).unwrap());

        let limits = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("model_limits deadlocked while reading the connection");
        assert_eq!(limits.max_combined_tokens, Some(32_768));
        assert_eq!(limits.max_output_tokens, Some(4_096));
    }

    #[test]
    fn effort_uses_the_portable_top_level_openai_field() {
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::High),
            })
            .unwrap();
        assert_eq!(p.reasoning_effort_value(), Some("high"));
    }

    #[test]
    fn minimal_floors_to_low_on_the_compatible_route() {
        // vLLM/SGLang reject "minimal"; only none/low/medium/high are portable.
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Minimal),
            })
            .unwrap();
        assert_eq!(p.reasoning_effort_value(), Some("low"));
    }

    #[test]
    fn explicit_off_sends_none_not_a_silent_omit() {
        // Compatible servers disable thinking via reasoning_effort:"none".
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_reasoning(ReasoningConfig {
                enabled: Some(false),
                effort: None,
            })
            .unwrap();
        assert_eq!(p.reasoning_effort_value(), Some("none"));
    }

    #[test]
    fn profile_switch_rejects_carrying_effort_into_an_unsupported_model() {
        let provider = OpenAiCompat::new("http://one", "", "model")
            .with_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Low),
            })
            .unwrap();
        let mut next = ProviderProfile::openai_chat("http://two", "other-model", AuthKind::None);
        next.reasoning = ReasoningSupport::Unsupported;

        assert!(provider.switch_provider_profile(next, "").is_err());
        assert_eq!(provider.active_model(), "model");
    }

    #[test]
    fn gemini_reasoning_on_defaults_to_minimal_and_survives_switch_to_openai_chat() {
        // A Gemini session runs with "reasoning on" and no explicit effort —
        // valid for gemini-interactions. Start a provider in exactly that state.
        let mut gemini =
            ProviderProfile::openai_chat("http://gemini/v1", "gemini-3.5-flash", AuthKind::XGoogApiKey);
        gemini.protocol = Protocol::GeminiInteractions;
        let provider = ProviderClient::from_profile(gemini, "key").unwrap();
        provider
            .set_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: None,
            })
            .unwrap();
        // Normalised to Minimal, so the canonical config is portable across
        // protocols instead of carrying an effort-less "on".
        assert_eq!(provider.reasoning().effort, Some(ReasoningEffort::Minimal));

        // Switching to an openai-chat model must now succeed — this previously
        // failed with "open-ai-chat cannot enable reasoning without an explicit
        // effort" because the effort-less Gemini config was carried across.
        let next = ProviderProfile::openai_chat("http://vllm/v1", "qwen", AuthKind::None);
        assert!(provider.switch_provider_profile(next, "").is_ok());
        assert_eq!(provider.active_model(), "qwen");
        assert_eq!(provider.reasoning().effort, Some(ReasoningEffort::Minimal));
    }

    #[test]
    fn only_explicit_first_run_constructor_allows_an_empty_profile() {
        let invalid = ProviderProfile::openai_chat("", "", AuthKind::None);
        assert!(ProviderClient::from_profile(invalid, "").is_err());

        let provider = ProviderClient::unconfigured();
        let connection = provider.connection.lock().unwrap();
        assert!(connection.profile.base_url.is_empty());
        assert!(connection.profile.model.is_empty());
    }

    #[test]
    fn untouched_reasoning_sends_no_effort_field() {
        let p = OpenAiCompat::new("http://x", "", "m");
        assert!(p.reasoning_effort_value().is_none());
    }

    #[test]
    fn switching_connection_updates_model_and_context_for_the_next_turn() {
        let p = OpenAiCompat::new("http://one", "", "first").with_max_ctx(8_192);
        assert_eq!(p.active_model(), "first");
        assert_eq!(p.context_window(), Some(8_192));

        p.switch_connection_with_context("http://two", "secret", "second", Some(32_768));
        assert_eq!(p.active_model(), "second");
        assert_eq!(p.context_window(), Some(32_768));
    }

    #[test]
    fn auth_header_is_omitted_when_empty_and_normalizes_bearer_prefix() {
        let client = reqwest::Client::new();
        let no_key = http::with_bearer(client.get("http://localhost"), "")
            .build()
            .unwrap();
        assert!(
            no_key
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );

        let key = http::with_bearer(client.get("http://localhost"), "Bearer secret")
            .build()
            .unwrap();
        assert_eq!(
            key.headers().get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer secret"
        );
    }

    #[test]
    fn full_request_json_actually_contains_the_field() {
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_reasoning(ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Medium),
            })
            .unwrap();
        let req = p
            .prepare_request(&CompiledContext {
                model: String::new(),
                messages: vec![Message::user("hello")],
                ordered: None,
                tools: Vec::new(),
            })
            .unwrap();
        assert_eq!(req.body["reasoning_effort"], "medium");
        assert!(req.body.get("chat_template_kwargs").is_none());
    }
}
