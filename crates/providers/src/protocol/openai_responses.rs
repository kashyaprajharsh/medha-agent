//! OpenAI Responses, stateless (`store: false`). Reasoning comes back as blobs sealed to the
//! server and model that made them, so it is replayed to that pair only.

use super::{Origin, Reply, Settings, Wire, requested_effort};
use crate::protocol::openai_chat::{endpoint_url, sniff_target, wire_name_map, wire_tool_name};
use crate::transport::sse::SseEvent;
use crate::transport::stream::{self, StreamDecoder};
use crate::{AuthKind, CapabilityState, ProviderProfile};
use futures::future::BoxFuture;
use kernel::{
    Block, CompiledContext, ContentPart, ModelMessage, Protocol, ProviderError, ProviderState,
    ReasoningConfig, ReasoningEffort, ReasoningSupport, Role, ToolIntent, Usage,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

const REASONING: &str = "reasoning";
/// The plan route takes functions only inside a namespace; this is the one Medha uses.
const NAMESPACE: &str = "functions";

pub(crate) struct Responses;

/// Plan tokens take a narrower request: always streamed, no output cap, tools in a namespace.
fn plan_route(profile: &ProviderProfile) -> bool {
    profile.auth == AuthKind::ChatGptPlan
}

/// One spelling per server, however its address was typed.
fn server(base_url: &str) -> String {
    reqwest::Url::parse(base_url).map_or_else(
        |_| base_url.trim_end_matches('/').to_ascii_lowercase(),
        |url| url.as_str().trim_end_matches('/').to_string(),
    )
}

pub(crate) fn prepare_body(
    context: &CompiledContext,
    settings: &Settings<'_>,
) -> Result<Value, ProviderError> {
    let profile = settings.profile;
    let plan = plan_route(profile);
    let canonical: Vec<String> = context.tools.iter().map(|tool| tool.name.clone()).collect();
    let wire_to_canonical = wire_name_map(&canonical);
    let canonical_to_wire: HashMap<&str, &str> = wire_to_canonical
        .iter()
        .map(|(wire, canonical)| (canonical.as_str(), wire.as_str()))
        .collect();
    let replay = Replay {
        origin: Origin {
            server: server(&profile.base_url),
            model: settings.model.to_string(),
        },
        after: settings.replay_after.unwrap_or(0),
    };
    let (instructions, input) = lower_messages(
        &context.ordered_messages(),
        &canonical_to_wire,
        &replay,
        plan,
    )?;

    let mut body = Map::new();
    body.insert("model".into(), json!(settings.model));
    if let Some(instructions) = instructions {
        body.insert("instructions".into(), json!(instructions));
    }
    body.insert("input".into(), Value::Array(input));
    let functions: Vec<Value> = context
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": canonical_to_wire.get(tool.name.as_str()).copied().unwrap_or(&tool.name),
                "description": tool.description,
                "parameters": tool.schema,
                "strict": false,
            })
        })
        .collect();
    if !functions.is_empty() {
        let tools = match plan {
            true => vec![
                json!({"type": "namespace", "name": NAMESPACE, "description": "", "tools": functions}),
            ],
            false => functions,
        };
        body.insert("tools".into(), Value::Array(tools));
        body.insert("tool_choice".into(), json!("auto"));
    }
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(settings.streaming || plan));
    reasoning_fields(&mut body, profile, settings.reasoning);
    if let Some(cap) = profile.max_output_tokens.filter(|_| !plan) {
        body.insert("max_output_tokens".into(), json!(cap));
    }
    Ok(Value::Object(body))
}

/// Reasoning is asked for only where the model is known to reason or the person chose a level.
fn reasoning_fields(
    body: &mut Map<String, Value>,
    profile: &ProviderProfile,
    config: &ReasoningConfig,
) {
    body.remove("reasoning");
    body.remove("include");
    if profile.reasoning == ReasoningSupport::Unsupported {
        return;
    }
    let effort = requested_effort(config);
    if effort == Some(ReasoningEffort::None) {
        body.insert("reasoning".into(), json!({"effort": "none"}));
        return;
    }
    let reasons = effort.is_some()
        || profile.reasoning == ReasoningSupport::Effort
        || profile
            .capabilities
            .as_ref()
            .is_some_and(|caps| caps.reasoning_state() == CapabilityState::Supported);
    if !reasons {
        return;
    }
    let mut reasoning = json!({"summary": "auto"});
    if let Some(effort) = effort {
        reasoning["effort"] = json!(effort.as_str());
    }
    body.insert("reasoning".into(), reasoning);
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
}

