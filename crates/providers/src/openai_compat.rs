//! The baseline, open-first adapter: the OpenAI-compatible Chat Completions
//! API. Point `base_url` at any compatible server (local or hosted) and it
//! works with zero new code. Translates to/from the canonical `Block` so the
//! kernel stays vendor-neutral (§4.4).

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use kernel::{
    Block, CompiledContext, Message, Provider, ProviderCaps, ProviderError, ReasoningConfig,
    ReasoningEffort, Role, ToolCallStrategy, ToolIntent, Usage,
};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

pub struct OpenAiCompat {
    base_url: String,
    api_key: String,
    model: String,
    http: reqwest::Client,
    caps: ProviderCaps,
    /// Runtime-mutable (config at startup, `/think` slash command live) —
    /// interior mutability since `Provider` methods take `&self` (shared
    /// behind `Arc`, §4.4).
    reasoning: Mutex<ReasoningConfig>,
    /// Which exact-token-count route this host offers, discovered on first use
    /// (see `count_tokens`). Cached so we probe once, not every turn.
    count_probe: Mutex<ProbeState>,
}

/// A host's exact-token-count route, if any. Hosts differ: a direct vLLM serves
/// `/tokenize`; some gateways expose only an Anthropic-style
/// `/messages/count_tokens`; Ollama offers neither. So we discover, never assume.
#[derive(Clone, Copy, PartialEq)]
enum CountRoute {
    /// vLLM-native `POST {host}/tokenize` (applies the model's chat template).
    VllmTokenize,
    /// Anthropic-style `POST {base_url}/messages/count_tokens`.
    Anthropic,
}

#[derive(Clone, Copy, PartialEq)]
enum ProbeState {
    /// Not yet probed.
    Unknown,
    /// Probed — no exact-count route on this host (fall back to a local estimate).
    Unavailable,
    /// Probed — this route works.
    Route(CountRoute),
}

impl OpenAiCompat {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            http: reqwest::Client::new(),
            caps: ProviderCaps {
                vision: false,
                caching: false,
                // Unknown until discovered/configured — never a fabricated
                // constant (it would mislead the context compiler, §4.3).
                max_ctx: None,
                // Initial selection only, not an asserted capability. The
                // tool-calling ladder (§4.4) owns the runtime contract: it
                // attempts the selected strategy and downgrades on failure so a
                // schema-valid intent (or a structured parse failure) always
                // reaches the kernel (P1/P10). Discovery or config may pin a
                // lower rung up front for endpoints known to lack native calls.
                tool_calls: ToolCallStrategy::Native,
            },
            reasoning: Mutex::new(ReasoningConfig::default()),
            count_probe: Mutex::new(ProbeState::Unknown),
        }
    }

    /// Set the known context window (from discovery, config, or `medha.lock`).
    pub fn with_max_ctx(mut self, tokens: u32) -> Self {
        self.caps.max_ctx = Some(tokens);
        self
    }

    /// Pin the initial tool-calling strategy (from discovery or config) for
    /// endpoints whose capability is known ahead of time.
    pub fn with_tool_calls(mut self, strategy: ToolCallStrategy) -> Self {
        self.caps.tool_calls = strategy;
        self
    }

    /// Set the initial reasoning config (from `medha.lock`), before wrapping
    /// in `Arc`. Equivalent to `set_reasoning` but chainable at construction.
    pub fn with_reasoning(self, config: ReasoningConfig) -> Self {
        *self.reasoning.lock().unwrap() = config;
        self
    }

    /// Build `chat_template_kwargs` from the current reasoning config, for the
    /// vLLM/SGLang-style extra_body shape this adapter targets (§4.4). `None`
    /// fields are simply absent — a model/server that doesn't support reasoning
    /// just ignores unknown template variables. Two knobs, both widely honored
    /// by open reasoning models (GLM, Qwen, …) and OpenAI-compatible servers:
    ///   - `enable_thinking` (bool) — turn CoT on/off.
    ///   - `reasoning_effort` ("low"|"medium"|"high") — how hard to think. This
    ///     is the standard string knob (verified against GLM: effort=high roughly
    ///     3× the reasoning tokens of default); an unknown server ignores it.
    fn chat_template_kwargs(&self, tools_present: bool) -> Option<serde_json::Value> {
        let cfg = self.reasoning.lock().unwrap();
        let mut obj = serde_json::Map::new();
        if let Some(enabled) = cfg.enabled {
            obj.insert("enable_thinking".into(), serde_json::json!(enabled));
        }
        if let Some(effort) = cfg.effort {
            let level = match effort {
                ReasoningEffort::Low => "low",
                ReasoningEffort::Medium => "medium",
                ReasoningEffort::High => "high",
            };
            obj.insert("reasoning_effort".into(), serde_json::json!(level));
        }
        // Reasoning + tool calls can otherwise yield an empty `content` on some
        // servers; ask the template to keep it non-empty when both are active.
        if tools_present && cfg.enabled == Some(true) {
            obj.insert("force_nonempty_content".into(), serde_json::json!(true));
        }
        if obj.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(obj))
        }
    }

    /// Call one exact-count route and parse its token total. Any failure
    /// (route absent → 404, bad payload → 4xx, transport error, timeout) returns
    /// `None` so the probe can move on or the caller can fall back.
    async fn count_via(&self, route: CountRoute, messages: &[Message]) -> Option<u32> {
        let (url, body, field) = match route {
            CountRoute::VllmTokenize => (
                vllm_tokenize_url(&self.base_url),
                vllm_tokenize_body(&self.model, messages),
                "count",
            ),
            CountRoute::Anthropic => (
                format!("{}/messages/count_tokens", self.base_url.trim_end_matches('/')),
                anthropic_count_body(&self.model, messages),
                "input_tokens",
            ),
        };
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        v.get(field).and_then(|c| c.as_u64()).map(|n| n as u32)
    }
}

