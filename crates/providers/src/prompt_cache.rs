//! Tells a server which start of a request it has seen before, so it bills that
//! part as a re-read, and keeps one conversation on one cache.

use crate::profile::ProviderProfile;
use kernel::RequestScope;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// How long a server is asked to keep a marked start of a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheTtl {
    /// An hour where a person types and may pause; five minutes where none does.
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

/// A request carries at most this many marks; servers refuse more.
const MARKS: usize = 4;

/// What one route does with a conversation's requests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Route {
    /// Mark the request; `Some(false)` keeps marks off tool results.
    marks: Option<bool>,
    /// The longer lifetime is not offered on this route.
    short_only: bool,
    cache_key: bool,
    sticky: bool,
    conversation_header: bool,
}

fn host(profile: &ProviderProfile) -> String {
    let parsed = reqwest::Url::parse(&profile.base_url).ok();
    let host = parsed.as_ref().and_then(reqwest::Url::host_str);
    host.unwrap_or_default().to_ascii_lowercase()
}

/// `host` is `domain` or under it; a lookalike that merely contains it is not.
fn under(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// `name` appears as a whole word of `text`, split at any of `breaks`.
fn names(text: &str, breaks: &[char], name: &str) -> bool {
    text.split(breaks).any(|word| word == name)
}

fn kimi(model: &str) -> bool {
    const STARTS: [&str; 12] = [
        "kimi-",
        "kimi_",
        "moonshot-",
        "moonshot_",
        "k1.",
        "k1-",
        "k2.",
        "k2-",
        "k25",
        "k2.5",
        "k3.",
        "k3-",
    ];
    let bare = model.rsplit('/').next().unwrap_or(model);
    bare == "k3" || STARTS.iter().any(|start| bare.starts_with(start)) || model.contains("moonshot")
}

/// What `profile`'s server does for `model`. The order matters: an explicit
/// setting wins, then each known route.
pub(crate) fn route(profile: &ProviderProfile, model: &str) -> Route {
    let host = host(profile);
    let model = model.to_ascii_lowercase();
    let provider = profile.provider_id.as_deref().unwrap_or_default();
    let provider = provider.to_ascii_lowercase();
    let openrouter = under(&host, "openrouter.ai");
    let litellm =
        names(&provider, &[':', '-', '_', '/'], "litellm") || names(&host, &['.', '-'], "litellm");
    let alibaba = matches!(
        host.as_str(),
        "dashscope-intl.aliyuncs.com" | "dashscope.aliyuncs.com"
    );
    let (claude, qwen) = (model.contains("claude"), model.contains("qwen"));
    let marks = match profile.prompt_cache {
        Some(false) => None,
        Some(true) => Some(!litellm),
        None if openrouter && (claude || kimi(&model)) => Some(true),
        // This proxy copies a mark on a tool result where the server refuses it.
        None if litellm && claude => Some(false),
        None if alibaba && qwen => Some(true),
        None => None,
    };
    Route {
        marks,
        short_only: qwen || alibaba,
        cache_key: host == "api.openai.com",
        sticky: openrouter,
        conversation_header: openrouter
            && ["x-ai/grok-", "xai/grok-"]
                .iter()
                .any(|start| model.starts_with(start)),
    }
}

fn text(text: &str, mark: Option<&Value>) -> Value {
    let mut part = json!({ "type": "text", "text": text });
    if let Some(mark) = mark {
        part["cache_control"] = mark.clone();
    }
    part
}

/// Whether a mark on this message would be honoured.
fn carries(message: &Value, tool_results: bool) -> bool {
    if message["role"] == "system" || (message["role"] == "tool" && !tool_results) {
        return false;
    }
    match &message["content"] {
        Value::String(content) => !content.is_empty(),
        Value::Array(parts) => parts.last().is_some_and(Value::is_object),
        _ => false,
    }
}

fn mark_last_part(message: &mut Value, mark: &Value) {
    match &mut message["content"] {
        Value::String(content) => message["content"] = json!([text(content, Some(mark))]),
        Value::Array(parts) => {
            if let Some(last) = parts.last_mut() {
                last["cache_control"] = mark.clone();
            }
        }
        _ => {}
    }
}

/// Mark the system prompt, split at the part every chat shares when it starts
/// with `stable`, then the newest messages that can carry a mark.
fn mark(body: &mut Value, mark: &Value, tool_results: bool, stable: Option<&str>) {
    let Some(messages) = body["messages"].as_array_mut() else {
        return;
    };
    let mut used = 0;
    if let Some(Value::String(system)) = messages
        .first_mut()
        .filter(|first| first["role"] == "system")
        .map(|first| &mut first["content"])
        && !system.is_empty()
    {
        let rest = stable.and_then(|stable| system.strip_prefix(stable));
        let parts = match (stable, rest) {
            (Some(stable), Some(rest)) if !stable.is_empty() && !rest.trim().is_empty() => {
                vec![text(stable, Some(mark)), text(rest, Some(mark))]
            }
            _ => vec![text(system, Some(mark))],
        };
        used = parts.len();
        messages[0]["content"] = Value::Array(parts);
    }
    let carriers: Vec<usize> = (0..messages.len())
        .filter(|at| carries(&messages[*at], tool_results))
        .collect();
    for at in carriers.into_iter().rev().take(MARKS - used) {
        mark_last_part(&mut messages[at], mark);
    }
}

/// A short, stable name for this conversation's unchanging start.
fn cache_key(body: &Value, conversation: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(conversation.as_bytes());
    hash.update([0]);
    if body["messages"][0]["role"] == "system" {
        hash.update(body["messages"][0]["content"].to_string().as_bytes());
    }
    hash.update([0]);
    hash.update(body["tools"].to_string().as_bytes());
    let digest = hash.finalize();
    let hex: String = digest.iter().take(12).map(|b| format!("{b:02x}")).collect();
    format!("pck_{hex}")
}

/// Add to `body` what this route uses, and return the headers it needs.
pub(crate) fn apply(
    body: &mut Value,
    route: Route,
    ttl: CacheTtl,
    scope: &RequestScope,
    stable_system: Option<&str>,
) -> Vec<(String, String)> {
    // The key names the unmarked request, so it is the same for every lifetime.
    let key = route
        .cache_key
        .then(|| cache_key(body, &scope.conversation));
    if let Some(tool_results) = route.marks {
        let hour = match ttl {
            CacheTtl::Auto => !scope.unattended,
            CacheTtl::FiveMinutes => false,
            CacheTtl::OneHour => true,
        };
        let lifetime = match hour && !route.short_only {
            true => json!({ "type": "ephemeral", "ttl": "1h" }),
            false => json!({ "type": "ephemeral" }),
        };
        mark(body, &lifetime, tool_results, stable_system);
    }
    if scope.conversation.is_empty() {
        return Vec::new();
    }
    if let Some(key) = key {
        body["prompt_cache_key"] = json!(key);
    }
    if route.sticky {
        body["session_id"] = json!(scope.conversation);
    }
    match route.conversation_header {
        true => vec![("x-grok-conv-id".into(), scope.conversation.clone())],
        false => Vec::new(),
    }
}

#[cfg(test)]
#[path = "prompt_cache_tests.rs"]
mod tests;