/// Which sealed reasoning may go back: this server and model, sealed after `after`.
struct Replay {
    origin: Origin,
    after: u64,
}

fn lower_messages(
    messages: &[ModelMessage],
    canonical_to_wire: &HashMap<&str, &str>,
    replay: &Replay,
    plan: bool,
) -> Result<(Option<String>, Vec<Value>), ProviderError> {
    let mut system = String::new();
    let mut input = Vec::new();
    for message in messages {
        match message.role {
            Role::System => {
                let text = text_only(&message.parts, "system")?;
                if !text.is_empty() {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(&text);
                }
            }
            Role::User => {
                let content = user_content(&message.parts)?;
                input.push(json!({"type": "message", "role": "user", "content": content}));
            }
            Role::Assistant => {
                lower_assistant(&message.parts, canonical_to_wire, replay, plan, &mut input)?
            }
            Role::Tool => lower_tool_results(&message.parts, &mut input)?,
        }
    }
    Ok(((!system.is_empty()).then_some(system), input))
}

fn lower_assistant(
    parts: &[ContentPart],
    canonical_to_wire: &HashMap<&str, &str>,
    replay: &Replay,
    plan: bool,
    input: &mut Vec<Value>,
) -> Result<(), ProviderError> {
    for part in parts {
        match part {
            ContentPart::Text(part) => {
                reject_own_state(&part.provider_state, "assistant text")?;
                if !part.text.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": part.text}],
                    }));
                }
            }
            ContentPart::Reasoning(part) => {
                if let Some(item) = replayable(&part.provider_state, replay)? {
                    input.push(item);
                }
            }
            ContentPart::ToolCall(part) => {
                reject_own_state(&part.provider_state, "function call")?;
                let name = canonical_to_wire
                    .get(part.tool.as_str())
                    .map_or_else(|| wire_tool_name(&part.tool), |wire| (*wire).to_string());
                let mut call = json!({
                    "type": "function_call",
                    "call_id": part.id,
                    "name": name,
                    "arguments": part.args.to_string(),
                });
                if plan {
                    call["namespace"] = json!(NAMESPACE);
                }
                input.push(call);
            }
            ContentPart::ToolResult(part) => {
                reject_own_state(&part.provider_state, "assistant tool result")?;
                return Err(ProviderError::Decode(
                    "open-ai-responses cannot place a function result in an assistant message"
                        .into(),
                ));
            }
            ContentPart::Media(part) => {
                reject_own_state(&part.provider_state, "assistant media")?;
                return Err(ProviderError::Decode(
                    "open-ai-responses accepts images only in user messages".into(),
                ));
            }
        }
    }
    Ok(())
}

fn lower_tool_results(parts: &[ContentPart], input: &mut Vec<Value>) -> Result<(), ProviderError> {
    for part in parts {
        match part {
            ContentPart::ToolResult(part) => {
                reject_own_state(&part.provider_state, "function result")?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": part.tool_call_id,
                    "output": part.content,
                }));
            }
            ContentPart::Reasoning(part) => {
                reject_own_state(&part.provider_state, "tool reasoning")?
            }
            _ => {
                return Err(ProviderError::Decode(
                    "open-ai-responses tool messages carry function results only".into(),
                ));
            }
        }
    }
    Ok(())
}

