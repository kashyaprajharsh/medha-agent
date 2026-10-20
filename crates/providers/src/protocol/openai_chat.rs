//! OpenAI Chat Completions request lowering.
//!
//! This module owns the protocol's wire JSON. Deployment concerns such as the
//! base URL, credential, HTTP client, and selected optional counter remain in
//! the provider client.

use crate::transport::sse::SseEvent;
use kernel::{
    Block, CompiledContext, ContentPart, ModelMessage, PreparedModelRequest, Protocol,
    ProviderError, ProviderState, Role, ToolIntent, Usage,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
pub(crate) struct ChatMessage {
    pub role: &'static str,
    pub content: serde_json::Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<OutgoingToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// An assistant turn's own thinking, as it was stored. Never sent as it is.
    #[serde(skip)]
    pub thinking: String,
    /// The thinking sent back, set only for a server that requires it; any
    /// other server refuses a request that carries this field at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct OutgoingToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    pub function: OutgoingFunction,
}

#[derive(Serialize)]
pub(crate) struct OutgoingFunction {
    pub name: String,
    /// OpenAI requires arguments to be a JSON string, not an object.
    arguments: String,
}

#[derive(Serialize)]
struct ToolDefinition {
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionDefinition,
}

#[derive(Serialize)]
struct FunctionDefinition {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

pub(crate) fn prepare_body(
    context: &CompiledContext,
    model: &str,
    streaming: bool,
    reasoning_effort: Option<&'static str>,
    max_tokens: Option<u64>,
    echo_thinking: bool,
) -> Result<serde_json::Value, ProviderError> {
    let canonical_names: Vec<String> = context.tools.iter().map(|tool| tool.name.clone()).collect();
    let wire_to_canonical = wire_name_map(&canonical_names);
    let canonical_to_wire: HashMap<&str, &str> = wire_to_canonical
        .iter()
        .map(|(wire, canonical)| (canonical.as_str(), wire.as_str()))
        .collect();
    let ordered = context.ordered_messages();
    let messages = lower_chat_messages(&ordered, &canonical_to_wire, echo_thinking)?;
    let tools = context
        .tools
        .iter()
        .map(|tool| ToolDefinition {
            kind: "function",
            function: FunctionDefinition {
                name: canonical_to_wire
                    .get(tool.name.as_str())
                    .copied()
                    .unwrap_or(tool.name.as_str())
                    .to_string(),
                description: tool.description.clone(),
                parameters: tool.schema.clone(),
            },
        })
        .collect();

    serde_json::to_value(ChatRequest {
        model: model.to_string(),
        messages,
        stream: streaming,
        tools,
        stream_options: streaming.then_some(StreamOptions {
            include_usage: true,
        }),
        reasoning_effort,
        max_tokens,
    })
    .map_err(|error| ProviderError::Decode(format!("request encoding: {error}")))
}

/// Fails rather than drop Chat-tagged state or part order. With `echo`, each assistant
/// turn carries its thinking, a space when none: such servers refuse an empty field.
pub(crate) fn lower_chat_messages(
    messages: &[ModelMessage],
    canonical_to_wire: &HashMap<&str, &str>,
    echo: bool,
) -> Result<Vec<ChatMessage>, ProviderError> {
    let mut system = String::new();
    let mut rest = Vec::with_capacity(messages.len());
    for message in messages {
        if message.role == Role::System {
            let content = lower_text_only(&message.parts, "system")?;
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(&content);
            continue;
        }
        match message.role {
            Role::System => unreachable!("system messages were handled above"),
            Role::User => push_user(&mut rest, lower_user(&message.parts)?),
            Role::Assistant => {
                let mut next = lower_assistant(&message.parts, canonical_to_wire)?;
                if echo {
                    let thought = std::mem::take(&mut next.thinking);
                    next.reasoning_content = Some(match thought.is_empty() {
                        true => " ".into(),
                        false => thought,
                    });
                }
                // A turn with no words and no action is not sent, thinking or
                // not: servers refuse an empty turn, and one that ends a request.
                let said = next
                    .content
                    .as_str()
                    .is_none_or(|text| !text.trim().is_empty());
                if said || !next.tool_calls.is_empty() {
                    push_assistant(&mut rest, next);
                }
            }
            Role::Tool => rest.extend(lower_tool_results(&message.parts)?),
        }
    }

    let mut out = Vec::with_capacity(rest.len() + 1);
    if !system.is_empty() {
        out.push(ChatMessage {
            role: "system",
            content: system.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            thinking: String::new(),
            reasoning_content: None,
        });
    }
    out.extend(rest);
    Ok(out)
}

/// Add an assistant message, folded into the one before it when that is the
/// assistant's too: its text on a new line, its tool calls after the others.
/// Strict chat templates refuse two assistant messages in a row as well.
fn push_assistant(messages: &mut Vec<ChatMessage>, next: ChatMessage) {
    let before = messages.last_mut().filter(|last| last.role == "assistant");
    let (Some(before), serde_json::Value::String(more)) = (before, &next.content) else {
        return messages.push(next);
    };
    let serde_json::Value::String(text) = &mut before.content else {
        return messages.push(next);
    };
    if !text.trim().is_empty() && !more.trim().is_empty() {
        text.push('\n');
    }
    text.push_str(more);
    before.tool_calls.extend(next.tool_calls);
    // The earlier turn's thinking stands; a later one fills only a blank.
    let blank = |held: &Option<String>| held.as_deref().is_none_or(|held| held.trim().is_empty());
    if blank(&before.reasoning_content) && !blank(&next.reasoning_content) {
        before.reasoning_content = next.reasoning_content;
    }
}

/// Add a user message, joined to the one before it when both are plain text.
/// Strict chat templates refuse two user messages in a row.
fn push_user(messages: &mut Vec<ChatMessage>, content: serde_json::Value) {
    let before = messages.last_mut().filter(|last| last.role == "user");
    if let (Some(serde_json::Value::String(before)), serde_json::Value::String(next)) =
        (before.map(|last| &mut last.content), &content)
    {
        if !before.is_empty() && !next.is_empty() {
            before.push_str("\n\n");
        }
        before.push_str(next);
        return;
    }
    messages.push(ChatMessage {
        role: "user",
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        thinking: String::new(),
        reasoning_content: None,
    });
}

fn check_replay_state(states: &[ProviderState]) -> Result<(), ProviderError> {
    if let Some(state) = states
        .iter()
        .find(|state| state.protocol == Protocol::OpenAiChat)
    {
        return Err(ProviderError::Decode(format!(
            "open-ai-chat cannot replay provider state of kind '{}'",
            state.kind
        )));
    }
    Ok(())
}

fn lower_user(parts: &[ContentPart]) -> Result<serde_json::Value, ProviderError> {
    use kernel::MediaSource;
    use serde_json::json;
    if !parts
        .iter()
        .any(|part| matches!(part, ContentPart::Media(_)))
    {
        return lower_text_only(parts, "user").map(Into::into);
    }
    let mut content = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text(text) => {
                check_replay_state(&text.provider_state)?;
                content.push(json!({"type": "text", "text": text.text}));
            }
            ContentPart::Media(media) => {
                check_replay_state(&media.provider_state)?;
                if !matches!(
                    media.mime_type.as_str(),
                    "image/png" | "image/jpeg" | "image/webp" | "image/gif"
                ) {
                    return Err(ProviderError::Decode(
                        "open-ai-chat user images require PNG, JPEG, WebP, or GIF".into(),
                    ));
                }
                let url = match &media.source {
                    MediaSource::Url(url)
                        if url.starts_with("https://") || url.starts_with("http://") =>
                    {
                        url.clone()
                    }
                    MediaSource::Base64(data) if !data.is_empty() => {
                        format!("data:{};base64,{data}", media.mime_type)
                    }
                    MediaSource::Artifact(_) => {
                        return Err(ProviderError::Decode(
                            "image artifact must be resolved before provider encoding".into(),
                        ));
                    }
                    _ => return Err(ProviderError::Decode("invalid image source".into())),
                };
                content.push(json!({"type": "image_url", "image_url": {"url": url}}));
            }
            _ => {
                lower_text_only(std::slice::from_ref(part), "user")?;
            }
        }
    }
    Ok(content.into())
}

