//! Explicit, bounded setup checks through the production protocol adapters.
//! Synthetic messages only. Tool responses are fabricated; no tools execute.

use crate::{ProviderClient, ProviderProfile};
use futures::StreamExt;
use kernel::{
    Block, CompiledContext, ContentPart, Message, ModelMessage, Provider, ProviderError, Role,
    ToolIntent, ToolSpec,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// A queued/reasoning model can legitimately take minutes even for a small
// prompt. Keep setup bounded without imposing a shorter first-response limit
// than the production transport. Streaming still uses its progress watchdog.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_TIMEOUT: Duration = Duration::from_secs(480);
const MAX_BYTES: usize = 64 * 1024;
const TOOL: &str = "medha_setup_echo";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Passed,
    Failed,
    Skipped,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub status: Status,
    pub message: String,
    pub failure: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Result {
    pub text: Check,
    pub streaming: Check,
    pub tools: Check,
}

fn skipped() -> Check {
    Check {
        status: Status::Skipped,
        message: "Not tested because an earlier check failed.".into(),
        failure: None,
    }
}
fn passed(message: &str) -> Check {
    Check {
        status: Status::Passed,
        message: message.into(),
        failure: None,
    }
}
#[derive(Debug, thiserror::Error)]
enum CheckError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    // A completion deadline says nothing about whether progress was made.
    #[error(
        "The setup test's time limit was reached before this check completed. Model support is not confirmed. You can save without testing. No automatic retry was sent."
    )]
    Deadline,
}

fn failed(error: impl Into<CheckError>, credential: &str) -> Check {
    let error = error.into();
    let kind = match &error {
        CheckError::Deadline => "timeout",
        CheckError::Provider(error) => match error {
            ProviderError::Status(401, _) => "credentials_rejected",
            ProviderError::Status(403, _) => "access_restricted",
            ProviderError::Status(429, _) | ProviderError::Throttled { status: 429, .. } => {
                "throttled"
            }
            ProviderError::Status(500..=599, _) | ProviderError::Throttled { .. } => "unavailable",
            ProviderError::Status(_, _) => "invalid_request",
            ProviderError::ResponseTimeout { .. }
            | ProviderError::ProgressTimeout { .. }
            | ProviderError::StreamStalled { .. } => "timeout",
            ProviderError::Truncated => "incomplete",
            ProviderError::Transport(_) | ProviderError::Stream(_) => "transport",
            _ => "invalid_response",
        },
    };
    // Provider diagnostics are already redacted. Remove the exact supplied key
    // too, including an upstream which echoes it in an otherwise innocuous field.
    let message = if credential.is_empty() {
        error.to_string()
    } else {
        error.to_string().replace(credential, "<redacted>")
    };
    Check {
        status: Status::Failed,
        message: message.chars().take(512).collect(),
        failure: Some(kind.into()),
    }
}

struct Answer {
    text: bool,
    intents: Vec<ToolIntent>,
    message: Option<ModelMessage>,
}
async fn call(
    client: &ProviderClient,
    context: &CompiledContext,
    streaming: bool,
    deadline: tokio::time::Instant,
) -> std::result::Result<Answer, CheckError> {
    let now = tokio::time::Instant::now();
    if deadline <= now {
        return Err(CheckError::Deadline);
    }
    client.set_streaming(streaming);
    tokio::time::timeout_at(deadline.min(now + REQUEST_TIMEOUT), async {
        let mut stream = client.stream(context).await?;
        let mut answer = Answer {
            text: false,
            intents: Vec::new(),
            message: None,
        };
        let mut bytes = 0usize;
        let mut blocks = 0usize;
        let mut chat_parts = Vec::new();
        while let Some(block) = stream.next().await {
            blocks += 1;
            let block = block?;
            match block {
                Block::Text(text) => {
                    answer.text |= !text.trim().is_empty();
                    bytes = bytes.saturating_add(text.len());
                    chat_parts.push(ContentPart::Text(kernel::TextPart {
                        text,
                        provider_state: vec![],
                    }));
                }
                Block::Reasoning(text) => {
                    bytes = bytes.saturating_add(text.len());
                    chat_parts.push(ContentPart::Reasoning(kernel::ReasoningPart {
                        text: Some(text),
                        provider_state: vec![],
                    }));
                }
                Block::ToolInput { delta, .. } => bytes = bytes.saturating_add(delta.len()),
                Block::ToolIntent(intent) => {
                    bytes = bytes.saturating_add(
                        serde_json::to_vec(&intent)
                            .map_err(|_| {
                                ProviderError::Decode("Invalid synthetic tool call.".into())
                            })?
                            .len(),
                    );
                    chat_parts.push(ContentPart::ToolCall(kernel::ToolCallPart {
                        id: intent.id.clone(),
                        tool: intent.tool.clone(),
                        args: intent.args.clone(),
                        provider_state: vec![],
                    }));
                    answer.intents.push(intent);
                }
                Block::CompletedMessage(message) => {
                    bytes = bytes.saturating_add(
                        serde_json::to_vec(&message)
                            .map_err(|_| {
                                ProviderError::Decode("Invalid synthetic response.".into())
                            })?
                            .len(),
                    );
                    if answer.message.is_some() {
                        return Err(ProviderError::Decode(
                            "Multiple completed messages in setup check.".into(),
                        ));
                    }
                    answer.message = Some(message);
                }
                _ => {}
            }
            if bytes > MAX_BYTES || blocks > 4096 || answer.intents.len() > 1 {
                return Err(ProviderError::Decode(
                    "Setup response exceeded its limit.".into(),
                ));
            }
        }
        // Chat Completions reports text/intents rather than a completed ordered
        // message. Native state-bearing adapters must supply their canonical
        // message; never reconstruct their signatures from a text projection.
        if answer.message.is_none() && client.protocol() == kernel::Protocol::OpenAiChat {
            answer.message = Some(ModelMessage {
                role: Role::Assistant,
                parts: chat_parts,
                trust: None,
            });
        }
        Ok(answer)
    })
    .await
    .map_err(|_| CheckError::Deadline)?
    .map_err(CheckError::from)
}

