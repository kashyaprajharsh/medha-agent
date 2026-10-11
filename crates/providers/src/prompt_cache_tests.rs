use super::*;
use crate::profile::AuthKind;

fn profile(base_url: &str) -> ProviderProfile {
    ProviderProfile::openai_chat(base_url, "m", AuthKind::None)
}

fn person() -> RequestScope {
    RequestScope {
        conversation: "chat-1".into(),
        unattended: false,
    }
}

/// A chat with a system prompt, two finished tool rounds and a new question.
fn chat() -> Value {
    json!({
        "messages": [
            { "role": "system", "content": "You are Medha.\n\nEnvironment: today" },
            { "role": "user", "content": "first" },
            { "role": "assistant", "content": "", "tool_calls": [{ "id": "a" }] },
            { "role": "tool", "content": "result a", "tool_call_id": "a" },
            { "role": "assistant", "content": "done" },
            { "role": "user", "content": [
                { "type": "text", "text": "look" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AA" } },
            ] },
        ],
        "tools": [{ "type": "function", "function": { "name": "read" } }],
    })
}

/// Where the marks sit: `s0`/`s1` for parts of the system prompt, else the
/// message's place in the request.
fn marked(body: &Value) -> Vec<String> {
    let mut at = Vec::new();
    for (index, message) in body["messages"].as_array().unwrap().iter().enumerate() {
        let parts = message["content"].as_array().cloned().unwrap_or_default();
        for (part, _) in parts
            .iter()
            .enumerate()
            .filter(|(_, part)| part.get("cache_control").is_some())
        {
            at.push(match message["role"] == "system" {
                true => format!("s{part}"),
                false => index.to_string(),
            });
        }
    }
    at
}

/// Only routes known to honour marks get them, and a profile can force either way.
#[test]
fn marks_go_only_to_servers_that_honour_them() {
    let openrouter = profile("https://openrouter.ai/api/v1");
    let routes = [
        (&openrouter, "anthropic/claude-sonnet-4.5", Some(true)),
        (&openrouter, "moonshotai/kimi-k2.5", Some(true)),
        (&openrouter, "openai/gpt-5", None),
        (
            &profile("https://litellm.corp.example/v1"),
            "claude-opus",
            Some(false),
        ),
        (
            &profile("https://litellm.corp.example/v1"),
            "qwen3-coder",
            None,
        ),
        (
            &profile("https://dashscope-intl.aliyuncs.com/compatible-mode/v1"),
            "qwen3.6-plus",
            Some(true),
        ),
        (&profile("https://api.openai.com/v1"), "gpt-5", None),
        // A lookalike host is not the service.
        (
            &profile("https://openrouter.ai.evil.example/v1"),
            "claude-x",
            None,
        ),
        (
            &profile("https://evil.example/openrouter.ai/v1"),
            "claude-x",
            None,
        ),
    ];
    for (profile, model, marks) in routes {
        assert_eq!(
            route(profile, model).marks,
            marks,
            "{model} at {}",
            profile.base_url
        );
    }

    let mut forced = profile("https://gateway.example/v1");
    forced.prompt_cache = Some(true);
    assert_eq!(route(&forced, "any-model").marks, Some(true));
    let mut off = openrouter.clone();
    off.prompt_cache = Some(false);
    assert_eq!(route(&off, "anthropic/claude-sonnet-4.5").marks, None);
}

/// At most four marks: the shared start of the system prompt, its end, and the
/// two newest messages that can carry one. An empty tool-call message cannot.
#[test]
fn marks_sit_on_the_system_prompt_and_the_newest_messages() {
    let route = route(
        &profile("https://openrouter.ai/api/v1"),
        "anthropic/claude-x",
    );

    let mut split = chat();
    apply(
        &mut split,
        route,
        CacheTtl::Auto,
        &person(),
        Some("You are Medha."),
    );
    assert_eq!(marked(&split), ["s0", "s1", "4", "5"]);
    assert_eq!(split["messages"][0]["content"][0]["text"], "You are Medha.");
    assert_eq!(
        split["messages"][5]["content"][1]["cache_control"]["type"], "ephemeral",
        "the mark rides the message's last part"
    );

    // No shared start known: one mark on the system prompt, three on messages.
    let mut whole = chat();
    apply(
        &mut whole,
        route,
        CacheTtl::Auto,
        &person(),
        Some("Another prompt"),
    );
    assert_eq!(marked(&whole), ["s0", "3", "4", "5"]);
    assert_eq!(whole["messages"][3]["content"][0]["text"], "result a");
    assert_eq!(
        whole["messages"][2]["content"], "",
        "an empty turn carries none"
    );

    // A proxy that would copy a mark into a tool result gets none there.
    let litellm = super::route(&profile("https://litellm.corp.example/v1"), "claude-x");
    let mut proxied = chat();
    apply(&mut proxied, litellm, CacheTtl::Auto, &person(), None);
    assert_eq!(marked(&proxied), ["s0", "1", "4", "5"]);
    assert_eq!(proxied["messages"][3]["content"], "result a");
}

/// An hour where a person may pause, five minutes where nobody does, and never
/// the hour on a route that does not offer it.
#[test]
fn the_lifetime_follows_who_is_typing_and_what_the_route_offers() {
    let lifetime = |profile: &ProviderProfile, model: &str, ttl, unattended| {
        let mut body = chat();
        let scope = RequestScope {
            conversation: "c".into(),
            unattended,
        };
        apply(&mut body, route(profile, model), ttl, &scope, None);
        body["messages"][0]["content"][0]["cache_control"].clone()
    };
    let openrouter = profile("https://openrouter.ai/api/v1");
    let hour = json!({ "type": "ephemeral", "ttl": "1h" });
    let short = json!({ "type": "ephemeral" });

    assert_eq!(
        lifetime(&openrouter, "anthropic/claude-x", CacheTtl::Auto, false),
        hour
    );
    assert_eq!(
        lifetime(&openrouter, "anthropic/claude-x", CacheTtl::Auto, true),
        short
    );
    assert_eq!(
        lifetime(
            &openrouter,
            "anthropic/claude-x",
            CacheTtl::FiveMinutes,
            false
        ),
        short
    );
    assert_eq!(
        lifetime(&openrouter, "anthropic/claude-x", CacheTtl::OneHour, true),
        hour
    );
    let alibaba = profile("https://dashscope-intl.aliyuncs.com/compatible-mode/v1");
    assert_eq!(
        lifetime(&alibaba, "qwen3.6-plus", CacheTtl::OneHour, false),
        short
    );
}

/// What is planned is what is sent: the marks arrive in the request a real
/// client posts, split at the shared start, with the request's own header.
#[tokio::test]
async fn the_marks_and_the_header_reach_the_server() {
    use futures::StreamExt;
    use kernel::Provider;
    let reply =
        r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
    let server =
        crate::test_http::Server::scripted(vec![crate::test_http::Response::json(reply)]).await;
    let mut profile = profile(&format!("{}/v1", server.url));
    profile.prompt_cache = Some(true);
    let client = crate::ProviderClient::from_profile(profile, "").unwrap();
    client.stable_system_start("You are Medha.");
    let context = kernel::CompiledContext {
        model: String::new(),
        messages: vec![
            kernel::Message::system("You are Medha.\n\nEnvironment: today"),
            kernel::Message::user("hello"),
        ],
        ordered: None,
        tools: Vec::new(),
    };

    let mut request = client.prepare_request_in(&context, &person()).unwrap();
    request
        .headers
        .push(("x-grok-conv-id".into(), "chat-1".into()));
    let mut stream = client.stream_prepared(&request).await.unwrap();
    while stream.next().await.is_some() {}

    let (head, body) = server.requests.lock().unwrap()[0].clone();
    assert!(
        head.to_ascii_lowercase().contains("x-grok-conv-id: chat-1"),
        "{head}"
    );
    let sent: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(marked(&sent), ["s0", "s1", "1"]);
    assert_eq!(sent["messages"][0]["content"][0]["text"], "You are Medha.");
    assert_eq!(
        sent["messages"][1]["content"][0]["cache_control"]["ttl"],
        "1h"
    );
}

/// One chat keeps to one cache: a key for the one API that documents it, a
/// sticky id and a header for the router. Nothing is added for other servers,
/// which may refuse a field they do not know.
#[test]
fn a_conversation_is_named_only_to_servers_that_use_the_name() {
    let sent = |base_url: &str, model: &str, scope: &RequestScope| {
        let mut body = chat();
        let route = route(&profile(base_url), model);
        let headers = apply(&mut body, route, CacheTtl::Auto, scope, None);
        (body, headers)
    };
    let (openai, headers) = sent("https://api.openai.com/v1", "gpt-5", &person());
    let key = openai["prompt_cache_key"].as_str().unwrap().to_owned();
    assert!(key.starts_with("pck_") && key.len() <= 64, "{key}");
    assert!(openai.get("session_id").is_none() && headers.is_empty());
    let other = RequestScope {
        conversation: "chat-2".into(),
        unattended: true,
    };
    let (second, _) = sent("https://api.openai.com/v1", "gpt-5", &other);
    assert_ne!(second["prompt_cache_key"], key, "two chats, two keys");
    assert_eq!(
        sent("https://api.openai.com/v1", "gpt-5", &person()).0["prompt_cache_key"],
        key
    );

    let (routed, headers) = sent("https://openrouter.ai/api/v1", "x-ai/grok-4", &person());
    assert_eq!(routed["session_id"], "chat-1");
    assert_eq!(
        headers,
        [("x-grok-conv-id".to_string(), "chat-1".to_string())]
    );
    assert!(routed.get("prompt_cache_key").is_none());

    let (plain, headers) = sent("https://gateway.example/v1", "qwen3-6-27b", &person());
    assert_eq!(plain, chat(), "an unknown server's request is untouched");
    assert!(headers.is_empty());
    let nameless = RequestScope::default();
    let (unnamed, _) = sent("https://api.openai.com/v1", "gpt-5", &nameless);
    assert!(unnamed.get("prompt_cache_key").is_none());
}