fn lower_text_only(parts: &[ContentPart], role: &str) -> Result<String, ProviderError> {
    let mut content = String::new();
    for part in parts {
        match part {
            ContentPart::Text(part) => {
                check_replay_state(&part.provider_state)?;
                content.push_str(&part.text);
            }
            ContentPart::Reasoning(part) => {
                check_replay_state(&part.provider_state)?;
            }
            ContentPart::Media(part) => {
                check_replay_state(&part.provider_state)?;
                return Err(ProviderError::Decode(format!(
                    "open-ai-chat media lowering is not implemented for {role} messages"
                )));
            }
            ContentPart::ToolCall(part) => {
                check_replay_state(&part.provider_state)?;
                return Err(ProviderError::Decode(format!(
                    "open-ai-chat cannot place a tool call in a {role} message"
                )));
            }
            ContentPart::ToolResult(part) => {
                check_replay_state(&part.provider_state)?;
                return Err(ProviderError::Decode(format!(
                    "open-ai-chat cannot place a tool result in a {role} message"
                )));
            }
        }
    }
    Ok(content)
}

fn lower_assistant(
    parts: &[ContentPart],
    canonical_to_wire: &HashMap<&str, &str>,
) -> Result<ChatMessage, ProviderError> {
    let mut content = String::new();
    let mut thinking = String::new();
    let mut tool_calls = Vec::new();
    let mut saw_tool_call = false;
    for part in parts {
        match part {
            ContentPart::Text(part) => {
                check_replay_state(&part.provider_state)?;
                if saw_tool_call {
                    return Err(ProviderError::Decode(
                        "open-ai-chat cannot preserve assistant text after a tool call".into(),
                    ));
                }
                content.push_str(&part.text);
            }
            ContentPart::ToolCall(part) => {
                check_replay_state(&part.provider_state)?;
                saw_tool_call = true;
                tool_calls.push(OutgoingToolCall {
                    id: part.id.clone(),
                    kind: "function",
                    function: OutgoingFunction {
                        name: canonical_to_wire
                            .get(part.tool.as_str())
                            .copied()
                            .map(str::to_string)
                            .unwrap_or_else(|| wire_tool_name(&part.tool)),
                        arguments: part.args.to_string(),
                    },
                });
            }
            ContentPart::Reasoning(part) => {
                check_replay_state(&part.provider_state)?;
                thinking.push_str(part.text.as_deref().unwrap_or_default());
            }
            ContentPart::Media(part) => {
                check_replay_state(&part.provider_state)?;
                return Err(ProviderError::Decode(
                    "open-ai-chat media lowering is not implemented for assistant messages".into(),
                ));
            }
            ContentPart::ToolResult(part) => {
                check_replay_state(&part.provider_state)?;
                return Err(ProviderError::Decode(
                    "open-ai-chat cannot place a tool result in an assistant message".into(),
                ));
            }
        }
    }
    Ok(ChatMessage {
        role: "assistant",
        content: content.into(),
        tool_calls,
        tool_call_id: None,
        thinking,
        reasoning_content: None,
    })
}

