pub(crate) mod gemini_interactions;
pub(crate) mod openai_chat;

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
