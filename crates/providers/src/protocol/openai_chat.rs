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
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<OutgoingToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
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
) -> Result<serde_json::Value, ProviderError> {
    let canonical_names: Vec<String> = context.tools.iter().map(|tool| tool.name.clone()).collect();
    let wire_to_canonical = wire_name_map(&canonical_names);
    let canonical_to_wire: HashMap<&str, &str> = wire_to_canonical
        .iter()
        .map(|(wire, canonical)| (canonical.as_str(), wire.as_str()))
        .collect();
    let messages = lower_chat_messages(&context.ordered_messages(), &canonical_to_wire)?;
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

/// Hoist all system content to the first wire message while preserving the
/// exact order of every non-system message and tool result.
#[cfg(test)]
pub(crate) fn build_chat_messages(
    messages: &[kernel::Message],
    canonical_to_wire: &HashMap<&str, &str>,
) -> Vec<ChatMessage> {
    let ordered: Vec<_> = messages.iter().map(kernel::Message::ordered).collect();
    lower_chat_messages(&ordered, canonical_to_wire)
        .expect("legacy messages are always representable as OpenAI Chat messages")
}

/// Lower ordered canonical history to the Chat Completions message shape.
///
/// Chat cannot represent arbitrary interleaving or replay state. We therefore
/// fail closed when lowering would destroy OpenAI-Chat-tagged state or part
/// order. State owned by another protocol is deliberately ignored: protocol
/// adapters may consume only state tagged for themselves.
fn lower_chat_messages(
    messages: &[ModelMessage],
    canonical_to_wire: &HashMap<&str, &str>,
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
            Role::User => rest.push(ChatMessage {
                role: "user",
                content: lower_text_only(&message.parts, "user")?,
                tool_calls: Vec::new(),
                tool_call_id: None,
            }),
            Role::Assistant => {
                rest.push(lower_assistant(&message.parts, canonical_to_wire)?);
            }
            Role::Tool => rest.extend(lower_tool_results(&message.parts)?),
        }
    }

    let mut out = Vec::with_capacity(rest.len() + 1);
    if !system.is_empty() {
        out.push(ChatMessage {
            role: "system",
            content: system,
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    out.extend(rest);
    Ok(out)
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
        content,
        tool_calls,
        tool_call_id: None,
    })
}