fn user_content(parts: &[ContentPart]) -> Result<Vec<Value>, ProviderError> {
    let mut content = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text(part) => {
                reject_own_state(&part.provider_state, "user text")?;
                content.push(json!({"type": "input_text", "text": part.text}));
            }
            ContentPart::Media(media) => {
                reject_own_state(&media.provider_state, "user image")?;
                content.push(input_image(media)?);
            }
            ContentPart::Reasoning(part) => {
                reject_own_state(&part.provider_state, "user reasoning")?
            }
            ContentPart::ToolCall(_) | ContentPart::ToolResult(_) => {
                return Err(ProviderError::Decode(
                    "open-ai-responses cannot place a function call or result in a user message"
                        .into(),
                ));
            }
        }
    }
    Ok(content)
}

fn input_image(media: &kernel::MediaPart) -> Result<Value, ProviderError> {
    if !matches!(
        media.mime_type.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) {
        return Err(ProviderError::Decode(
            "open-ai-responses images must be PNG, JPEG, WebP, or GIF".into(),
        ));
    }
    let url = match &media.source {
        kernel::MediaSource::Url(url)
            if url.starts_with("https://") || url.starts_with("http://") =>
        {
            url.clone()
        }
        kernel::MediaSource::Base64(data) if !data.is_empty() => {
            format!("data:{};base64,{data}", media.mime_type)
        }
        kernel::MediaSource::Artifact(_) => {
            return Err(ProviderError::Decode(
                "image artifact must be resolved before provider encoding".into(),
            ));
        }
        _ => return Err(ProviderError::Decode("invalid image source".into())),
    };
    Ok(json!({"type": "input_image", "image_url": url}))
}

fn text_only(parts: &[ContentPart], role: &str) -> Result<String, ProviderError> {
    let mut text = String::new();
    for part in parts {
        match part {
            ContentPart::Text(part) => {
                reject_own_state(&part.provider_state, role)?;
                text.push_str(&part.text);
            }
            ContentPart::Reasoning(part) => reject_own_state(&part.provider_state, role)?,
            _ => {
                return Err(ProviderError::Decode(format!(
                    "open-ai-responses {role} messages carry text only"
                )));
            }
        }
    }
    Ok(text)
}

fn reject_own_state(states: &[ProviderState], location: &str) -> Result<(), ProviderError> {
    match states
        .iter()
        .find(|state| state.protocol == Protocol::OpenAiResponses)
    {
        Some(state) => Err(ProviderError::Decode(format!(
            "Responses state '{}' is invalid on {location}",
            state.kind
        ))),
        None => Ok(()),
    }
}

static FOREIGN_DROPPED: AtomicBool = AtomicBool::new(false);

/// The reasoning item to send back, if this exact server and model sealed it in time.
fn replayable(states: &[ProviderState], replay: &Replay) -> Result<Option<Value>, ProviderError> {
    let origin = &replay.origin;
    let own: Vec<_> = states
        .iter()
        .filter(|state| state.protocol == Protocol::OpenAiResponses)
        .collect();
    let [state] = own.as_slice() else {
        return match own.is_empty() {
            true => Ok(None),
            false => Err(ProviderError::Decode(
                "Responses reasoning replay requires exactly one reasoning state".into(),
            )),
        };
    };
    let value = &state.value;
    let encrypted = value
        .pointer("/item/encrypted_content")
        .and_then(Value::as_str)
        .filter(|blob| !blob.is_empty());
    let (Some(encrypted), REASONING) = (encrypted, state.kind.as_str()) else {
        return Err(ProviderError::Decode(
            "Responses reasoning state must hold a sealed reasoning item".into(),
        ));
    };
    let sealed_here = value.get("server").and_then(Value::as_str) == Some(origin.server.as_str())
        && value.get("model").and_then(Value::as_str) == Some(origin.model.as_str());
    if !sealed_here {
        if !FOREIGN_DROPPED.swap(true, Ordering::Relaxed) {
            tracing::info!("earlier reasoning from another server or model is not sent back");
        }
        return Ok(None);
    }
    if value.get("sealed_at").and_then(Value::as_u64).unwrap_or(0) < replay.after {
        return Ok(None);
    }
    let summary = value
        .pointer("/item/summary")
        .cloned()
        .unwrap_or_else(|| json!([]));
    Ok(Some(
        json!({"type": "reasoning", "encrypted_content": encrypted, "summary": summary}),
    ))
}