fn lower_tool_results(parts: &[ContentPart]) -> Result<Vec<ChatMessage>, ProviderError> {
    let has_result = parts
        .iter()
        .any(|part| matches!(part, ContentPart::ToolResult(_)));
    if !has_result {
        return Ok(vec![ChatMessage {
            role: "tool",
            content: lower_text_only(parts, "tool")?.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            thinking: String::new(),
            reasoning_content: None,
        }]);
    }

    let mut messages = Vec::new();
    for part in parts {
        match part {
            ContentPart::ToolResult(part) => {
                check_replay_state(&part.provider_state)?;
                messages.push(ChatMessage {
                    role: "tool",
                    content: part.content.clone().into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(part.tool_call_id.clone()),
                    thinking: String::new(),
                    reasoning_content: None,
                });
            }
            ContentPart::Reasoning(part) => check_replay_state(&part.provider_state)?,
            ContentPart::Text(part) => {
                check_replay_state(&part.provider_state)?;
                return Err(ProviderError::Decode(
                    "open-ai-chat cannot preserve text mixed with tool results".into(),
                ));
            }
            ContentPart::Media(part) => {
                check_replay_state(&part.provider_state)?;
                return Err(ProviderError::Decode(
                    "open-ai-chat media lowering is not implemented for tool messages".into(),
                ));
            }
            ContentPart::ToolCall(part) => {
                check_replay_state(&part.provider_state)?;
                return Err(ProviderError::Decode(
                    "open-ai-chat cannot place a tool call in a tool message".into(),
                ));
            }
        }
    }
    Ok(messages)
}