fn lower_tool_results(parts: &[ContentPart]) -> Result<Vec<ChatMessage>, ProviderError> {
    let has_result = parts
        .iter()
        .any(|part| matches!(part, ContentPart::ToolResult(_)));
    if !has_result {
        return Ok(vec![ChatMessage {
            role: "tool",
            content: lower_text_only(parts, "tool")?,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }]);
    }

    let mut messages = Vec::new();
    for part in parts {
        match part {
            ContentPart::ToolResult(part) => {
                check_replay_state(&part.provider_state)?;
                messages.push(ChatMessage {
                    role: "tool",
                    content: part.content.clone(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(part.tool_call_id.clone()),
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
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Construct the deterministic wire→canonical mapping used by both request
/// lowering and response decoding. Sanitization collisions receive suffixes.
pub(crate) fn wire_name_map(canonical: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for name in canonical {
        let mut wire = wire_tool_name(name);
        while map.contains_key(&wire) {
            wire.push('_');
        }
        map.insert(wire, name.clone());
    }
    map
}

/// vLLM's TokenizeChatRequest derives from the exact prepared generation body,
/// with generation-only fields removed.
pub(crate) fn vllm_tokenize_body(request: &PreparedModelRequest) -> serde_json::Value {
    let mut body = request.body.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("stream");
        object.remove("stream_options");
        object.remove("max_tokens");
        object.remove("reasoning_effort");
        object.insert("add_generation_prompt".into(), serde_json::json!(true));
    }
    body
}

// ── response decoding ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
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
}

#[derive(Default, Clone, Copy, Deserialize)]
struct UsageRaw {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
    #[serde(default)]
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
    #[serde(default)]
    choices: Vec<CompletionChoice>,
    #[serde(default)]
    usage: Option<UsageRaw>,
    #[serde(default)]
    error: Option<ResponseError>,
}

#[derive(Deserialize)]
struct CompletionChoice {
    #[serde(default)]
    message: CompletionMessage,
}

#[derive(Default, Deserialize)]
struct CompletionMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
    #[serde(default)]
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
}

impl ResponseDecoder {
    pub(crate) fn new(names: HashMap<String, String>) -> Self {
        Self {
            names,
            accum: BTreeMap::new(),
            target_announced: HashSet::new(),
            think_filter: ThinkTagFilter::default(),
        }
    }

    pub(crate) fn push(&mut self, event: &SseEvent) -> Result<Vec<Block>, ProviderError> {
        process_sse_event(
            event,
            &mut self.accum,
            &mut self.think_filter,
            &mut self.target_announced,
        )
    }

    /// Preserve visible text that the inline-thinking filter held while
    /// waiting to see whether a partial tag would complete.
    pub(crate) fn flush_pending(&mut self) -> Option<Block> {
        self.think_filter.flush()
    }

    pub(crate) fn finish(mut self) -> Vec<Block> {
        let mut blocks = Vec::new();
        if let Some(block) = self.think_filter.flush() {
            blocks.push(block);
        }
        blocks.extend(
            finalize_tool_calls(self.accum, &self.names)
                .into_iter()
                .map(Block::ToolIntent),
        );
        blocks
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
        return Err(ProviderError::Stream(error_message(error)));
    }

    let mut blocks = Vec::new();
    if let Some(choice) = parsed.choices.into_iter().next() {
        let message = choice.message;
        if let Some(reasoning) = message.reasoning_content.filter(|value| !value.is_empty()) {
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
            let Some(function) = tool_call.function else {
                continue;
            };
            let Some(name) = function.name.filter(|name| !name.is_empty()) else {
                continue;
            };
            let id = tool_call
                .id
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("call_{index}"));
            let arguments = match function.arguments {
                Some(arguments) if !arguments.trim().is_empty() => serde_json::from_str(&arguments)
                    .unwrap_or_else(|_| serde_json::json!({ "_raw": arguments })),
                _ => serde_json::json!({}),
            };
            blocks.push(Block::ToolIntent(ToolIntent {
                id,
                tool: names.get(&name).cloned().unwrap_or(name),
                args: repair_args(arguments),
            }));
        }
    }
    if let Some(usage) = parsed.usage {
        blocks.push(usage_block(usage));
    }
    Ok(blocks)
}

fn error_message(error: ResponseError) -> String {
    match (error.message, error.kind) {
        (Some(message), Some(kind)) => format!("{message} ({kind})"),
        (Some(message), None) => message,
        (None, Some(kind)) => kind,
        (None, None) => "provider returned an error frame".to_string(),
    }
}

fn usage_block(usage: UsageRaw) -> Block {
    Block::Usage(Usage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
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
    let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
        return Ok(blocks);
    };
    if let Some(error) = chunk.error {
        return Err(ProviderError::Stream(error_message(error)));
    }
    if let Some(usage) = chunk.usage {
        blocks.push(usage_block(usage));
    }
    if let Some(choice) = chunk.choices.into_iter().next() {
        if let Some(reasoning) = choice
            .delta
            .reasoning_content
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
                    entry.2.push_str(&arguments);
                    if !entry.1.is_empty() && !target_announced.contains(&index) {
                        if let Some(target) = sniff_target(&entry.2) {
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
    }
    Ok(blocks)
}

pub(crate) fn finalize_tool_calls(
    accum: BTreeMap<u32, (String, String, String)>,
    names: &HashMap<String, String>,
) -> Vec<ToolIntent> {
    let mut intents = Vec::new();
    for (index, (id, name, arguments)) in accum {
        if name.is_empty() {
            continue;
        }
        let id = if id.is_empty() {
            format!("call_{index}")
        } else {
            id
        };
        let arguments = if arguments.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&arguments)
                .unwrap_or_else(|_| serde_json::json!({ "_raw": arguments }))
        };
        intents.push(ToolIntent {
            id,
            tool: names.get(&name).cloned().unwrap_or(name),
            args: repair_args(arguments),
        });
    }
    intents
}

pub(crate) fn repair_args(arguments: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    if let Value::String(value) = &arguments {
        if let Ok(inner @ Value::Object(_)) = serde_json::from_str::<Value>(value) {
            return inner;
        }
        return arguments;
    }
    if let Value::Object(map) = &arguments {
        if map.len() == 1 {
            let (key, value) = map.iter().next().expect("length checked");
            if matches!(key.as_str(), "arguments" | "input" | "parameters" | "args") {
                match value {
                    Value::Object(_) => return value.clone(),
                    Value::String(encoded) => {
                        if let Ok(inner @ Value::Object(_)) = serde_json::from_str::<Value>(encoded)
                        {
                            return inner;
                        }
                    }
                    _ => {}
                }
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
mod tests {
    use super::*;
    use kernel::{
        BlastRadius, MediaPart, MediaSource, Message, ProviderState, TextPart, ToolCallPart,
        ToolCategory, ToolIntent, ToolResultPart, ToolSpec,
    };

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: format!("call {name}"),
            schema: serde_json::json!({"type": "object"}),
            blast_radius: BlastRadius::Read,
            category: ToolCategory::Read,
            icon: "t".into(),
        }
    }

    #[test]
    fn collisions_use_the_same_unique_names_in_definitions_and_history() {
        let context = CompiledContext {
            model: String::new(),
            messages: vec![Message::assistant_calls(
                "",
                vec![
                    ToolIntent {
                        id: "one".into(),
                        tool: "a.b".into(),
                        args: serde_json::json!({}),
                    },
                    ToolIntent {
                        id: "two".into(),
                        tool: "a_b".into(),
                        args: serde_json::json!({}),
                    },
                ],
            )],
            ordered: None,
            tools: vec![tool("a.b"), tool("a_b")],
        };

        let body = prepare_body(&context, "model", true, None, Some(1_000)).unwrap();
        let definitions: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        let history: Vec<&str> = body["messages"][0]["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call["function"]["name"].as_str().unwrap())
            .collect();

        assert_eq!(definitions.len(), 2);
        assert_ne!(definitions[0], definitions[1]);
        assert_eq!(history, definitions);
    }

    #[test]
    fn request_body_contains_only_portable_chat_fields() {
        let context = CompiledContext {
            model: String::new(),
            messages: vec![Message::user("hello")],
            ordered: None,
            tools: Vec::new(),
        };
        let body = prepare_body(&context, "model", true, Some("high"), Some(2_000)).unwrap();
        assert_eq!(body["model"], "model");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_tokens"], 2_000);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn ordered_bridge_keeps_legacy_chat_json_exactly_stable() {
        let legacy = vec![
            Message::system("system"),
            Message::user("question"),
            Message::assistant_calls(
                "checking",
                vec![ToolIntent {
                    id: "call-1".into(),
                    tool: "fs.read".into(),
                    args: serde_json::json!({"path": "README.md"}),
                }],
            ),
            Message::tool_result("call-1", "contents"),
        ];
        let names = HashMap::from([("fs.read", "fs_read")]);
        let compatibility = build_chat_messages(&legacy, &names);
        let ordered: Vec<_> = legacy.iter().map(Message::ordered).collect();
        let lowered = lower_chat_messages(&ordered, &names).unwrap();

        assert_eq!(
            serde_json::to_value(lowered).unwrap(),
            serde_json::to_value(compatibility).unwrap()
        );
    }

    #[test]
    fn ordered_media_is_rejected_until_chat_media_lowering_exists() {
        let messages = vec![ModelMessage {
            role: Role::User,
            parts: vec![ContentPart::Media(MediaPart {
                mime_type: "image/png".into(),
                source: MediaSource::Url("https://example.test/image.png".into()),
                provider_state: Vec::new(),
            })],
            trust: None,
        }];

        let error = lower_chat_messages(&messages, &HashMap::new())
            .err()
            .expect("media must be rejected");
        assert!(
            error
                .to_string()
                .contains("media lowering is not implemented")
        );
    }

    #[test]
    fn chat_rejects_its_own_replay_state_but_ignores_foreign_state() {
        let state = |protocol| ProviderState {
            protocol,
            kind: "opaque".into(),
            value: serde_json::json!({"secret": "state"}),
        };
        let own = vec![ModelMessage {
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart {
                text: "hello".into(),
                provider_state: vec![state(Protocol::OpenAiChat)],
            })],
            trust: None,
        }];
        let error = lower_chat_messages(&own, &HashMap::new())
            .err()
            .expect("own replay state must be rejected");
        assert!(error.to_string().contains("cannot replay provider state"));

        let foreign = vec![ModelMessage {
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart {
                text: "hello".into(),
                provider_state: vec![state(Protocol::AnthropicMessages)],
            })],
            trust: None,
        }];
        let lowered = lower_chat_messages(&foreign, &HashMap::new()).unwrap();
        assert_eq!(lowered[0].content, "hello");
    }

    #[test]
    fn multiple_ordered_tool_results_become_ordered_chat_messages() {
        let messages = vec![ModelMessage {
            role: Role::Tool,
            parts: vec![
                ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: "one".into(),
                    content: "first".into(),
                    provider_state: Vec::new(),
                }),
                ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: "two".into(),
                    content: "second".into(),
                    provider_state: Vec::new(),
                }),
            ],
            trust: None,
        }];

        let lowered = lower_chat_messages(&messages, &HashMap::new()).unwrap();
        assert_eq!(lowered.len(), 2);
        assert_eq!(lowered[0].tool_call_id.as_deref(), Some("one"));
        assert_eq!(lowered[1].tool_call_id.as_deref(), Some("two"));
    }

    #[test]
    fn assistant_interleaving_that_chat_cannot_express_fails_closed() {
        let messages = vec![ModelMessage {
            role: Role::Assistant,
            parts: vec![
                ContentPart::ToolCall(ToolCallPart {
                    id: "call".into(),
                    tool: "fs.read".into(),
                    args: serde_json::json!({}),
                    provider_state: Vec::new(),
                }),
                ContentPart::Text(TextPart {
                    text: "after the call".into(),
                    provider_state: Vec::new(),
                }),
            ],
            trust: None,
        }];

        let error = lower_chat_messages(&messages, &HashMap::new())
            .err()
            .expect("unrepresentable interleaving must be rejected");
        assert!(error.to_string().contains("text after a tool call"));
    }
}
