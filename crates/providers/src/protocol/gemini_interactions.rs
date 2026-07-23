//! Google Gemini Interactions API v1 wire contract.
//!
//! This adapter is stateless (`store: false`): Medha sends complete ordered
//! history and must therefore replay every Gemini thought signature unchanged.

use crate::protocol::openai_chat::{wire_name_map, wire_tool_name};
use crate::transport::sse::SseEvent;
use kernel::{
    Block, CompiledContext, ContentPart, ModelMessage, Protocol, ProviderError, ProviderState,
    ReasoningConfig, ReasoningEffort, Role, ToolIntent, Usage,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

const THOUGHT_SIGNATURE: &str = "thought_signature";

#[derive(Serialize)]
struct Request {
    model: String,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    stream: bool,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
}

#[derive(Serialize)]
struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_summaries: Option<&'static str>,
}

pub(crate) fn prepare_body(
    context: &CompiledContext,
    model: &str,
    streaming: bool,
    reasoning: &ReasoningConfig,
    max_output_tokens: Option<u64>,
) -> Result<(Value, HashMap<String, String>), ProviderError> {
    let canonical_names: Vec<String> = context.tools.iter().map(|tool| tool.name.clone()).collect();
    let wire_to_canonical = wire_name_map(&canonical_names);
    let canonical_to_wire: HashMap<&str, &str> = wire_to_canonical
        .iter()
        .map(|(wire, canonical)| (canonical.as_str(), wire.as_str()))
        .collect();

    let (system_instruction, input) =
        lower_messages(&context.ordered_messages(), &canonical_to_wire)?;
    let tools = context
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": canonical_to_wire
                    .get(tool.name.as_str())
                    .copied()
                    .unwrap_or(tool.name.as_str()),
                "description": tool.description,
                "parameters": tool.schema,
            })
        })
        .collect();
    let generation_config = generation_config(reasoning, max_output_tokens)?;
    let body = serde_json::to_value(Request {
        model: model.to_string(),
        input,
        system_instruction,
        tools,
        stream: streaming,
        store: false,
        generation_config,
    })
    .map_err(|error| ProviderError::Decode(format!("Gemini request encoding: {error}")))?;
    Ok((body, wire_to_canonical))
}

fn generation_config(
    reasoning: &ReasoningConfig,
    max_output_tokens: Option<u64>,
) -> Result<Option<GenerationConfig>, ProviderError> {
    if reasoning.enabled == Some(false) {
        return Err(ProviderError::Decode(
            "gemini-interactions v1 has no portable thinking-disable control".into(),
        ));
    }
    let thinking_level = reasoning.effort.map(|effort| match effort {
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
    });
    let thinking_summaries =
        (reasoning.enabled == Some(true) || thinking_level.is_some()).then_some("auto");
    if max_output_tokens.is_none() && thinking_level.is_none() && thinking_summaries.is_none() {
        return Ok(None);
    }
    Ok(Some(GenerationConfig {
        max_output_tokens,
        thinking_level,
        thinking_summaries,
    }))
}