pub(crate) fn wire_tool_name(name: &str) -> String {
    kernel::portable_tool_name(name)
}

/// Construct the deterministic wire→canonical mapping used by both request
/// lowering and response decoding. Sanitization collisions receive suffixes.
pub(crate) fn wire_name_map(canonical: &[String]) -> HashMap<String, String> {
    kernel::portable_tool_name_map(canonical)
}

/// vLLM's TokenizeChatRequest derives from the exact prepared generation body,
/// with generation-only fields removed.
pub(crate) fn vllm_tokenize_body(request: &PreparedModelRequest) -> serde_json::Value {
    let mut body = request.body.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("stream");
        object.remove("stream_options");
        object.remove("max_tokens");
        object.remove("max_completion_tokens");
        object.remove("reasoning_effort");
        object.insert("add_generation_prompt".into(), serde_json::json!(true));
    }
    body
}

/// Accept an explicitly `null` field as the type's default. `#[serde(default)]`
/// alone only covers an *absent* key, so a server that serialises its whole
/// schema and spells empties as `null` (mlx-vlm, some llama.cpp builds) would
/// otherwise fail the whole response with "invalid type: null".
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default, deserialize_with = "null_as_default")]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<UsageRaw>,
    /// Some compatible gateways report a failure as a final JSON SSE frame
    /// instead of an unsuccessful HTTP response.
    #[serde(default)]
    error: Option<ResponseError>,
}

#[derive(Deserialize)]
struct ResponseError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    code: Option<serde_json::Value>,
}

#[derive(Default, Clone, Copy, Deserialize)]
struct UsageRaw {
    /// Inclusive of anything served from cache: on every endpoint that reports
    /// the split, this equals hit + miss.
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
    /// The OpenAI-compatible spelling of the cache-hit count.
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// DeepSeek's own spelling of the same number.
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u32>,
    /// The spellings proxies that route Claude or Kimi pass through unchanged.
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_write_tokens: Option<u32>,
}

#[derive(Default, Clone, Copy, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    cache_write_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
}

impl UsageRaw {
    /// Absent unless the route actually reported it. A zero would read as a
    /// measured miss, so every endpoint that does not report the bucket would
    /// look like one that never caches.
    fn cached_prompt_tokens(&self) -> Option<u32> {
        self.prompt_tokens_details
            .and_then(|details| details.cached_tokens)
            .or(self.cache_read_input_tokens)
            .or(self.prompt_cache_hit_tokens)
            .or(self.cached_tokens)
            // A gateway reporting more hits than prompt tokens is reporting
            // something else; keep the count inside the prompt it belongs to.
            .map(|cached| cached.min(self.prompt_tokens))
    }

    /// What the server stored for later re-reading, under any of its spellings.
    fn cache_write_tokens(&self) -> Option<u32> {
        let details = self.prompt_tokens_details.unwrap_or_default();
        details
            .cache_write_tokens
            .or(details.cache_creation_input_tokens)
            .or(self.cache_creation_input_tokens)
            .or(self.cache_write_tokens)
            .map(|written| written.min(self.prompt_tokens))
    }
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    delta: Delta,
}