/// The parts one finished output item adds to the reply.
fn item_parts(
    item: &Value,
    names: &HashMap<String, String>,
    origin: &Origin,
) -> Result<Vec<ContentPart>, ProviderError> {
    let parts = match item.get("type").and_then(Value::as_str).unwrap_or_default() {
        "message" => {
            let text: String = item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("output_text") => part.get("text").and_then(Value::as_str),
                    Some("refusal") => part.get("refusal").and_then(Value::as_str),
                    _ => None,
                })
                .collect();
            (!text.is_empty())
                .then(|| {
                    ContentPart::Text(kernel::TextPart {
                        text,
                        provider_state: Vec::new(),
                    })
                })
                .into_iter()
                .collect()
        }
        "reasoning" => {
            let summary = item
                .get("summary")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n");
            let sealed = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .filter(|blob| !blob.is_empty());
            let provider_state = sealed.map_or_else(Vec::new, |blob| {
                vec![ProviderState {
                    protocol: Protocol::OpenAiResponses,
                    kind: REASONING.into(),
                    value: json!({
                        "item": {"encrypted_content": blob, "summary": item.get("summary").cloned().unwrap_or_else(|| json!([]))},
                        "server": server(&origin.server),
                        "model": origin.model,
                        "sealed_at": super::now_ms(),
                    }),
                }]
            });
            (!summary.is_empty() || !provider_state.is_empty())
                .then(|| {
                    ContentPart::Reasoning(kernel::ReasoningPart {
                        text: (!summary.is_empty()).then_some(summary),
                        provider_state,
                    })
                })
                .into_iter()
                .collect()
        }
        "function_call" => {
            let field = |name: &str| {
                item.get(name)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let wire = field("name").unwrap_or_default();
            let Some(call_id) = field("call_id").filter(|_| !wire.is_empty()) else {
                return Err(ProviderError::invalid_tool_call(
                    &wire,
                    &field("call_id").unwrap_or_default(),
                    "missing tool-call name or call_id",
                ));
            };
            let arguments = field("arguments").unwrap_or_else(|| "{}".into());
            let args = serde_json::from_str(&arguments).map_err(|error| {
                ProviderError::invalid_tool_call(
                    &wire,
                    &call_id,
                    format!("invalid or incomplete JSON arguments: {error}"),
                )
            })?;
            let args = super::validate_tool_arguments(&wire, &call_id, args)?;
            vec![ContentPart::ToolCall(kernel::ToolCallPart {
                id: call_id,
                tool: names.get(&wire).cloned().unwrap_or(wire),
                args,
                provider_state: Vec::new(),
            })]
        }
        _ => Vec::new(),
    };
    Ok(parts)
}

fn usage_block(usage: &Value) -> Option<Block> {
    let count = |path: &str| {
        usage
            .pointer(path)
            .and_then(Value::as_u64)
            .map(|count| count.min(u64::from(u32::MAX)) as u32)
    };
    let prompt_tokens = count("/input_tokens")?;
    // Reasoning tokens are already inside output_tokens; adding them again would double count.
    let completion_tokens = count("/output_tokens").unwrap_or_default();
    Some(Block::Usage(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: count("/total_tokens")
            .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens)),
        cached_prompt_tokens: count("/input_tokens_details/cached_tokens")
            .map(|cached| cached.min(prompt_tokens)),
        cache_write_tokens: None,
    }))
}

/// Why a response that did not complete ended; no tool of it ever runs.
fn unfinished(response: &Value) -> ProviderError {
    let text = |path: &str| response.pointer(path).and_then(Value::as_str);
    match response.get("status").and_then(Value::as_str) {
        Some("incomplete") => match text("/incomplete_details/reason") {
            Some("max_output_tokens") => ProviderError::Truncated,
            Some("content_filter") => {
                ProviderError::Refused("The server's content filter stopped this reply.".into())
            }
            reason => ProviderError::Response(format!(
                "the reply ended early: {}",
                reason.unwrap_or("no reason given")
            )),
        },
        _ => ProviderError::Response(format!(
            "{} ({})",
            text("/error/message").unwrap_or("the response failed"),
            text("/error/code").unwrap_or("no code"),
        )),
    }
}