#[derive(Serialize)]
struct ChatReq {
    model: String,
    messages: Vec<ChatMsg>,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct StreamOptions {
    /// Ask the server to emit a final chunk with real token `usage`.
    include_usage: bool,
}

#[derive(Serialize)]
struct ChatMsg {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OutToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct OutToolCall {
    id: String,
    #[serde(rename = "type")]
    typ: &'static str, // always "function"
    function: OutFn,
}

#[derive(Serialize)]
struct OutFn {
    name: String,
    /// OpenAI requires the arguments to be a JSON *string*, not an object.
    arguments: String,
}

#[derive(Serialize)]
struct ToolDef {
    #[serde(rename = "type")]
    typ: &'static str, // always "function"
    function: FnDef,
}

#[derive(Serialize)]
struct FnDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

// ── streaming (SSE) response shapes ─────────────────────────────────────────
// Each `data: {…}` frame carries incremental deltas; tool calls arrive in
// fragments keyed by `index` and must be reassembled.

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    /// Present only on the final chunk (when include_usage is set).
    #[serde(default)]
    usage: Option<UsageRaw>,
    /// Some OpenAI-compatible gateways (OpenRouter, vLLM/SGLang proxies) emit a
    /// mid-stream `data: {"error": {...}}` frame on failure (rate limit,
    /// moderation, upstream 5xx) instead of a normal delta. Without this field
    /// it deserializes as an empty chunk and the turn silently truncates.
    #[serde(default)]
    error: Option<StreamError>,
}

#[derive(Deserialize)]
struct StreamError {
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
    /// Reasoning/thinking tokens (vLLM `--enable-reasoning` / DeepSeek-R1-style
    /// servers). Some servers spell this `reasoning` instead of
    /// `reasoning_content`; accept both.
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

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Best-effort extraction of the primary target (file path or shell command) from
/// a PARTIAL tool-call arguments JSON string, for live "writing <file>…" labels.
/// Naive on purpose (partial JSON can't be parsed): finds the first known key and
/// returns its complete quoted value if the closing quote has arrived yet.
fn sniff_target(args: &str) -> Option<String> {
    for key in ["\"path\"", "\"file_path\"", "\"command\""] {
        if let Some(i) = args.find(key) {
            let rest = args[i + key.len()..].trim_start();
            let Some(rest) = rest.strip_prefix(':') else { continue };
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix('"') else { continue };
            if let Some(end) = rest.find('"') {
                let val = &rest[..end];
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

/// Strips raw `<think>...</think>` markers out of the `content` stream,
/// routing the inner text to `Block::Reasoning`. Many self-hosted reasoning
/// models (Ollama, llama.cpp, DeepSeek-R1 without a server-side reasoning
/// parser) emit thinking this way instead of a separate `reasoning_content`
/// field — both conventions are real and this handles the inline-tag one.
/// Stateful because a tag can straddle two SSE chunks.
#[derive(Default)]
struct ThinkTagFilter {
    buf: String,
    in_think: bool,
}

const OPEN_TAG: &str = "<think>";
const CLOSE_TAG: &str = "</think>";

impl ThinkTagFilter {
    /// Feed newly-arrived content text; returns the blocks safe to emit now.
    /// Anything that might still be a partial tag is held back in `buf`.
    fn feed(&mut self, chunk: &str) -> Vec<Block> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        loop {
            let found = if self.in_think { self.buf.find(CLOSE_TAG) } else { self.buf.find(OPEN_TAG) };
            match found {
                Some(pos) => {
                    let head = self.buf[..pos].to_string();
                    let rest_start = pos + if self.in_think { CLOSE_TAG.len() } else { OPEN_TAG.len() };
                    self.buf = self.buf[rest_start..].to_string();
                    if !head.is_empty() {
                        out.push(if self.in_think { Block::Reasoning(head) } else { Block::Text(head) });
                    }
                    self.in_think = !self.in_think;
                }
                None => {
                    // Hold back a tail long enough to contain a partial tag
                    // start, so "<thi" arriving now and "nk>" next chunk still
                    // matches. Emit everything before that as safe.
                    let margin = CLOSE_TAG.len().max(OPEN_TAG.len()) - 1;
                    if self.buf.len() > margin {
                        let split = self.buf.len() - margin;
                        // Split on a char boundary at or before `split`.
                        let split = (0..=split).rev().find(|&i| self.buf.is_char_boundary(i)).unwrap_or(0);
                        let emit = self.buf[..split].to_string();
                        self.buf = self.buf[split..].to_string();
                        if !emit.is_empty() {
                            out.push(if self.in_think {
                                Block::Reasoning(emit)
                            } else {
                                Block::Text(emit)
                            });
                        }
                    }
                    break;
                }
            }
        }
        out
    }

    /// Flush any remaining buffered text at end of stream.
    fn flush(&mut self) -> Option<Block> {
        if self.buf.is_empty() {
            return None;
        }
        let rest = std::mem::take(&mut self.buf);
        Some(if self.in_think { Block::Reasoning(rest) } else { Block::Text(rest) })
    }
}

/// A model advertised by an endpoint, with whatever capability metadata the
/// server chose to expose. `context_length` is `None` when the endpoint doesn't
/// report it — never guessed (§4.3/§4.4).
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub context_length: Option<u32>,
}

/// Discover the models an OpenAI-compatible endpoint serves via `GET /v1/models`
/// (§4.4). Lets the setup wizard show a *picker* instead of asking the user to
/// type a model id blind, and captures the context window when the server
/// reports it. Servers spell that field differently, so we accept the common
/// spellings; absence stays `None`. Errors if the endpoint doesn't implement
/// `/models` so callers can fall back to manual entry.
pub async fn list_models(base_url: &str, api_key: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    #[derive(Deserialize)]
    struct ModelsResp {
        data: Vec<ModelEntry>,
    }
    #[derive(Deserialize)]
    struct ModelEntry {
        id: String,
        // Different servers expose the window under different keys; map them all.
        #[serde(default, alias = "max_model_len", alias = "context_window")]
        context_length: Option<u32>,
    }

    let url = format!("{}/models", base_url.trim_end_matches('/'));
    // Bounded timeout: this runs at startup before the UI paints, so a slow or
    // unreachable endpoint must not hang the launch (it falls back gracefully).
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(|e| ProviderError::Transport(e.to_string()))?;
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| ProviderError::Transport(e.to_string()))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::Status(status.as_u16(), body));
    }