#[derive(Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    refusal: Option<serde_json::Value>,
    /// Two wire spellings of the same field. Held separately rather than as one
    /// `alias`, because a server that sends *both* makes `alias` report a
    /// duplicate field and reject the response. `reasoning_content` wins when
    /// both carry text: it is the canonical name.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    tool_calls: Vec<DeltaToolCall>,
}

#[derive(Deserialize)]
struct DeltaToolCall {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFn>,
}

#[derive(Deserialize)]
struct DeltaFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ChatCompletion {
    #[serde(default, deserialize_with = "null_as_default")]
    choices: Vec<CompletionChoice>,
    #[serde(default)]
    usage: Option<UsageRaw>,
    #[serde(default)]
    error: Option<ResponseError>,
}

#[derive(Deserialize)]
struct CompletionChoice {
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    message: CompletionMessage,
}

#[derive(Default, Deserialize)]
struct CompletionMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    refusal: Option<serde_json::Value>,
    /// See [`Delta`] for why these are two fields and not one `alias`.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    tool_calls: Vec<CompletionToolCall>,
}

#[derive(Deserialize)]
struct CompletionToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFn>,
}

/// Stateful OpenAI Chat decoder. The transport owns byte and SSE record
/// boundaries; this type owns only protocol JSON and cross-event assembly.
pub(crate) struct ResponseDecoder {
    names: HashMap<String, String>,
    accum: BTreeMap<u32, (String, String, String)>,
    target_announced: HashSet<u32>,
    think_filter: ThinkTagFilter,
    completed: bool,
}

impl ResponseDecoder {
    pub(crate) fn new(names: HashMap<String, String>) -> Self {
        Self {
            names,
            accum: BTreeMap::new(),
            target_announced: HashSet::new(),
            think_filter: ThinkTagFilter::default(),
            completed: false,
        }
    }

    pub(crate) fn push(&mut self, event: &SseEvent) -> Result<Vec<Block>, ProviderError> {
        let blocks = process_sse_event(
            event,
            &mut self.accum,
            &mut self.think_filter,
            &mut self.target_announced,
        )?;
        if event.data.trim() == "[DONE]" {
            self.completed = true;
        } else if let Ok(chunk) = serde_json::from_str::<StreamChunk>(&event.data)
            && let Some(choice) = chunk.choices.first()
        {
            self.completed |= choice.finish_reason.is_some();
        }
        Ok(blocks)
    }

    pub(crate) fn completed(&self) -> bool {
        self.completed
    }

    /// Preserve visible text that the inline-thinking filter held while
    /// waiting to see whether a partial tag would complete.
    pub(crate) fn flush_pending(&mut self) -> Option<Block> {
        self.think_filter.flush()
    }

    pub(crate) fn finish(mut self) -> Result<Vec<Block>, ProviderError> {
        let mut blocks = Vec::new();
        if let Some(block) = self.think_filter.flush() {
            blocks.push(block);
        }
        if !self.completed {
            return Err(ProviderError::Stream(
                "stream ended before completing the model response; no tools were executed".into(),
            ));
        }
        let intents = finalize_tool_calls(self.accum, &self.names)?;
        blocks.extend(intents.into_iter().map(Block::ToolIntent));
        Ok(blocks)
    }
}