/// The finished reply: tool calls, the message to keep, then usage.
fn finished(parts: Vec<ContentPart>, usage: Option<&Value>) -> Vec<Block> {
    let mut blocks: Vec<Block> = parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolCall(call) => Some(Block::ToolIntent(ToolIntent {
                id: call.id.clone(),
                tool: call.tool.clone(),
                args: call.args.clone(),
            })),
            _ => None,
        })
        .collect();
    blocks.push(Block::CompletedMessage(ModelMessage {
        role: Role::Assistant,
        parts,
        trust: None,
    }));
    blocks.extend(usage.and_then(usage_block));
    blocks
}

fn output_parts(
    response: &Value,
    names: &HashMap<String, String>,
    origin: &Origin,
) -> Result<Vec<ContentPart>, ProviderError> {
    let mut parts = Vec::new();
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        parts.extend(item_parts(item, names, origin)?);
    }
    Ok(parts)
}

pub(crate) struct ResponseDecoder {
    names: HashMap<String, String>,
    origin: Origin,
    /// Finished items by output index, used when the terminal event omits `output`.
    items: BTreeMap<u64, Vec<ContentPart>>,
    /// Calls being written: call id, wire name, and the arguments so far.
    calls: HashMap<u64, (String, String, String)>,
    targeted: HashSet<u64>,
    summaries: HashMap<u64, u64>,
    terminal: bool,
}

impl ResponseDecoder {
    pub(crate) fn new(names: HashMap<String, String>, origin: Origin) -> Self {
        Self {
            names,
            origin,
            items: BTreeMap::new(),
            calls: HashMap::new(),
            targeted: HashSet::new(),
            summaries: HashMap::new(),
            terminal: false,
        }
    }

    fn complete(&mut self, response: &Value) -> Result<Vec<Block>, ProviderError> {
        if response.get("status").and_then(Value::as_str) != Some("completed") {
            return Err(unfinished(response));
        }
        let mut parts = output_parts(response, &self.names, &self.origin)?;
        if parts.is_empty() {
            parts = std::mem::take(&mut self.items)
                .into_values()
                .flatten()
                .collect();
        }
        self.terminal = true;
        Ok(finished(parts, response.get("usage")))
    }
}

