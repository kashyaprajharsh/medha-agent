//! The one provider client: a saved profile's connection, keys and stand-ins.

use crate::protocol;
use crate::transport::http;
use crate::{AuthKind, ProviderProfile, TokenCounter};
use async_trait::async_trait;
use futures::stream::BoxStream;
use kernel::{
    Block, CompiledContext, ImageInputMode, ImageSupport, InputTokenCount, ModelLimits,
    PreparedModelRequest, Protocol, Provider, ProviderCaps, ProviderError, ReasoningConfig,
    ReasoningEffort, ReasoningSupport, TokenAccountingMode, TokenCountError, ToolCallStrategy,
};
use std::sync::Mutex;

mod fallback;
mod reasoning;
mod token_count;

use fallback::{Fallbacks, KeySource};
use reasoning::{profile_reasoning_efforts, reconcile_reasoning_for_switch, validate_reasoning};

pub struct ProviderClient {
    /// Swapped only between turns; a stream snapshots it, so one request uses one model and key.
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

#[derive(Clone)]
struct Connection {
    profile: ProviderProfile,
    credential: String,
}

fn check_profile(profile: &ProviderProfile, credential: &str) -> Result<(), ProviderError> {
    profile.validate().map_err(ProviderError::Decode)?;
    protocol::wire(profile.protocol)?;
    if profile.auth.requires_credential() && credential.trim().is_empty() {
        return Err(ProviderError::Decode(format!(
            "{} authentication requires a credential",
            profile.protocol.as_str()
        )));
    }
    Ok(())
}

impl ProviderClient {
    /// Inert client for first-run setup; saved profiles always go through [`Self::from_profile`].
    pub fn unconfigured() -> Self {
        Self::with_connection(Connection {
            profile: ProviderProfile::openai_chat("", "", AuthKind::None),
            credential: String::new(),
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

    /// One served model: what was learned about it and its context are keyed by this.
    fn identity(profile: &ProviderProfile) -> String {
        format!(
            "{}:{}:{}",
            profile.protocol.as_str(),
            profile.base_url,
            profile.model
        )
    }

    /// Missing metadata stays `Unknown`, so a custom server is tried rather than sent a description.
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
        let credential = credential.into();
        kernel::redact::remember(&credential);
        check_profile(&profile, &credential)?;
        Ok(Self::with_connection(Connection {
            profile,
            credential,
        }))
    }

    /// Switch between turns; a running stream keeps the connection it snapshotted.
    pub fn switch_provider_profile(
        &self,
        profile: ProviderProfile,
        credential: impl Into<String>,
    ) -> Result<(), ProviderError> {
        let credential = credential.into();
        check_profile(&profile, &credential)?;
        // A carried setting the new model cannot take falls back to the server's default, never a guessed effort.
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
            .images(&Self::identity(&profile))
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
        self.learned.set_images(&Self::identity(&profile), support);
    }

    fn context_identity(&self) -> String {
        Self::identity(&self.connection.lock().unwrap().profile)
    }

    fn protocol(&self) -> Protocol {
        self.connection.lock().unwrap().profile.protocol
    }

    fn model_limits(&self) -> ModelLimits {
        // Avoid recursively locking `connection`.
        let connection = self.connection.lock().unwrap();
        ModelLimits {
            max_input_tokens: None,
            // The profile's cap is a per-request allowance, not the model's ceiling.
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
        self.stand_in_for(failed, rest)
    }

    fn standing_in(&self) -> Option<(String, String)> {
        self.stand_in_now()
    }

    fn stand_in_left(&self) -> bool {
        self.has_stand_in_left()
    }

    fn restore_primary(&self) -> Option<(String, String)> {
        self.return_to_own()
    }

    fn another_key(&self) -> bool {
        self.has_another_key()
    }

    fn next_key(&self, status: u16, asked: Option<std::time::Duration>) -> Option<(usize, usize)> {
        self.turn_to_next_key(status, asked)
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
        protocol::wire(profile.protocol).is_ok_and(|wire| wire.replays_reasoning(&profile))
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
        let wire = protocol::wire(connection.profile.protocol)?;
        let reasoning = self.reasoning();
        let stable_system = self.stable_system.lock().unwrap();
        let settings = protocol::Settings {
            profile: &connection.profile,
            model: &model,
            streaming: self.streaming(),
            reasoning: &reasoning,
            plain_schemas: self
                .plain_schemas
                .load(std::sync::atomic::Ordering::Relaxed),
            scope,
            stable_system: stable_system.as_deref(),
        };
        let (body, headers) = wire.prepare(ctx, &settings)?;
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
        let Ok(wire) = protocol::wire(request.protocol) else {
            return Ok(None);
        };
        let profile = &self.connection.lock().unwrap().profile;
        wire.set_output_limit(object, max_output_tokens, profile)?;
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
        let Ok(wire) = protocol::wire(request.protocol) else {
            return Ok(None);
        };
        let mut body = request.body.clone();
        wire.set_reasoning(&mut body, config)?;
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
        let Ok(wire) = protocol::wire(request.protocol) else {
            return Err(ProviderError::Decode(format!(
                "ProviderClient cannot send {}",
                request.protocol.as_str()
            )));
        };
        let tools: Vec<_> = request
            .context
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        let names = kernel::portable_tool_name_map(&tools);
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
        let url = wire.url(&connection.profile.base_url)?;
        // MEDHA_DEBUG_HTTP=1 prints the exact JSON sent.
        http::debug_json_request("POST", &url, &request.body);

        let mut post = self.http_for(&connection.profile)?.post(&url);
        for (name, value) in &request.headers {
            post = post.header(name, value);
        }
        let outgoing = http::with_profile(post, &connection.profile, &connection.credential)?
            .json(&request.body);
        let response = http::require_success(http::send_model_request(outgoing).await?).await?;
        let idle = http::stream_idle(&connection.profile);
        wire.read(response, streaming, idle, names).await
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