    let parsed: ModelsResp = resp
        .json()
        .await
        .map_err(|e| ProviderError::Decode(e.to_string()))?;
    Ok(parsed
        .data
        .into_iter()
        .map(|m| ModelInfo { id: m.id, context_length: m.context_length })
        .collect())
}

fn role_str(r: &Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// vLLM's `/tokenize` lives at the server root, a sibling of `/v1` — not under
/// it. Derive the root from the (usually `…/v1`) chat base URL.
fn vllm_tokenize_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    format!("{root}/tokenize")
}

/// vLLM `/tokenize` body: messages rendered through the model's chat template.
/// Keeps every role (system/tool/assistant), so the count matches what the chat
/// endpoint would actually send.
fn vllm_tokenize_body(model: &str, messages: &[Message]) -> serde_json::Value {
    let msgs: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| serde_json::json!({ "role": role_str(&m.role), "content": m.content }))
        .collect();
    serde_json::json!({ "model": model, "messages": msgs, "add_generation_prompt": true })
}

/// Anthropic-style `/messages/count_tokens` body. That API takes `system`
/// separately and only user/assistant turns, so we hoist system messages out
/// and fold tool results into user turns — a close (not byte-exact) mapping,
/// which is fine: it's an estimate the post-turn `usage` later corrects.
fn anthropic_count_body(model: &str, messages: &[Message]) -> serde_json::Value {
    let mut system = String::new();
    let mut msgs: Vec<serde_json::Value> = Vec::new();
    let non_empty = |c: &str| if c.is_empty() { " ".to_string() } else { c.to_string() };
    for m in messages {
        match m.role {
            Role::System => {
                if !system.is_empty() {
                    system.push('\n');
                }
                system.push_str(&m.content);
            }
            Role::Assistant => {
                msgs.push(serde_json::json!({ "role": "assistant", "content": non_empty(&m.content) }))
            }
            // user + tool results both map to a user turn.
            _ => msgs.push(serde_json::json!({ "role": "user", "content": non_empty(&m.content) })),
        }
    }
    let mut body = serde_json::json!({ "model": model, "messages": msgs });
    if !system.is_empty() {
        body["system"] = serde_json::json!(system);
    }
    body
}