fn lower_messages(
    messages: &[ModelMessage],
    canonical_to_wire: &HashMap<&str, &str>,
) -> Result<(Option<String>, Vec<Value>), ProviderError> {
    let mut system = String::new();
    let mut steps = Vec::new();
    // Stateless Gemini replay requires a function result to identify both the
    // original call and its function. Canonical tool-result messages carry the
    // call id, so retain the wire name while lowering the preceding call.
    let mut function_names = HashMap::new();
    for message in messages {
        if message.role == Role::System {
            let text = text_parts(&message.parts, "system")?;
            if !text.is_empty() {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&text);
            }
            continue;
        }
        match message.role {
            Role::System => unreachable!(),
            Role::User => {
                let content = content_values(&message.parts, "user")?;
                steps.push(json!({"type": "user_input", "content": content}));
            }
            Role::Assistant => {
                for part in &message.parts {
                    match part {
                        ContentPart::Text(part) => {
                            reject_gemini_state(&part.provider_state, "model output")?;
                            steps.push(json!({
                                "type": "model_output",
                                "content": [{"type": "text", "text": part.text}],
                            }));
                        }
                        ContentPart::Reasoning(part) => {
                            let Some(signature) = thought_signature(&part.provider_state)? else {
                                // Reasoning produced by another protocol has no
                                // Gemini replay semantics and is display-only.
                                continue;
                            };
                            let summary = part.text.as_ref().map_or_else(Vec::new, |text| {
                                vec![json!({"type": "text", "text": text})]
                            });
                            steps.push(json!({
                                "type": "thought",
                                "signature": signature,
                                "summary": summary,
                            }));
                        }
                        ContentPart::ToolCall(part) => {
                            // In Interactions v1, signatures belong to dedicated
                            // thought steps (or built-in tool steps), never a
                            // standard function_call.
                            reject_gemini_state(&part.provider_state, "standard function call")?;
                            let name = canonical_to_wire
                                .get(part.tool.as_str())
                                .copied()
                                .map(str::to_string)
                                .unwrap_or_else(|| wire_tool_name(&part.tool));
                            function_names.insert(part.id.clone(), name.clone());
                            let step = json!({
                                "type": "function_call",
                                "id": part.id,
                                "name": name,
                                "arguments": part.args,
                            });
                            steps.push(step);
                        }
                        ContentPart::ToolResult(part) => {
                            reject_gemini_state(&part.provider_state, "assistant tool result")?;
                            return Err(ProviderError::Decode(
                                "gemini-interactions cannot place a function result in an assistant message".into(),
                            ));
                        }
                        ContentPart::Media(part) => {
                            reject_gemini_state(&part.provider_state, "media")?;
                            return Err(ProviderError::Decode(
                                "Gemini media lowering is deferred to Stage 7".into(),
                            ));
                        }
                    }
                }
            }
            Role::Tool => {
                for part in &message.parts {
                    match part {
                        ContentPart::ToolResult(part) => {
                            reject_gemini_state(&part.provider_state, "function result")?;
                            let name = function_names.get(&part.tool_call_id).ok_or_else(|| {
                                ProviderError::Decode(format!(
                                    "Gemini function result '{}' has no preceding function call",
                                    part.tool_call_id
                                ))
                            })?;
                            steps.push(json!({
                                "type": "function_result",
                                "name": name,
                                "call_id": part.tool_call_id,
                                "result": [{"type": "text", "text": part.content}],
                            }));
                        }
                        ContentPart::Reasoning(part) => {
                            reject_gemini_state(&part.provider_state, "tool reasoning")?;
                        }
                        ContentPart::Media(part) => {
                            reject_gemini_state(&part.provider_state, "media")?;
                            return Err(ProviderError::Decode(
                                "Gemini media lowering is deferred to Stage 7".into(),
                            ));
                        }
                        ContentPart::Text(part) => {
                            reject_gemini_state(&part.provider_state, "tool text")?;
                            return Err(ProviderError::Decode(
                                "gemini-interactions tool messages require a function result id"
                                    .into(),
                            ));
                        }
                        ContentPart::ToolCall(part) => {
                            reject_gemini_state(&part.provider_state, "tool function call")?;
                            return Err(ProviderError::Decode(
                                "gemini-interactions cannot place a function call in a tool message".into(),
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(((!system.is_empty()).then_some(system), steps))
}

fn content_values(parts: &[ContentPart], role: &str) -> Result<Vec<Value>, ProviderError> {
    let mut content = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text(part) => {
                reject_gemini_state(&part.provider_state, role)?;
                content.push(json!({"type": "text", "text": part.text}));
            }
            ContentPart::Reasoning(part) => reject_gemini_state(&part.provider_state, role)?,
            ContentPart::Media(part) => {
                reject_gemini_state(&part.provider_state, "media")?;
                return Err(ProviderError::Decode(
                    "Gemini media lowering is deferred to Stage 7".into(),
                ));
            }
            ContentPart::ToolCall(part) => {
                reject_gemini_state(&part.provider_state, role)?;
                return Err(ProviderError::Decode(format!(
                    "gemini-interactions cannot place a function call in a {role} message"
                )));
            }
            ContentPart::ToolResult(part) => {
                reject_gemini_state(&part.provider_state, role)?;
                return Err(ProviderError::Decode(format!(
                    "gemini-interactions cannot place a function result in a {role} message"
                )));
            }
        }
    }
    Ok(content)
}

fn text_parts(parts: &[ContentPart], role: &str) -> Result<String, ProviderError> {
    let values = content_values(parts, role)?;
    Ok(values
        .iter()
        .filter_map(|value| value.get("text").and_then(Value::as_str))
        .collect())
}

fn reject_gemini_state(states: &[ProviderState], location: &str) -> Result<(), ProviderError> {
    if let Some(state) = states
        .iter()
        .find(|state| state.protocol == Protocol::GeminiInteractions)
    {
        return Err(ProviderError::Decode(format!(
            "Gemini-owned state '{}' is invalid on {location}",
            state.kind
        )));
    }
    Ok(())
}

fn thought_signature(states: &[ProviderState]) -> Result<Option<String>, ProviderError> {
    let own: Vec<_> = states
        .iter()
        .filter(|state| state.protocol == Protocol::GeminiInteractions)
        .collect();
    if own.is_empty() {
        return Ok(None);
    }
    if own.len() != 1 || own[0].kind != THOUGHT_SIGNATURE {
        return Err(ProviderError::Decode(
            "Gemini thought replay requires exactly one thought_signature state".into(),
        ));
    }
    own[0]
        .value
        .as_str()
        .filter(|signature| !signature.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ProviderError::Decode(
                "Gemini thought_signature state must be a non-empty string".into(),
            )
        })
        .map(Some)
}

#[derive(Default, Clone)]
struct UsageRaw {
    input: u32,
    output: u32,
    total: u32,
}

fn usage_from(value: &Value) -> Option<UsageRaw> {
    let usage = value
        .get("interaction")
        .and_then(|interaction| interaction.get("usage"))
        .or_else(|| value.get("usage"))
        .or_else(|| value.pointer("/metadata/total_usage"))?;
    let number = |name: &str| {
        usage
            .get(name)
            .and_then(Value::as_u64)
            .unwrap_or_default()
            .min(u64::from(u32::MAX)) as u32
    };
    Some(UsageRaw {
        input: number("total_input_tokens"),
        output: number("total_output_tokens"),
        total: number("total_tokens"),
    })
}

fn usage_block(usage: UsageRaw) -> Block {
    Block::Usage(Usage {
        prompt_tokens: usage.input,
        completion_tokens: usage.output,
        total_tokens: usage.total,
    })
}

fn provider_state(signature: String) -> Vec<ProviderState> {
    vec![ProviderState {
        protocol: Protocol::GeminiInteractions,
        kind: THOUGHT_SIGNATURE.into(),
        value: Value::String(signature),
    }]
}

fn content_text(content: &Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect()
}

fn decode_steps(
    steps: &[Value],
    names: &HashMap<String, String>,
) -> Result<ModelMessage, ProviderError> {
    let mut parts = Vec::new();
    for step in steps {
        match step.get("type").and_then(Value::as_str).unwrap_or_default() {
            "thought" => {
                let signature = step
                    .get("signature")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        ProviderError::Decode(
                            "Gemini thought omitted its required signature".into(),
                        )
                    })?;
                let summary = content_text(step.get("summary").unwrap_or(&Value::Null));
                parts.push(ContentPart::Reasoning(kernel::ReasoningPart {
                    text: (!summary.is_empty()).then_some(summary),
                    provider_state: provider_state(signature.to_string()),
                }));
            }
            "model_output" => {
                let text = content_text(step.get("content").unwrap_or(&Value::Null));
                if !text.is_empty() {
                    parts.push(ContentPart::Text(kernel::TextPart {
                        text,
                        provider_state: Vec::new(),
                    }));
                }
            }
            "function_call" => {
                let id = required_string(step, "id", "Gemini function call")?;
                let wire = required_string(step, "name", "Gemini function call")?;
                if step.get("signature").is_some() {
                    return Err(ProviderError::Decode(
                        "Gemini standard function call unexpectedly contained a signature".into(),
                    ));
                }
                parts.push(ContentPart::ToolCall(kernel::ToolCallPart {
                    id,
                    tool: names.get(&wire).cloned().unwrap_or(wire),
                    args: step.get("arguments").cloned().unwrap_or_else(|| json!({})),
                    provider_state: Vec::new(),
                }));
            }
            _ => {}
        }
    }
    Ok(ModelMessage {
        role: Role::Assistant,
        parts,
    })
}

fn required_string(value: &Value, field: &str, context: &str) -> Result<String, ProviderError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ProviderError::Decode(format!("{context} omitted required '{field}'")))
}

