//! What the window asks on behalf of a tool's screen: its page, and a call back
//! to the server that drew it. A screen names only its own server; which tools
//! it may reach is the MCP manager's rule, not the window's.

use serde_json::{Value, json};
use std::sync::Arc;

type Manager = Option<Arc<mcp::McpManager>>;

fn text<'a>(params: &'a Value, key: &str) -> Result<&'a str, String> {
    params[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("`{key}` is required"))
}

fn manager(mcp: &Manager) -> Result<&Arc<mcp::McpManager>, String> {
    mcp.as_ref().ok_or_else(|| "MCP is not enabled".to_string())
}

/// The tools that come with a screen, so the window can open one as soon as the
/// model starts calling it, before there is a result.
pub(crate) fn offered(mcp: &Manager) -> Result<Value, String> {
    let tools: Vec<Value> = manager(mcp)?
        .tool_specs()
        .into_iter()
        .filter_map(|spec| {
            let (server, tool) = spec.name.strip_prefix(mcp::TOOL_PREFIX)?.split_once("__")?;
            Some(json!({
                "name": spec.name, "server": server, "tool": tool, "resource": spec.screen?,
            }))
        })
        .collect();
    Ok(json!({ "tools": tools }))
}

pub(crate) async fn page(mcp: &Manager, params: &Value) -> Result<Value, String> {
    let page = manager(mcp)?
        .read_screen(text(params, "server")?, text(params, "uri")?)
        .await
        .map_err(|error| error.to_string())?;
    serde_json::to_value(page).map_err(|error| error.to_string())
}

/// The person has already allowed this one call in the window; the window sends
/// nothing here without that.
pub(crate) async fn call(mcp: &Manager, params: &Value) -> Result<Value, String> {
    let args = params.get("args").cloned().unwrap_or_else(|| json!({}));
    let output = manager(mcp)?
        .call_from_screen(text(params, "server")?, text(params, "tool")?, &args)
        .await
        .map_err(|error| error.to_string())?;
    Ok(output.result.filter(|result| !result.is_null()).unwrap_or_else(|| {
        json!({ "content": [{ "type": "text", "text": output.text }], "isError": output.is_error })
    }))
}