/// Parse a non-streamed completion into the same canonical block order as the
/// streaming decoder: reasoning, text, tool calls, then usage.
pub(crate) fn parse_completion(
    body: &str,
    names: &HashMap<String, String>,
) -> Result<Vec<Block>, ProviderError> {
    let parsed: ChatCompletion = serde_json::from_str(body)
        .map_err(|error| ProviderError::Stream(format!("non-streaming response parse: {error}")))?;
    if let Some(error) = parsed.error {
        return Err(ProviderError::Response(error_message(error)));
    }
    if parsed.choices.is_empty() {
        return Err(ProviderError::Stream(
            "completion omitted its response choice".into(),
        ));
    }

    let mut blocks = Vec::new();
    if let Some(choice) = parsed.choices.into_iter().next() {
        validate_finish_reason(choice.finish_reason.as_deref())?;
        let message = choice.message;
        validate_refusal(message.refusal.as_ref())?;
        if let Some(reasoning) = message
            .reasoning_content
            .or(message.reasoning)
            .filter(|value| !value.is_empty())
        {
            blocks.push(Block::Reasoning(reasoning));
        }
        if let Some(content) = message.content.filter(|value| !value.is_empty()) {
            let mut filter = ThinkTagFilter::default();
            blocks.extend(filter.feed(&content));
            if let Some(block) = filter.flush() {
                blocks.push(block);
            }
        }
        for (index, tool_call) in message.tool_calls.into_iter().enumerate() {
            let id = tool_call
                .id
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("call_{index}"));
            let function = tool_call.function.ok_or_else(|| {
                ProviderError::invalid_tool_call("<unknown>", &id, "missing function definition")
            })?;
            let name = function
                .name
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| {
                    ProviderError::invalid_tool_call("<unknown>", &id, "missing tool name")
                })?;
            let arguments = parse_tool_arguments(
                &name,
                &id,
                function.arguments.as_deref().unwrap_or_default(),
            )?;
            blocks.push(Block::ToolIntent(ToolIntent {
                id,
                tool: names.get(&name).cloned().unwrap_or(name),
                args: arguments,
            }));
        }
    }
    if let Some(usage) = parsed.usage {
        blocks.push(usage_block(usage));
    }
    Ok(blocks)
}

fn error_message(error: ResponseError) -> String {
    let message = match (error.message, error.kind) {
        (Some(message), Some(kind)) => format!("{message} ({kind})"),
        (Some(message), None) => message,
        (None, Some(kind)) => kind,
        (None, None) => "provider returned an error frame".to_string(),
    };
    match error.code.filter(|code| !code.is_null()) {
        Some(code) => format!("{message} (code: {code})"),
        None => message,
    }
}

fn usage_block(usage: UsageRaw) -> Block {
    Block::Usage(Usage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
        cached_prompt_tokens: usage.cached_prompt_tokens(),
        cache_write_tokens: usage.cache_write_tokens(),
    })
}

pub(crate) fn process_sse_event(
    event: &SseEvent,
    accum: &mut BTreeMap<u32, (String, String, String)>,
    think_filter: &mut ThinkTagFilter,
    target_announced: &mut HashSet<u32>,
) -> Result<Vec<Block>, ProviderError> {
    let mut blocks = Vec::new();
    let data = event.data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(blocks);
    }
    let chunk: StreamChunk = serde_json::from_str(data).map_err(|error| {
        ProviderError::Garbled(format!(
            "the server sent part of its reply in a form that could not be read: {error}"
        ))
    })?;
    if let Some(error) = chunk.error {
        return Err(ProviderError::Response(error_message(error)));
    }
    if let Some(usage) = chunk.usage {
        blocks.push(usage_block(usage));
    }
    if let Some(choice) = chunk.choices.into_iter().next() {
        validate_finish_reason(choice.finish_reason.as_deref())?;
        validate_refusal(choice.delta.refusal.as_ref())?;
        if let Some(reasoning) = choice
            .delta
            .reasoning_content
            .or(choice.delta.reasoning)
            .filter(|value| !value.is_empty())
        {
            blocks.push(Block::Reasoning(reasoning));
        }
        if let Some(content) = choice.delta.content.filter(|value| !value.is_empty()) {
            blocks.extend(think_filter.feed(&content));
        }
        for tool_call in choice.delta.tool_calls {
            let index = tool_call.index;
            let entry = accum.entry(index).or_default();
            if let Some(id) = tool_call.id.filter(|id| !id.is_empty()) {
                entry.0 = id;
            }
            if let Some(function) = tool_call.function {
                if let Some(name) = function.name.filter(|name| !name.is_empty()) {
                    let first = entry.1.is_empty();
                    entry.1 = name.clone();
                    if first {
                        blocks.push(Block::ToolStarted { name, target: None });
                    }
                }
                if let Some(arguments) = function.arguments {
                    // Under the id the finished call will have, so a surface can
                    // tie what it drew from these pieces to the call that follows.
                    if !entry.1.is_empty() && !arguments.is_empty() {
                        blocks.push(Block::ToolInput {
                            id: call_id(&entry.0, index),
                            name: entry.1.clone(),
                            delta: arguments.clone(),
                        });
                    }
                    entry.2.push_str(&arguments);
                    if !entry.1.is_empty()
                        && !target_announced.contains(&index)
                        && let Some(target) = sniff_target(&entry.2)
                    {
                        target_announced.insert(index);
                        blocks.push(Block::ToolStarted {
                            name: entry.1.clone(),
                            target: Some(target),
                        });
                    }
                }
            }
        }
    }
    Ok(blocks)
}