#[async_trait]
impl Provider for OpenAiCompat {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    fn set_reasoning(&self, config: ReasoningConfig) {
        *self.reasoning.lock().unwrap() = config;
    }

    fn reasoning(&self) -> ReasoningConfig {
        self.reasoning.lock().unwrap().clone()
    }

    /// Exact server-side token count, when the host offers one. Probes the known
    /// routes once (vLLM `/tokenize`, then Anthropic `/messages/count_tokens`),
    /// caches the result, and returns `None` if neither works — so the same code
    /// gives exact counts on hosts that support it and degrades gracefully
    /// (to the caller's local estimate) on those that don't. No host-specific
    /// assumptions. Opt out entirely with `MEDHA_EXACT_TOKENS=off`.
    async fn count_tokens(&self, messages: &[Message]) -> Option<u32> {
        if std::env::var("MEDHA_EXACT_TOKENS").is_ok_and(|v| v == "off" || v == "0") {
            return None;
        }
        let state = *self.count_probe.lock().unwrap();
        match state {
            ProbeState::Unavailable => None,
            ProbeState::Route(route) => self.count_via(route, messages).await,
            ProbeState::Unknown => {
                for route in [CountRoute::VllmTokenize, CountRoute::Anthropic] {
                    if let Some(n) = self.count_via(route, messages).await {
                        *self.count_probe.lock().unwrap() = ProbeState::Route(route);
                        return Some(n);
                    }
                }
                *self.count_probe.lock().unwrap() = ProbeState::Unavailable;
                None
            }
        }
    }

    async fn stream(
        &self,
        ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        // Translate canonical messages → OpenAI shape, carrying tool calls and
        // tool results so the native tool-calling protocol round-trips (§4.4).
        let messages: Vec<ChatMsg> = ctx
            .messages
            .iter()
            .map(|m| ChatMsg {
                role: role_str(&m.role),
                content: m.content.clone(),
                tool_calls: m
                    .tool_calls
                    .iter()
                    .map(|tc| OutToolCall {
                        id: tc.id.clone(),
                        typ: "function",
                        function: OutFn {
                            name: tc.tool.clone(),
                            arguments: tc.args.to_string(),
                        },
                    })
                    .collect(),
                tool_call_id: m.tool_call_id.clone(),
            })
            .collect();

        // Expose the K2 capability sheath as OpenAI tool definitions.
        let tools: Vec<ToolDef> = ctx
            .tools
            .iter()
            .map(|t| ToolDef {
                typ: "function",
                function: FnDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.schema.clone(),
                },
            })
            .collect();