pub async fn test(profile: &ProviderProfile, credential: &str) -> Result {
    let deadline = tokio::time::Instant::now() + TEST_TIMEOUT;
    let mut result = Result {
        text: skipped(),
        streaming: skipped(),
        tools: skipped(),
    };
    let mut bounded = profile.clone();
    bounded.max_output_tokens = Some(profile.max_output_tokens.unwrap_or(1024).min(1024));
    if let Some(context) = bounded.max_ctx {
        bounded.max_output_tokens = bounded
            .max_output_tokens
            .map(|limit| limit.min(u64::from(context / 2).max(1)));
    }
    let client = match ProviderClient::from_profile(bounded, credential.to_owned()) {
        Ok(client) => client,
        Err(error) => {
            result.text = failed(error, credential);
            return result;
        }
    };
    let mut context = CompiledContext {
        model: profile.model.clone(),
        messages: vec![Message::user(
            "Respond with the word MEDHA_OK. This is a connection test.",
        )],
        ordered: None,
        tools: Vec::new(),
    };
    match call(&client, &context, false, deadline).await {
        Ok(answer) if answer.text => result.text = passed("Text response completed."),
        Ok(_) => {
            result.text = failed(
                ProviderError::Decode("The text check returned no text.".into()),
                credential,
            );
            return result;
        }
        Err(error) => {
            result.text = failed(error, credential);
            return result;
        }
    }
    match call(&client, &context, true, deadline).await {
        Ok(answer) if answer.text => result.streaming = passed("Streaming response completed."),
        Ok(_) => {
            result.streaming = failed(
                ProviderError::Decode("The streaming check returned no text.".into()),
                credential,
            );
            return result;
        }
        Err(error) => {
            result.streaming = failed(error, credential);
            return result;
        }
    }
    context.messages = vec![Message::user(
        "Call medha_setup_echo with value medha-check. This function is a synthetic echo for this test only.",
    )];
    context.tools = vec![ToolSpec {
        name: TOOL.into(),
        description: "Synthetic echo. Returns the same value. No file, network or command access."
            .into(),
        schema: serde_json::json!({"type":"object","properties":{"value":{"type":"string","enum":["medha-check"]}},"required":["value"],"additionalProperties":false}),
        blast_radius: kernel::BlastRadius::Read,
        category: kernel::ToolCategory::Read,
        icon: String::new(),
    }];
    let answer = match call(&client, &context, true, deadline).await {
        Ok(answer) => answer,
        Err(error) => {
            result.tools = failed(error, credential);
            return result;
        }
    };
    let Some(intent) = answer.intents.first().filter(|intent| {
        intent.tool == TOOL && intent.args == serde_json::json!({"value":"medha-check"})
    }) else {
        result.tools = failed(ProviderError::Decode("The model did not return the requested synthetic function call; tool support is not confirmed.".into()), credential);
        return result;
    };
    // Retain canonical provider state (e.g. Gemini thought signatures) for the
    // result request. Never rebuild a signed call from its text projection.
    let Some(message) = answer.message.filter(|message| message.role == Role::Assistant && message.parts.iter().any(|part| matches!(part, ContentPart::ToolCall(call) if call.id == intent.id && call.tool == TOOL && call.args == intent.args))) else {
        result.tools = failed(ProviderError::Decode("The function call has no canonical replay message.".into()), credential);
        return result;
    };
    let mut ordered = context.ordered_messages();
    ordered.push(message);
    ordered.push(Message::tool_result(&intent.id, r#"{"value":"medha-check"}"#).ordered());
    ordered.push(
        Message::user(
            "The test function returned. Reply with MEDHA_OK; do not call another function.",
        )
        .ordered(),
    );
    context.ordered = Some(ordered);
    context.tools.clear();
    match call(&client, &context, true, deadline).await {
        Ok(answer) if answer.text && answer.intents.is_empty() => {
            result.tools =
                passed("Function call and synthetic result completed. No tools executed.")
        }
        Ok(_) => {
            result.tools = failed(
                ProviderError::Decode("No final text after the synthetic function result.".into()),
                credential,
            )
        }
        Err(error) => result.tools = failed(error, credential),
    }
    result
}

#[cfg(test)]
#[path = "model_test_tests.rs"]
mod tests;