/// The id a streamed call is known by: the provider's, or its place in the reply.
fn call_id(id: &str, index: u32) -> String {
    if id.is_empty() {
        format!("call_{index}")
    } else {
        id.to_string()
    }
}

pub(crate) fn finalize_tool_calls(
    accum: BTreeMap<u32, (String, String, String)>,
    names: &HashMap<String, String>,
) -> Result<Vec<ToolIntent>, ProviderError> {
    let mut intents = Vec::new();
    for (index, (id, name, arguments)) in accum {
        let id = call_id(&id, index);
        if name.trim().is_empty() {
            return Err(ProviderError::invalid_tool_call(
                "<unknown>",
                &id,
                "missing tool name",
            ));
        }
        let arguments = parse_tool_arguments(&name, &id, &arguments)?;
        intents.push(ToolIntent {
            id,
            tool: names.get(&name).cloned().unwrap_or(name),
            args: arguments,
        });
    }
    Ok(intents)
}

fn validate_refusal(refusal: Option<&serde_json::Value>) -> Result<(), ProviderError> {
    if let Some(refusal) = refusal.filter(|value| !value.is_null() && value.as_str() != Some("")) {
        return Err(ProviderError::Refused(format!(
            "the model declined to answer: {refusal}"
        )));
    }
    Ok(())
}

fn validate_finish_reason(reason: Option<&str>) -> Result<(), ProviderError> {
    match reason {
        Some("length" | "max_tokens") => Err(ProviderError::Truncated),
        Some("content_filter") => Err(ProviderError::Refused(
            "the provider's content filter stopped the reply; no tools from it were run".into(),
        )),
        _ => Ok(()),
    }
}

fn parse_tool_arguments(
    name: &str,
    id: &str,
    arguments: &str,
) -> Result<serde_json::Value, ProviderError> {
    let value = if arguments.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(arguments).map_err(|error| {
            ProviderError::invalid_tool_call(
                name,
                id,
                format!("invalid or incomplete JSON arguments: {error}"),
            )
        })?
    };
    super::validate_tool_arguments(name, id, repair_args(value))
}

pub(crate) fn repair_args(arguments: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    if let Value::String(value) = &arguments {
        if let Ok(inner @ Value::Object(_)) = serde_json::from_str::<Value>(value) {
            return inner;
        }
        return arguments;
    }
    if let Value::Object(map) = &arguments
        && map.len() == 1
    {
        let (key, value) = map.iter().next().expect("length checked");
        if matches!(key.as_str(), "arguments" | "input" | "parameters" | "args") {
            match value {
                Value::Object(_) => return value.clone(),
                Value::String(encoded) => {
                    if let Ok(inner @ Value::Object(_)) = serde_json::from_str::<Value>(encoded) {
                        return inner;
                    }
                }
                _ => {}
            }
        }
    }
    arguments
}