        let model = if ctx.model.is_empty() { self.model.clone() } else { ctx.model.clone() };

        // Real SSE streaming: text deltas surface token-by-token; tool calls
        // arrive as fragments keyed by index and are reassembled, then emitted
        // once complete (§4.4).
        let chat_template_kwargs = self.chat_template_kwargs(!tools.is_empty());
        let req = ChatReq {
            model,
            messages,
            stream: true,
            tools,
            stream_options: Some(StreamOptions { include_usage: true }),
            chat_template_kwargs,
        };
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        // Direct proof of what actually goes over the wire — no guessing, no
        // separate curl experiment needed. Set MEDHA_DEBUG_HTTP=1 to see the
        // exact outgoing JSON (including chat_template_kwargs) for every call.
        if std::env::var("MEDHA_DEBUG_HTTP").is_ok_and(|v| v == "1") {
            let body = serde_json::to_string_pretty(&req).unwrap_or_default();
            eprintln!("\n[MEDHA_DEBUG_HTTP] POST {url}\n{body}\n");
        }

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Status(status.as_u16(), body));
        }

        let byte_stream = resp.bytes_stream();
        let s = async_stream::stream! {
            use std::collections::BTreeMap;
            let mut buf: Vec<u8> = Vec::new();
            // index → (id, name, accumulated-arguments)
            let mut accum: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
            // tool-call indices whose target (path/command) we've already surfaced.
            let mut target_announced: std::collections::HashSet<u32> = std::collections::HashSet::new();
            let mut think_filter = ThinkTagFilter::default();

            futures::pin_mut!(byte_stream);
            while let Some(chunk) = byte_stream.next().await {
                match chunk {
                    Err(e) => {
                        yield Err(ProviderError::Transport(e.to_string()));
                        return;
                    }
                    Ok(bytes) => buf.extend_from_slice(&bytes),
                }

                // Drain complete SSE records. A record ends at a blank line —
                // "\n\n" (LF) or the spec-legal "\r\n\r\n" (CRLF). Handling CRLF
                // is essential: without it a CRLF server's records never match,
                // the buffer grows unbounded, and the whole response is silently
                // dropped.
                while let Some((pos, sep_len)) = find_record_boundary(&buf) {
                    let record: Vec<u8> = buf.drain(..pos + sep_len).collect();
                    let record = String::from_utf8_lossy(&record);
                    match process_sse_record(&record, &mut accum, &mut think_filter, &mut target_announced) {
                        Ok(blocks) => for b in blocks { yield Ok(b); },
                        Err(e) => { yield Err(e); return; }
                    }
                }
            }

            // Drain any trailing partial record the server never terminated with
            // a blank line — otherwise its final frame (often the one carrying
            // the last tool-call args or usage) is thrown away at EOF.
            if !buf.is_empty() {
                let record = String::from_utf8_lossy(&buf);
                match process_sse_record(&record, &mut accum, &mut think_filter, &mut target_announced) {
                    Ok(blocks) => for b in blocks { yield Ok(b); },
                    Err(e) => { yield Err(e); return; }
                }
            }

            if let Some(block) = think_filter.flush() {
                yield Ok(block);
            }

            // Emit fully-assembled tool calls, in the order the model issued them.
            for intent in finalize_tool_calls(accum) {
                yield Ok(Block::ToolIntent(intent));
            }
        };

        Ok(s.boxed())
    }
}

/// The end of the first complete SSE record in `buf`: a blank-line separator,
/// which is `"\n\n"` on LF servers or `"\r\n\r\n"` on spec-legal CRLF servers.
/// Returns `(offset of the separator, its length)`, whichever variant appears
/// earliest. Without the CRLF case a CRLF server's records never drain.
fn find_record_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = find_subslice(buf, b"\n\n").map(|p| (p, 2usize));
    let crlf = find_subslice(buf, b"\r\n\r\n").map(|p| (p, 4usize));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Turn accumulated tool-call fragments into final intents, in issue order.