fn blocks_for_message(message: &ModelMessage) -> Vec<Block> {
    let mut blocks = Vec::new();
    for part in &message.parts {
        match part {
            ContentPart::Reasoning(part) => {
                if let Some(text) = &part.text {
                    blocks.push(Block::Reasoning(text.clone()));
                }
            }
            ContentPart::Text(part) => blocks.push(Block::Text(part.text.clone())),
            ContentPart::ToolCall(part) => blocks.push(Block::ToolIntent(ToolIntent {
                id: part.id.clone(),
                tool: part.tool.clone(),
                args: part.args.clone(),
            })),
            _ => {}
        }
    }
    blocks.push(Block::CompletedMessage(message.clone()));
    blocks
}

pub(crate) fn parse_interaction(
    body: &str,
    names: &HashMap<String, String>,
) -> Result<Vec<Block>, ProviderError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| ProviderError::Decode(format!("Gemini response parse: {error}")))?;
    if value.get("status").and_then(Value::as_str) == Some("failed") {
        return Err(ProviderError::Stream(
            value
                .get("error")
                .map(Value::to_string)
                .unwrap_or_else(|| "Gemini interaction failed".into()),
        ));
    }
    let steps = value
        .get("steps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let message = decode_steps(&steps, names)?;
    let mut blocks = blocks_for_message(&message);
    if let Some(usage) = usage_from(&value) {
        blocks.push(usage_block(usage));
    }
    Ok(blocks)
}

#[derive(Default)]
enum StepAccum {
    Thought {
        summary: String,
        signature: Option<String>,
    },
    ModelOutput {
        text: String,
    },
    FunctionCall {
        id: String,
        name: String,
        initial_arguments: Option<Value>,
        argument_deltas: String,
    },
    #[default]
    Unknown,
}

pub(crate) struct ResponseDecoder {
    names: HashMap<String, String>,
    active: BTreeMap<u32, StepAccum>,
    finished_parts: BTreeMap<u32, Vec<ContentPart>>,
    usage: Option<UsageRaw>,
    terminal: bool,
}

impl ResponseDecoder {
    pub(crate) fn new(names: HashMap<String, String>) -> Self {
        Self {
            names,
            active: BTreeMap::new(),
            finished_parts: BTreeMap::new(),
            usage: None,
            terminal: false,
        }
    }