pub(crate) fn sniff_target(arguments: &str) -> Option<String> {
    for key in ["\"path\"", "\"file_path\"", "\"command\""] {
        if let Some(index) = arguments.find(key) {
            let rest = arguments[index + key.len()..].trim_start();
            let Some(rest) = rest.strip_prefix(':') else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix('"') else {
                continue;
            };
            let mut value = String::new();
            let mut chars = rest.chars();
            let mut closed = false;
            while let Some(character) = chars.next() {
                match character {
                    '\\' => match chars.next() {
                        Some('n') | Some('t') => value.push(' '),
                        Some(escaped) => value.push(escaped),
                        None => break,
                    },
                    '"' => {
                        closed = true;
                        break;
                    }
                    character => value.push(character),
                }
            }
            if closed && !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

#[derive(Default)]
pub(crate) struct ThinkTagFilter {
    buf: String,
    in_think: bool,
    line_has_content: bool,
}

const OPEN_TAG: &str = "<think>";
const CLOSE_TAG: &str = "</think>";

impl ThinkTagFilter {
    fn note_emitted(&mut self, value: &str) {
        match value.rfind('\n') {
            Some(index) => self.line_has_content = !value[index + 1..].trim().is_empty(),
            None => self.line_has_content = self.line_has_content || !value.trim().is_empty(),
        }
    }

    fn at_line_start(&self, prefix: &str) -> bool {
        match prefix.rfind('\n') {
            Some(index) => prefix[index + 1..].trim().is_empty(),
            None => !self.line_has_content && prefix.trim().is_empty(),
        }
    }

    pub(crate) fn feed(&mut self, chunk: &str) -> Vec<Block> {
        self.buf.push_str(chunk);
        let mut blocks = Vec::new();
        loop {
            if self.in_think {
                match self.buf.find(CLOSE_TAG) {
                    Some(position) => {
                        let head = self.buf[..position].to_string();
                        self.buf = self.buf[position + CLOSE_TAG.len()..].to_string();
                        if !head.is_empty() {
                            blocks.push(Block::Reasoning(head));
                        }
                        self.in_think = false;
                        self.line_has_content = false;
                    }
                    None => {
                        self.hold_margin(&mut blocks);
                        break;
                    }
                }
            } else {
                let opener = self
                    .buf
                    .match_indices(OPEN_TAG)
                    .map(|(position, _)| position)
                    .find(|position| self.at_line_start(&self.buf[..*position]));
                match opener {
                    Some(position) => {
                        let head = self.buf[..position].to_string();
                        self.buf = self.buf[position + OPEN_TAG.len()..].to_string();
                        if !head.is_empty() {
                            self.note_emitted(&head);
                            blocks.push(Block::Text(head));
                        }
                        self.in_think = true;
                    }
                    None => {
                        self.hold_margin(&mut blocks);
                        break;
                    }
                }
            }
        }
        blocks
    }

    fn hold_margin(&mut self, blocks: &mut Vec<Block>) {
        let margin = CLOSE_TAG.len().max(OPEN_TAG.len()) - 1;
        if self.buf.len() <= margin {
            return;
        }
        let split = self.buf.len() - margin;
        let split = (0..=split)
            .rev()
            .find(|index| self.buf.is_char_boundary(*index))
            .unwrap_or(0);
        let emitted = self.buf[..split].to_string();
        self.buf = self.buf[split..].to_string();
        if emitted.is_empty() {
            return;
        }
        if self.in_think {
            blocks.push(Block::Reasoning(emitted));
        } else {
            self.note_emitted(&emitted);
            blocks.push(Block::Text(emitted));
        }
    }

    pub(crate) fn flush(&mut self) -> Option<Block> {
        if self.buf.is_empty() {
            return None;
        }
        let rest = std::mem::take(&mut self.buf);
        Some(if self.in_think {
            Block::Reasoning(rest)
        } else {
            Block::Text(rest)
        })
    }
}

#[cfg(test)]
#[path = "openai_chat_tests.rs"]
mod tests;
