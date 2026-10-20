pub(crate) mod gemini_interactions;
pub(crate) mod openai_chat;
pub(crate) mod openai_responses;

use crate::ProviderProfile;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use kernel::{
    Block, CompiledContext, Protocol, ProviderError, ReasoningConfig, ReasoningEffort, RequestScope,
};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::time::Duration;

/// What a request is built from, whatever its wire format.
pub(crate) struct Settings<'a> {
    pub profile: &'a ProviderProfile,
    pub model: &'a str,
    pub streaming: bool,
    pub reasoning: &'a ReasoningConfig,
    pub plain_schemas: bool,
    pub scope: &'a RequestScope,
    pub stable_system: Option<&'a str>,
}

pub(crate) type Reply = BoxStream<'static, Result<Block, ProviderError>>;

/// The server and model a reply came from; state sealed to them is replayed nowhere else.
#[derive(Clone)]
pub(crate) struct Origin {
    pub server: String,
    pub model: String,
}

/// Everything the client does differently per wire format.
pub(crate) trait Wire: Send + Sync {
    /// The request body and any headers it needs.
    fn prepare(
        &self,
        context: &CompiledContext,
        settings: &Settings<'_>,
    ) -> Result<(Value, Vec<(String, String)>), ProviderError>;
    fn url(&self, base_url: &str) -> Result<String, ProviderError>;
    fn read(
        &self,
        response: reqwest::Response,
        streaming: bool,
        idle: Duration,
        names: HashMap<String, String>,
        origin: Origin,
    ) -> BoxFuture<'static, Result<Reply, ProviderError>>;
    /// False when this route takes no output cap at all.
    fn set_output_limit(
        &self,
        body: &mut Map<String, Value>,
        tokens: u64,
        profile: &ProviderProfile,
    ) -> Result<bool, ProviderError>;
    fn set_reasoning(
        &self,
        body: &mut Value,
        config: &ReasoningConfig,
        profile: &ProviderProfile,
    ) -> Result<(), ProviderError>;
    /// This format's own limits on a setting the model's levels already allow.
    fn check_reasoning(&self, config: &ReasoningConfig) -> Result<(), ProviderError>;
    /// The levels offered when the profile lists none.
    fn default_efforts(&self, model: &str) -> &'static [ReasoningEffort];
    /// Whether the format can send this level at all, even when a profile lists it.
    fn carries(&self, _effort: ReasoningEffort) -> bool {
        true
    }
    fn replays_reasoning(&self, _profile: &ProviderProfile) -> bool {
        false
    }
}

/// The one place a protocol is matched to its implementation.
pub(crate) fn wire(protocol: Protocol) -> Result<&'static dyn Wire, ProviderError> {
    match protocol {
        Protocol::OpenAiChat => Ok(&openai_chat::Chat),
        Protocol::GeminiInteractions => Ok(&gemini_interactions::Interactions),
        Protocol::OpenAiResponses => Ok(&openai_responses::Responses),
        other => Err(ProviderError::Decode(format!(
            "ProviderClient does not implement {}",
            other.as_str()
        ))),
    }
}

/// The effort a setting asks for; turning reasoning off is the `none` effort.
pub(crate) fn requested_effort(config: &ReasoningConfig) -> Option<ReasoningEffort> {
    if config.enabled == Some(false) {
        Some(ReasoningEffort::None)
    } else {
        config.effort
    }
}

fn validate_tool_arguments(
    tool: &str,
    call_id: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, kernel::ProviderError> {
    if !args.is_object() {
        return Err(kernel::ProviderError::invalid_tool_call(
            tool,
            call_id,
            "arguments must be a JSON object",
        ));
    }
    Ok(args)
}