/// Synthesizes `call_{idx}` for any call the stream never gave an id — some
/// gateways (llama.cpp and others) omit it, and an empty `tool_call_id` 400s on
/// strict backends when the result is sent back.
fn finalize_tool_calls(
    accum: std::collections::BTreeMap<u32, (String, String, String)>,
) -> Vec<ToolIntent> {
    let mut out = Vec::new();
    for (idx, (id, name, args)) in accum {
        if name.is_empty() {
            continue;
        }
        let id = if id.is_empty() { format!("call_{idx}") } else { id };
        let parsed = if args.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&args).unwrap_or_else(|_| serde_json::json!({ "_raw": args }))
        };
        out.push(ToolIntent { id, tool: name, args: parsed });
    }
    out
}

/// Parse one SSE record's `data:` lines into the blocks to yield, folding
/// tool-call deltas into `accum`. Shared by the mid-stream drain loop and the
/// end-of-stream residual drain so both paths behave identically. Returns
/// `Err` for a mid-stream error frame (ends the turn with a real error rather
/// than a silent truncation). `str::lines()` already tolerates `\r\n`, so only
/// the record *delimiter* needed CRLF handling (see `find_record_boundary`).
fn process_sse_record(
    record: &str,
    accum: &mut std::collections::BTreeMap<u32, (String, String, String)>,
    think_filter: &mut ThinkTagFilter,
    target_announced: &mut std::collections::HashSet<u32>,
) -> Result<Vec<Block>, ProviderError> {
    let mut out = Vec::new();
    for line in record.lines() {
        let Some(data) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
            continue; // tolerate keep-alives / non-JSON frames
        };
        if let Some(err) = chunk.error {
            let msg = match (err.message, err.kind) {
                (Some(m), Some(k)) => format!("{m} ({k})"),
                (Some(m), None) => m,
                (None, Some(k)) => k,
                (None, None) => "provider returned an error frame".to_string(),
            };
            return Err(ProviderError::Stream(msg));
        }
        if let Some(u) = chunk.usage {
            out.push(Block::Usage(Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            }));
        }
        if let Some(choice) = chunk.choices.into_iter().next() {
            if let Some(r) = choice.delta.reasoning_content {
                if !r.is_empty() {
                    out.push(Block::Reasoning(r));
                }
            }
            if let Some(c) = choice.delta.content {
                if !c.is_empty() {
                    out.extend(think_filter.feed(&c));
                }
            }
            for tc in choice.delta.tool_calls {
                let idx = tc.index;
                let e = accum.entry(idx).or_default();
                if let Some(id) = tc.id {
                    if !id.is_empty() {
                        e.0 = id;
                    }
                }
                if let Some(f) = tc.function {
                    if let Some(n) = f.name {
                        if !n.is_empty() {
                            let first = e.1.is_empty();
                            e.1 = n.clone();
                            // Surface the tool name the moment it's known so the UI
                            // can show "writing…/reading…" while the (possibly huge)
                            // arguments are still streaming in.
                            if first {
                                out.push(Block::ToolStarted { name: n, target: None });
                            }
                        }
                    }
                    if let Some(a) = f.arguments {
                        e.2.push_str(&a);
                        // The path/command usually appears at the START of the args
                        // JSON (before a huge `content` field), so we can surface
                        // "writing medha.html…" long before the write finishes.
                        if !e.1.is_empty() && !target_announced.contains(&idx) {
                            if let Some(t) = sniff_target(&e.2) {
                                target_announced.insert(idx);
                                out.push(Block::ToolStarted { name: e.1.clone(), target: Some(t) });
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod sniff_tests {
    use super::sniff_target;

    #[test]
    fn extracts_path_from_partial_args_before_content_finishes() {
        // Path is complete but the huge `content` is still streaming — we still get it.
        assert_eq!(
            sniff_target(r#"{"path":"medha.html","content":"<!doctype html"#),
            Some("medha.html".to_string())
        );
    }

    #[test]
    fn none_until_closing_quote_arrives() {
        assert_eq!(sniff_target(r#"{"file_path": "src/mai"#), None);
        assert_eq!(sniff_target(r#"{"file_path":"src/main.rs"}"#), Some("src/main.rs".to_string()));
    }

    #[test]
    fn command_and_unknown_keys() {
        assert_eq!(sniff_target(r#"{"command":"cargo build"}"#), Some("cargo build".to_string()));
        assert_eq!(sniff_target(r#"{"pattern":"foo"}"#), None);
    }
}

#[cfg(test)]
mod sse_tests {
    use super::*;
    use std::collections::{BTreeMap, HashSet};

    // ── K4: record boundary must accept BOTH LF and CRLF blank-line separators ──
    #[test]
    fn record_boundary_handles_lf_and_crlf() {
        assert_eq!(find_record_boundary(b"data: x\n\ndata: y"), Some((7, 2)));
        // CRLF: "data: x\r\n\r\n..." — the LF-only search would never match here.
        assert_eq!(find_record_boundary(b"data: x\r\n\r\nrest"), Some((7, 4)));
        // No blank line yet → no complete record.
        assert_eq!(find_record_boundary(b"data: partial\r\n"), None);
    }

    fn drive(record: &str) -> Vec<Block> {
        let mut accum = BTreeMap::new();
        let mut tf = ThinkTagFilter::default();
        let mut announced = HashSet::new();
        let mut blocks = process_sse_record(record, &mut accum, &mut tf, &mut announced).unwrap();
        // The think-tag filter holds a tail back pending a possible `<think>`;
        // flush at end-of-stream (as the real driver does) to emit it.
        if let Some(b) = tf.flush() {
            blocks.push(b);
        }
        blocks
    }

    #[test]
    fn crlf_record_is_parsed_not_dropped() {
        // A CRLF-delimited content frame must yield its text (previously the
        // whole response was silently dropped on CRLF servers).
        let blocks = drive("data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\r\n");
        let text: String = blocks.iter().filter_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            _ => None,
        }).collect();
        assert_eq!(text, "hello", "CRLF record must be parsed, not dropped: {blocks:?}");
    }

    // ── K13: a tool call the server never gave an id gets a synthesized one ─────
    #[test]
    fn missing_tool_call_id_is_synthesized() {
        let mut accum = BTreeMap::new();
        // index 0, a name, no id (empty) — as a lax gateway streams it.
        accum.insert(0u32, (String::new(), "fs.read".to_string(), r#"{"path":"a"}"#.to_string()));
        let intents = finalize_tool_calls(accum);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].id, "call_0", "empty id must be synthesized, not left blank");
        assert_eq!(intents[0].tool, "fs.read");
        assert_eq!(intents[0].args["path"], "a");
    }

    #[test]
    fn present_tool_call_id_is_preserved() {
        let mut accum = BTreeMap::new();
        accum.insert(0u32, ("real-id".to_string(), "fs.read".to_string(), String::new()));
        let intents = finalize_tool_calls(accum);
        assert_eq!(intents[0].id, "real-id");
        assert_eq!(intents[0].args, serde_json::json!({}), "empty args → empty object");
    }
}

#[cfg(test)]
mod think_tag_tests {
    use super::*;

    fn collect(blocks: &[Block]) -> (String, String) {
        let mut text = String::new();
        let mut reasoning = String::new();
        for b in blocks {
            match b {
                Block::Text(t) => text.push_str(t),
                Block::Reasoning(r) => reasoning.push_str(r),
                _ => {}
            }
        }
        (text, reasoning)
    }

    #[test]
    fn strips_think_tags_in_one_chunk() {
        let mut f = ThinkTagFilter::default();
        let blocks = f.feed("before <think>hmm let me consider</think> after");
        let flushed = f.flush();
        let mut all = blocks;
        all.extend(flushed);
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "before  after");
        assert_eq!(reasoning, "hmm let me consider");
    }

    #[test]
    fn handles_tag_split_across_chunks() {
        let mut f = ThinkTagFilter::default();
        let mut all = Vec::new();
        // "<think>" itself split mid-tag, and "</think>" split mid-tag too.
        all.extend(f.feed("start <thi"));
        all.extend(f.feed("nk>reasoning here</th"));
        all.extend(f.feed("ink> end"));
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "start  end");
        assert_eq!(reasoning, "reasoning here");
    }

    #[test]
    fn no_tags_passes_through_as_text() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("just a normal reply, no thinking here");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "just a normal reply, no thinking here");
        assert!(reasoning.is_empty());
    }
}

#[cfg(test)]
mod count_tokens_tests {
    use super::*;

    #[test]
    fn tokenize_url_is_server_root_sibling_of_v1() {
        assert_eq!(vllm_tokenize_url("https://h.example/v1"), "https://h.example/tokenize");
        assert_eq!(vllm_tokenize_url("https://h.example/v1/"), "https://h.example/tokenize");
        // No /v1 suffix → append at the given root.
        assert_eq!(vllm_tokenize_url("https://h.example"), "https://h.example/tokenize");
    }

    #[test]
    fn anthropic_body_hoists_system_and_folds_tools_into_user() {
        let msgs = vec![
            Message::system("be terse"),
            Message::user("hello"),
            Message::assistant_calls("", vec![]), // empty assistant content
            Message::tool_result("c1", "tool output"),
        ];
        let body = anthropic_count_body("m", &msgs);
        assert_eq!(body["system"], serde_json::json!("be terse"));
        let arr = body["messages"].as_array().unwrap();
        // system is hoisted out; the other three remain.
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[1]["content"], " "); // empty content padded to non-empty
        assert_eq!(arr[2]["role"], "user"); // tool result folded to user
        assert_eq!(arr[2]["content"], "tool output");
    }

    #[test]
    fn vllm_body_keeps_all_roles_for_the_chat_template() {
        let msgs = vec![Message::system("s"), Message::user("u"), Message::tool_result("c1", "t")];
        let body = vllm_tokenize_body("m", &msgs);
        assert_eq!(body["add_generation_prompt"], serde_json::json!(true));
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[2]["role"], "tool");
    }
}

#[cfg(test)]
mod reasoning_request_tests {
    use super::*;

    /// Tests the REAL production method (not a standalone copy) — proves the
    /// actual `OpenAiCompat` the kernel uses builds `chat_template_kwargs`
    /// correctly. If a request over the wire doesn't carry this field despite
    /// `/think on`, this test passing means the bug is downstream of us (a
    /// proxy/gateway stripping the field), not in this construction step.
    #[test]
    fn think_on_produces_enable_thinking_true() {
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_reasoning(ReasoningConfig { enabled: Some(true), effort: None });
        let kwargs = p.chat_template_kwargs(false).expect("must be Some when enabled");
        assert_eq!(kwargs["enable_thinking"], serde_json::json!(true));
    }

    #[test]
    fn think_off_produces_enable_thinking_false() {
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_reasoning(ReasoningConfig { enabled: Some(false), effort: None });
        let kwargs = p.chat_template_kwargs(false).expect("must be Some when explicitly off");
        assert_eq!(kwargs["enable_thinking"], serde_json::json!(false));
    }

    #[test]
    fn untouched_reasoning_sends_no_kwargs_at_all() {
        let p = OpenAiCompat::new("http://x", "", "m");
        assert!(p.chat_template_kwargs(false).is_none());
    }

    #[test]
    fn full_request_json_actually_contains_the_field() {
        // End-to-end through serde, exactly as it would serialize onto the wire.
        let p = OpenAiCompat::new("http://x", "", "m")
            .with_reasoning(ReasoningConfig { enabled: Some(true), effort: Some(ReasoningEffort::Medium) });
        let req = ChatReq {
            model: "m".into(),
            messages: vec![],
            stream: true,
            tools: vec![],
            stream_options: None,
            chat_template_kwargs: p.chat_template_kwargs(false),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("chat_template_kwargs"), "field missing from wire JSON: {json}");
        assert!(json.contains("enable_thinking"));
        // The standard string effort knob (GLM/Qwen/OpenAI-compatible), not a
        // vendor-specific boolean.
        assert!(json.contains("reasoning_effort"), "wire JSON: {json}");
        assert!(json.contains("\"medium\""), "wire JSON: {json}");
    }
}