    pub(crate) fn push(&mut self, event: &SseEvent) -> Result<Vec<Block>, ProviderError> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            return if event.event.as_deref() == Some("done") {
                Ok(Vec::new())
            } else {
                Err(ProviderError::Stream(
                    "Gemini [DONE] sentinel arrived outside the done event".into(),
                ))
            };
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|error| ProviderError::Stream(format!("Gemini SSE JSON: {error}")))?;
        // Stable v1 permits cumulative usage in `metadata.total_usage` on any
        // streamed event, while terminal lifecycle events may carry it under
        // `interaction.usage`. Retain the newest report so neither shape loses
        // accounting when the terminal payload omits usage.
        if let Some(usage) = usage_from(&value) {
            self.usage = Some(usage);
        }
        let kind = value
            .get("event_type")
            .or_else(|| value.get("type"))
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .unwrap_or_default();
        match kind {
            "step.start" => self.start_step(&value),
            "step.delta" => self.delta_step(&value),
            "step.stop" => self.stop_step(&value),
            "interaction.completed" | "interaction.requires_action" => self.complete(&value),
            "error" | "interaction.failed" => Err(ProviderError::Stream(
                value
                    .get("error")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "Gemini interaction failed".into()),
            )),
            _ => Ok(Vec::new()),
        }
    }

    fn index(value: &Value) -> Result<u32, ProviderError> {
        value
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| u32::try_from(index).ok())
            .ok_or_else(|| ProviderError::Stream("Gemini step event omitted a valid index".into()))
    }

    fn start_step(&mut self, value: &Value) -> Result<Vec<Block>, ProviderError> {
        let index = Self::index(value)?;
        let step = value
            .get("step")
            .ok_or_else(|| ProviderError::Stream("Gemini step.start omitted its step".into()))?;
        let accum = match step.get("type").and_then(Value::as_str).unwrap_or_default() {
            "thought" => StepAccum::Thought {
                summary: content_text(step.get("summary").unwrap_or(&Value::Null)),
                signature: step
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            "model_output" => StepAccum::ModelOutput {
                text: content_text(step.get("content").unwrap_or(&Value::Null)),
            },
            "function_call" => {
                if step.get("signature").is_some() {
                    return Err(ProviderError::Stream(
                        "Gemini standard function-call step unexpectedly contained a signature"
                            .into(),
                    ));
                }
                StepAccum::FunctionCall {
                    id: required_string(step, "id", "Gemini function-call step.start")?,
                    name: required_string(step, "name", "Gemini function-call step.start")?,
                    initial_arguments: step
                        .get("arguments")
                        .filter(|arguments| !arguments.is_null())
                        .cloned(),
                    argument_deltas: String::new(),
                }
            }
            _ => StepAccum::Unknown,
        };
        let initial_blocks = match &accum {
            StepAccum::Thought { summary, .. } if !summary.is_empty() => {
                vec![Block::Reasoning(summary.clone())]
            }
            StepAccum::ModelOutput { text } if !text.is_empty() => {
                vec![Block::Text(text.clone())]
            }
            _ => Vec::new(),
        };
        if self.active.insert(index, accum).is_some() || self.finished_parts.contains_key(&index) {
            return Err(ProviderError::Stream(format!(
                "Gemini emitted duplicate step.start index {index}"
            )));
        }
        Ok(initial_blocks)
    }

    fn delta_step(&mut self, value: &Value) -> Result<Vec<Block>, ProviderError> {
        let index = Self::index(value)?;
        let delta = value
            .get("delta")
            .ok_or_else(|| ProviderError::Stream("Gemini step.delta omitted its delta".into()))?;
        let accum = self.active.get_mut(&index).ok_or_else(|| {
            ProviderError::Stream(format!("Gemini delta referenced unknown step {index}"))
        })?;
        let kind = delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match (accum, kind) {
            (StepAccum::ModelOutput { text }, "text") => {
                let chunk = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                text.push_str(chunk);
                Ok((!chunk.is_empty())
                    .then(|| Block::Text(chunk.to_string()))
                    .into_iter()
                    .collect())
            }
            (StepAccum::Thought { summary, .. }, "thought_summary") => {
                let chunk = delta
                    .get("content")
                    .filter(|content| content.get("type").and_then(Value::as_str) == Some("text"))
                    .and_then(|content| content.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                summary.push_str(chunk);
                Ok((!chunk.is_empty())
                    .then(|| Block::Reasoning(chunk.to_string()))
                    .into_iter()
                    .collect())
            }
            (StepAccum::Thought { summary, .. }, "thought") => {
                let chunk = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                summary.push_str(chunk);
                Ok((!chunk.is_empty())
                    .then(|| Block::Reasoning(chunk.to_string()))
                    .into_iter()
                    .collect())
            }
            (StepAccum::Thought { signature, .. }, "thought_signature") => {
                *signature = delta
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Ok(Vec::new())
            }
            (
                StepAccum::FunctionCall {
                    initial_arguments,
                    argument_deltas,
                    ..
                },
                "arguments_delta" | "arguments",
            ) => {
                let chunk = delta
                    .get("arguments")
                    .or_else(|| delta.get("partial_arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let has_materialized_initial = initial_arguments.as_ref().is_some_and(
                    |value| !matches!(value, Value::Object(object) if object.is_empty()),
                );
                if has_materialized_initial && !chunk.is_empty() {
                    return Err(ProviderError::Stream(format!(
                        "Gemini function-call step {index} supplied both complete and streamed arguments"
                    )));
                }
                argument_deltas.push_str(chunk);
                Ok(Vec::new())
            }
            (StepAccum::Unknown, _) => Ok(Vec::new()),
            _ => Err(ProviderError::Stream(format!(
                "Gemini delta type '{kind}' does not match step {index}"
            ))),
        }
    }

    fn finalize_step(&mut self, index: u32) -> Result<Vec<Block>, ProviderError> {
        let accum = self.active.remove(&index).ok_or_else(|| {
            ProviderError::Stream(format!("Gemini step.stop referenced unknown step {index}"))
        })?;
        let parts = match accum {
            StepAccum::Thought { summary, signature } => {
                let signature = signature.filter(|value| !value.is_empty()).ok_or_else(|| {
                    ProviderError::Stream(format!(
                        "Gemini thought step {index} omitted its required signature"
                    ))
                })?;
                vec![ContentPart::Reasoning(kernel::ReasoningPart {
                    text: (!summary.is_empty()).then_some(summary),
                    provider_state: provider_state(signature),
                })]
            }
            StepAccum::ModelOutput { text } => (!text.is_empty())
                .then(|| {
                    ContentPart::Text(kernel::TextPart {
                        text,
                        provider_state: Vec::new(),
                    })
                })
                .into_iter()
                .collect(),
            StepAccum::FunctionCall {
                id,
                name,
                initial_arguments,
                argument_deltas,
            } => {
                let args = if argument_deltas.trim().is_empty() {
                    initial_arguments.unwrap_or_else(|| json!({}))
                } else {
                    serde_json::from_str(&argument_deltas).map_err(|error| {
                        ProviderError::Decode(format!("Gemini function arguments: {error}"))
                    })?
                };
                let canonical = self.names.get(&name).cloned().unwrap_or(name);
                vec![ContentPart::ToolCall(kernel::ToolCallPart {
                    id,
                    tool: canonical,
                    args,
                    provider_state: Vec::new(),
                })]
            }
            StepAccum::Unknown => Vec::new(),
        };
        self.finished_parts.insert(index, parts);
        Ok(Vec::new())
    }

    fn stop_step(&mut self, value: &Value) -> Result<Vec<Block>, ProviderError> {
        self.finalize_step(Self::index(value)?)
    }

    fn complete(&mut self, value: &Value) -> Result<Vec<Block>, ProviderError> {
        if !self.active.is_empty() {
            return Err(ProviderError::Stream(format!(
                "Gemini terminal interaction arrived before step.stop for indices {:?}",
                self.active.keys().collect::<Vec<_>>()
            )));
        }
        let mut blocks = Vec::new();
        let parts = std::mem::take(&mut self.finished_parts)
            .into_values()
            .flatten()
            .collect::<Vec<_>>();
        for part in &parts {
            if let ContentPart::ToolCall(call) = part {
                blocks.push(Block::ToolIntent(ToolIntent {
                    id: call.id.clone(),
                    tool: call.tool.clone(),
                    args: call.args.clone(),
                }));
            }
        }
        blocks.push(Block::CompletedMessage(ModelMessage {
            role: Role::Assistant,
            parts,
        }));
        if let Some(usage) = self.usage.take().or_else(|| usage_from(value)) {
            blocks.push(usage_block(usage));
        }
        self.terminal = true;
        Ok(blocks)
    }

    pub(crate) fn finish(self) -> Result<(), ProviderError> {
        if self.terminal {
            Ok(())
        } else {
            Err(ProviderError::Stream(
                "Gemini stream ended before a terminal interaction event".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::{
        BlastRadius, Message, ReasoningPart, TextPart, ToolCallPart, ToolCategory, ToolResultPart,
        ToolSpec,
    };

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "read a file".into(),
            schema: json!({"type": "object"}),
            blast_radius: BlastRadius::Read,
            category: ToolCategory::Read,
            icon: "t".into(),
        }
    }

    #[test]
    fn standard_function_call_rejects_misplaced_signature() {
        let sig = ProviderState {
            protocol: Protocol::GeminiInteractions,
            kind: THOUGHT_SIGNATURE.into(),
            value: json!("fc-sig"),
        };
        let ordered = vec![
            Message::user("do it").ordered(),
            ModelMessage {
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "c1".into(),
                    tool: "fs.read".into(),
                    args: json!({"path": "a"}),
                    provider_state: vec![sig],
                })],
            },
            ModelMessage {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: "c1".into(),
                    content: "x".into(),
                    provider_state: Vec::new(),
                })],
            },
        ];
        let context = CompiledContext {
            model: String::new(),
            messages: Vec::new(),
            ordered: Some(ordered),
            tools: vec![tool("fs.read")],
        };
        let error = prepare_body(
            &context,
            "gemini-model",
            true,
            &ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Minimal),
            },
            Some(4096),
        )
        .unwrap_err();
        assert!(error.to_string().contains("standard function call"));
    }

    #[test]
    fn completed_text_turn_thought_is_replayed_exactly() {
        let signature = ProviderState {
            protocol: Protocol::GeminiInteractions,
            kind: THOUGHT_SIGNATURE.into(),
            value: json!("stale-sig"),
        };
        let ordered = vec![
            Message::user("explain databases").ordered(),
            ModelMessage {
                role: Role::Assistant,
                parts: vec![
                    ContentPart::Reasoning(ReasoningPart {
                        text: None,
                        provider_state: vec![signature],
                    }),
                    ContentPart::Text(TextPart {
                        text: "here is the explanation".into(),
                        provider_state: Vec::new(),
                    }),
                ],
            },
            Message::user("now pick one").ordered(),
        ];
        let context = CompiledContext {
            model: String::new(),
            messages: Vec::new(),
            ordered: Some(ordered),
            tools: Vec::new(),
        };
        let (body, _) = prepare_body(
            &context,
            "gemini-model",
            true,
            &ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Minimal),
            },
            Some(4096),
        )
        .unwrap();
        let thought = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["type"] == "thought")
            .expect("stateless mode must replay every thought");
        assert_eq!(thought["signature"], "stale-sig");
        assert_eq!(thought["summary"], json!([]));
    }

    #[test]
    fn stateless_request_replays_thought_signature_and_function_timeline() {
        let signature = ProviderState {
            protocol: Protocol::GeminiInteractions,
            kind: THOUGHT_SIGNATURE.into(),
            value: json!("signed-value"),
        };
        let ordered = vec![
            Message::system("system").ordered(),
            Message::user("read it").ordered(),
            ModelMessage {
                role: Role::Assistant,
                parts: vec![
                    ContentPart::Reasoning(ReasoningPart {
                        text: Some("checking".into()),
                        provider_state: vec![signature],
                    }),
                    ContentPart::ToolCall(ToolCallPart {
                        id: "call-1".into(),
                        tool: "fs.read".into(),
                        args: json!({"path": "README.md"}),
                        provider_state: Vec::new(),
                    }),
                ],
            },
            ModelMessage {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: "call-1".into(),
                    content: "contents".into(),
                    provider_state: Vec::new(),
                })],
            },
        ];
        let context = CompiledContext {
            model: String::new(),
            messages: Vec::new(),
            ordered: Some(ordered),
            tools: vec![tool("fs.read")],
        };
        let (body, _) = prepare_body(
            &context,
            "gemini-model",
            true,
            &ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Medium),
            },
            Some(4096),
        )
        .unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system_instruction"], "system");
        assert_eq!(body["generation_config"]["thinking_level"], "medium");
        assert_eq!(body["input"][1]["signature"], "signed-value");
        assert_eq!(body["input"][2]["name"], "fs_read");
        assert_eq!(body["input"][3]["type"], "function_result");
        assert_eq!(body["input"][3]["name"], "fs_read");
        assert_eq!(body["input"][3]["call_id"], "call-1");
    }

    #[test]
    fn minimal_effort_maps_to_the_stable_gemini_level() {
        let context = CompiledContext {
            model: String::new(),
            messages: vec![Message::user("hello")],
            ordered: None,
            tools: Vec::new(),
        };
        let (body, _) = prepare_body(
            &context,
            "gemini-model",
            true,
            &ReasoningConfig {
                enabled: Some(true),
                effort: Some(ReasoningEffort::Minimal),
            },
            None,
        )
        .unwrap();
        assert_eq!(body["generation_config"]["thinking_level"], "minimal");
        assert_eq!(body["generation_config"]["thinking_summaries"], "auto");
    }

    #[test]
    fn nonstream_response_preserves_signature_and_normalizes_usage() {
        let body = json!({
            "status": "requires_action",
            "steps": [
                {"type":"thought","signature":"sig","summary":[{"type":"text","text":"plan"}]},
                {"type":"function_call","id":"c1","name":"fs_read","arguments":{"path":"a"}}
            ],
            "usage": {"total_input_tokens":10,"total_output_tokens":4,"total_tokens":14}
        })
        .to_string();
        let blocks = parse_interaction(
            &body,
            &HashMap::from([("fs_read".into(), "fs.read".into())]),
        )
        .unwrap();
        let completed = blocks
            .iter()
            .find_map(|block| match block {
                Block::CompletedMessage(message) => Some(message),
                _ => None,
            })
            .unwrap();
        assert!(completed.has_provider_state());
        assert!(
            matches!(&completed.parts[1], ContentPart::ToolCall(call) if call.tool == "fs.read")
        );
        assert!(matches!(
            blocks.last(),
            Some(Block::Usage(Usage {
                prompt_tokens: 10,
                total_tokens: 14,
                ..
            }))
        ));
    }

    #[test]
    fn nonstream_thought_without_signature_fails_closed() {
        let body = json!({
            "status": "completed",
            "steps": [
                {"type":"thought","summary":[{"type":"text","text":"plan"}]},
                {"type":"model_output","content":[{"type":"text","text":"done"}]}
            ]
        })
        .to_string();
        let error = parse_interaction(&body, &HashMap::new()).unwrap_err();
        assert!(error.to_string().contains("required signature"));
    }

    #[test]
    fn streaming_reassembles_arguments_and_signature_before_completion() {
        let mut decoder =
            ResponseDecoder::new(HashMap::from([("fs_read".into(), "fs.read".into())]));
        let events = [
            (
                "step.start",
                json!({"type":"step.start","index":0,"step":{"type":"thought"}}),
            ),
            (
                "step.delta",
                json!({"type":"step.delta","index":0,"delta":{"type":"thought_summary","content":{"type":"text","text":"plan"}}}),
            ),
            (
                "step.delta",
                json!({"type":"step.delta","index":0,"delta":{"type":"thought_signature","signature":"sig"}}),
            ),
            ("step.stop", json!({"type":"step.stop","index":0})),
            (
                "step.start",
                json!({"type":"step.start","index":1,"step":{"type":"function_call","id":"c1","name":"fs_read","arguments":{}}}),
            ),
            (
                "step.delta",
                json!({"type":"step.delta","index":1,"delta":{"type":"arguments_delta","arguments":"{\"path\":"}}),
            ),
            (
                "step.delta",
                json!({"type":"step.delta","index":1,"delta":{"type":"arguments_delta","arguments":"\"a\"}"},"metadata":{"total_usage":{"total_input_tokens":9,"total_output_tokens":3,"total_tokens":12}}}),
            ),
            ("step.stop", json!({"type":"step.stop","index":1})),
            (
                "interaction.requires_action",
                json!({"type":"interaction.requires_action","interaction":{"status":"requires_action"}}),
            ),
        ];
        let mut blocks = Vec::new();
        for (name, data) in events {
            blocks.extend(
                decoder
                    .push(&SseEvent {
                        event: Some(name.into()),
                        data: data.to_string(),
                    })
                    .unwrap(),
            );
        }
        decoder.finish().unwrap();
        assert!(blocks.iter().any(|block| matches!(block, Block::ToolIntent(intent) if intent.tool == "fs.read" && intent.args["path"] == "a")));
        let completed = blocks
            .iter()
            .find_map(|block| match block {
                Block::CompletedMessage(message) => Some(message),
                _ => None,
            })
            .unwrap();
        assert!(completed.has_provider_state());
        assert!(matches!(
            blocks.last(),
            Some(Block::Usage(Usage {
                prompt_tokens: 9,
                total_tokens: 12,
                ..
            }))
        ));
    }

    #[test]
    fn thought_without_signature_and_missing_step_stop_fail_closed() {
        let mut decoder = ResponseDecoder::new(HashMap::new());
        decoder
            .push(&SseEvent {
                event: Some("step.start".into()),
                data: json!({"event_type":"step.start","index":0,"step":{"type":"thought","signature":"","summary":[]}}).to_string(),
            })
            .unwrap();
        let stop_error = decoder
            .push(&SseEvent {
                event: Some("step.stop".into()),
                data: json!({"event_type":"step.stop","index":0}).to_string(),
            })
            .unwrap_err();
        assert!(stop_error.to_string().contains("required signature"));

        let mut decoder = ResponseDecoder::new(HashMap::new());
        decoder
            .push(&SseEvent {
                event: Some("step.start".into()),
                data: json!({"event_type":"step.start","index":0,"step":{"type":"model_output","content":[{"type":"text","text":"done"}]}}).to_string(),
            })
            .unwrap();
        let terminal_error = decoder
            .push(&SseEvent {
                event: Some("interaction.completed".into()),
                data: json!({"event_type":"interaction.completed","interaction":{"status":"completed"}}).to_string(),
            })
            .unwrap_err();
        assert!(terminal_error.to_string().contains("before step.stop"));
    }

    #[test]
    fn stream_cutoff_and_invalid_owned_state_fail_closed() {
        assert!(ResponseDecoder::new(HashMap::new()).finish().is_err());
        let context = CompiledContext {
            model: String::new(),
            messages: Vec::new(),
            ordered: Some(vec![ModelMessage {
                role: Role::Assistant,
                parts: vec![ContentPart::Reasoning(ReasoningPart {
                    text: None,
                    provider_state: vec![ProviderState {
                        protocol: Protocol::GeminiInteractions,
                        kind: "wrong".into(),
                        value: json!("sig"),
                    }],
                })],
            }]),
            tools: Vec::new(),
        };
        assert!(prepare_body(&context, "m", false, &ReasoningConfig::default(), None).is_err());
    }

    #[test]
    fn stable_error_event_is_not_ignored() {
        let mut decoder = ResponseDecoder::new(HashMap::new());
        let error = SseEvent {
            event: Some("error".into()),
            data: json!({
                "event_type": "error",
                "error": {"code": "not_found", "message": "interaction missing"}
            })
            .to_string(),
        };
        assert!(matches!(
            decoder.push(&error),
            Err(ProviderError::Stream(_))
        ));
    }

    #[test]
    fn done_sentinel_is_accepted_only_as_transport_termination() {
        let mut completed = ResponseDecoder::new(HashMap::new());
        completed
            .push(&SseEvent {
                event: Some("interaction.completed".into()),
                data: json!({
                    "event_type": "interaction.completed",
                    "interaction": {"status": "completed"}
                })
                .to_string(),
            })
            .unwrap();
        assert!(
            completed
                .push(&SseEvent {
                    event: Some("done".into()),
                    data: "[DONE]".into(),
                })
                .unwrap()
                .is_empty()
        );
        completed.finish().unwrap();

        let mut cutoff = ResponseDecoder::new(HashMap::new());
        cutoff
            .push(&SseEvent {
                event: Some("done".into()),
                data: "[DONE]".into(),
            })
            .unwrap();
        assert!(cutoff.finish().is_err());

        let mut malformed = ResponseDecoder::new(HashMap::new());
        assert!(
            malformed
                .push(&SseEvent {
                    event: Some("step.delta".into()),
                    data: "[DONE]".into(),
                })
                .is_err()
        );
    }

    #[test]
    fn arbitrary_sse_byte_splits_preserve_the_complete_interaction() {
        let fixture = concat!(
            "event: step.start\n",
            "data: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"thought\"}}\n\n",
            "event: step.delta\n",
            "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"thought_signature\",\"signature\":\"sig\"}}\n\n",
            "event: step.stop\n",
            "data: {\"event_type\":\"step.stop\",\"index\":0}\n\n",
            "event: step.start\n",
            "data: {\"event_type\":\"step.start\",\"index\":1,\"step\":{\"type\":\"model_output\",\"content\":[{\"type\":\"text\",\"text\":\"He\"}]}}\n\n",
            "event: step.delta\n",
            "data: {\"event_type\":\"step.delta\",\"index\":1,\"delta\":{\"type\":\"text\",\"text\":\"llo\"}}\n\n",
            "event: step.stop\n",
            "data: {\"event_type\":\"step.stop\",\"index\":1}\n\n",
            "event: interaction.completed\n",
            "data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"status\":\"completed\"}}\n\n",
            "event: done\n",
            "data: [DONE]\n\n"
        )
        .as_bytes();

        for split in 0..=fixture.len() {
            let mut framing = crate::transport::sse::SseDecoder::default();
            let mut decoder = ResponseDecoder::new(HashMap::new());
            let mut blocks = Vec::new();
            for chunk in [&fixture[..split], &fixture[split..]] {
                for event in framing.push(chunk) {
                    blocks.extend(decoder.push(&event).unwrap());
                }
            }
            if let Some(event) = framing.finish() {
                blocks.extend(decoder.push(&event).unwrap());
            }
            decoder.finish().unwrap();
            let visible: String = blocks
                .iter()
                .filter_map(|block| match block {
                    Block::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(visible, "Hello", "failed at byte split {split}");
            assert!(blocks.iter().any(|block| matches!(
                block,
                Block::CompletedMessage(message) if message.has_provider_state()
            )));
        }
    }

    #[test]
    fn parallel_function_calls_are_released_in_step_index_order() {
        let mut decoder = ResponseDecoder::new(HashMap::from([
            ("first_wire".into(), "first.tool".into()),
            ("second_wire".into(), "second.tool".into()),
        ]));
        let events = [
            json!({"event_type":"step.start","index":0,"step":{"type":"function_call","id":"c0","name":"first_wire"}}),
            json!({"event_type":"step.start","index":1,"step":{"type":"function_call","id":"c1","name":"second_wire"}}),
            json!({"event_type":"step.delta","index":1,"delta":{"type":"arguments_delta","arguments":"{\"n\":2}"}}),
            json!({"event_type":"step.delta","index":0,"delta":{"type":"arguments_delta","arguments":"{\"n\":1}"}}),
            json!({"event_type":"step.stop","index":1}),
            json!({"event_type":"step.stop","index":0}),
            json!({"event_type":"interaction.requires_action","interaction":{"status":"requires_action"}}),
        ];
        let mut blocks = Vec::new();
        for value in events {
            blocks.extend(
                decoder
                    .push(&SseEvent {
                        event: None,
                        data: value.to_string(),
                    })
                    .unwrap(),
            );
        }
        decoder.finish().unwrap();
        let calls = blocks
            .iter()
            .filter_map(|block| match block {
                Block::ToolIntent(intent) => Some((intent.id.as_str(), intent.tool.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls, vec![("c0", "first.tool"), ("c1", "second.tool")]);
    }

    #[test]
    fn foreign_state_is_ignored_not_consumed() {
        let message = ModelMessage {
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart {
                text: "hello".into(),
                provider_state: vec![ProviderState {
                    protocol: Protocol::AnthropicMessages,
                    kind: "signature".into(),
                    value: json!("foreign"),
                }],
            })],
        };
        let context = CompiledContext {
            model: String::new(),
            messages: Vec::new(),
            ordered: Some(vec![message]),
            tools: Vec::new(),
        };
        let (body, _) =
            prepare_body(&context, "m", false, &ReasoningConfig::default(), None).unwrap();
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
    }
}