impl StreamDecoder for ResponseDecoder {
    fn push(&mut self, event: &SseEvent) -> Result<Vec<Block>, ProviderError> {
        let data = event.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(data).map_err(|error| {
            ProviderError::Stream(format!("Responses event is not JSON: {error}"))
        })?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let delta = || {
            value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let blocks = match kind {
            "response.output_text.delta" | "response.refusal.delta" => {
                vec![Block::Text(delta())]
            }
            "response.reasoning_summary_text.delta" => {
                let part = value
                    .get("summary_index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                let before = self.summaries.insert(index, part);
                let mut blocks = Vec::new();
                if before.is_some_and(|before| before != part) {
                    blocks.push(Block::Reasoning("\n\n".into()));
                }
                blocks.push(Block::Reasoning(delta()));
                blocks
            }
            "response.reasoning_text.delta" => vec![Block::Reasoning(delta())],
            "response.output_item.added" => {
                let item = value.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return Ok(Vec::new());
                }
                let field = |name: &str| item.get(name).and_then(Value::as_str).unwrap_or_default();
                let name = field("name").to_string();
                self.calls.insert(
                    index,
                    (field("call_id").to_string(), name.clone(), String::new()),
                );
                vec![Block::ToolStarted { name, target: None }]
            }
            "response.function_call_arguments.delta" => {
                let Some((id, name, arguments)) = self.calls.get_mut(&index) else {
                    return Ok(Vec::new());
                };
                let chunk = delta();
                arguments.push_str(&chunk);
                let mut blocks = vec![Block::ToolInput {
                    id: id.clone(),
                    name: name.clone(),
                    delta: chunk,
                }];
                if let Some(target) = sniff_target(arguments)
                    && self.targeted.insert(index)
                {
                    blocks.push(Block::ToolStarted {
                        name: name.clone(),
                        target: Some(target),
                    });
                }
                blocks
            }
            "response.output_item.done" => {
                let item = value.get("item").ok_or_else(|| {
                    ProviderError::Stream("Responses output_item.done omitted its item".into())
                })?;
                let parts = item_parts(item, &self.names, &self.origin)?;
                self.items.insert(index, parts);
                Vec::new()
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                let response = value.get("response").ok_or_else(|| {
                    ProviderError::Stream(format!("Responses {kind} omitted its response"))
                })?;
                self.complete(response)?
            }
            "error" => return Err(unfinished(&json!({"error": value}))),
            _ => Vec::new(),
        };
        Ok(blocks.into_iter().filter(|block| !matches!(block, Block::Text(text) | Block::Reasoning(text) if text.is_empty())).collect())
    }

    fn completed(&self) -> bool {
        self.terminal
    }

    fn ends_stream(&self, _event: &SseEvent) -> bool {
        self.terminal
    }

    fn parse_whole(&self, body: &str) -> Result<Vec<Block>, ProviderError> {
        let response: Value = serde_json::from_str(body).map_err(|error| {
            ProviderError::Decode(format!("Responses reply is not JSON: {error}"))
        })?;
        if response.get("status").and_then(Value::as_str) != Some("completed") {
            return Err(unfinished(&response));
        }
        let parts = output_parts(&response, &self.names, &self.origin)?;
        let mut blocks: Vec<Block> = parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Reasoning(part) => part.text.clone().map(Block::Reasoning),
                ContentPart::Text(part) => Some(Block::Text(part.text.clone())),
                _ => None,
            })
            .collect();
        blocks.extend(finished(parts, response.get("usage")));
        Ok(blocks)
    }

    fn finish(self) -> Result<Vec<Block>, ProviderError> {
        match self.terminal {
            true => Ok(Vec::new()),
            false => Err(ProviderError::Stream(
                "stream ended before response.completed; no tools were executed".into(),
            )),
        }
    }
}

impl Wire for Responses {
    fn prepare(
        &self,
        context: &CompiledContext,
        settings: &Settings<'_>,
    ) -> Result<(Value, Vec<(String, String)>), ProviderError> {
        let mut body = prepare_body(context, settings)?;
        let headers = crate::prompt_cache::apply(
            &mut body,
            crate::prompt_cache::route(settings.profile, settings.model),
            settings.profile.cache_ttl,
            settings.scope,
            settings.stable_system,
        );
        Ok((body, headers))
    }

    fn url(&self, base_url: &str) -> Result<String, ProviderError> {
        endpoint_url(base_url, "/responses")
    }

    fn read(
        &self,
        response: reqwest::Response,
        streaming: bool,
        idle: std::time::Duration,
        names: HashMap<String, String>,
        origin: Origin,
    ) -> BoxFuture<'static, Result<Reply, ProviderError>> {
        Box::pin(stream::read(
            response,
            streaming,
            idle,
            ResponseDecoder::new(names, origin),
        ))
    }

    fn set_output_limit(
        &self,
        body: &mut Map<String, Value>,
        tokens: u64,
        profile: &ProviderProfile,
    ) -> Result<bool, ProviderError> {
        if plan_route(profile) {
            return Ok(false);
        }
        body.insert("max_output_tokens".into(), json!(tokens));
        Ok(true)
    }

    fn set_reasoning(
        &self,
        body: &mut Value,
        config: &ReasoningConfig,
        profile: &ProviderProfile,
    ) -> Result<(), ProviderError> {
        let body = body.as_object_mut().ok_or_else(|| {
            ProviderError::Decode("prepared model request is not an object".into())
        })?;
        reasoning_fields(body, profile, config);
        Ok(())
    }

    fn check_reasoning(&self, _config: &ReasoningConfig) -> Result<(), ProviderError> {
        Ok(())
    }

    fn default_efforts(&self, _model: &str) -> &'static [ReasoningEffort] {
        ReasoningEffort::ALL
    }
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod tests;
