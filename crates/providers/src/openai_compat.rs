//! The baseline, open-first adapter: the OpenAI-compatible Chat Completions
//! API. Point `base_url` at any compatible server (local or hosted) and it
//! works with zero new code. Translates to/from the canonical `Block` so the
//! kernel stays vendor-neutral (§4.4).

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use kernel::{
    Block, CompiledContext, Message, Provider, ProviderCaps, ProviderError, ReasoningConfig,
    ReasoningEffort, Role, ToolCallStrategy, ToolIntent, Usage,
};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

pub struct OpenAiCompat {
    /// Active connection is mutable between turns so an interactive surface can
    /// switch a saved model profile without rebuilding the kernel. A stream
    /// snapshots it before its first await, so one request always uses exactly
    /// one endpoint/model/key tuple.
    connection: Mutex<Connection>,
    http: reqwest::Client,
    caps: ProviderCaps,
    /// Runtime-mutable (config at startup, `/think` slash command live) —
    /// interior mutability since `Provider` methods take `&self` (shared
    /// behind `Arc`, §4.4).
    reasoning: Mutex<ReasoningConfig>,
    /// SSE streaming on/off (`/stream` slash command). Off → one blocking
    /// request, whole response yielded at once. Same interior-mutability reason
    /// as `reasoning`.
    streaming: std::sync::atomic::AtomicBool,
    /// Which exact-token-count route this host offers, discovered on first use
    /// (see `count_tokens`). Cached so we probe once, not every turn.
    count_probe: Mutex<ProbeState>,
}

#[derive(Clone)]
struct Connection {
    base_url: String,
    api_key: String,
    model: String,
    max_ctx: Option<u32>,
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
    /// Probed — no exact-count route on this host (fall back to a local
    /// estimate). Carries the calls since the failed probe: after
    /// `REPROBE_AFTER` we probe again (K16 — a transient startup failure must
    /// not disable exact counting for the whole process).
    Unavailable(u32),
    /// Probed — this route works.
    Route(CountRoute),
}

/// Re-try a failed count-tokens probe after this many skipped calls (K16).
const REPROBE_AFTER: u32 = 20;

/// Add Authorization only when a credential exists. Accepting a pasted
/// `Bearer …` value here also prevents a malformed double scheme at the final
/// network boundary, regardless of which configuration path supplied it.
fn with_bearer(request: reqwest::RequestBuilder, api_key: &str) -> reqwest::RequestBuilder {
    let key = api_key.trim();
    if key.is_empty() || key.eq_ignore_ascii_case("bearer") {
        return request;
    }
    let key = match key.split_once(char::is_whitespace) {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => token.trim(),
        _ => key,
    };
    if key.is_empty() {
        request
    } else {
        request.bearer_auth(key)
    }
}

impl OpenAiCompat {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            connection: Mutex::new(Connection {
                base_url: base_url.into(),
                api_key: api_key.into(),
                model: model.into(),
                max_ctx: None,
            }),
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
            streaming: std::sync::atomic::AtomicBool::new(true),
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

    /// Switch the active OpenAI-compatible connection. Safe callers invoke
    /// this only between turns; [`Provider::stream`] snapshots the connection
    /// before I/O so an already-running request is never retargeted midstream.
    pub fn switch_connection(
        &self,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) {
        self.switch_connection_with_context(base_url, api_key, model, None);
    }

    /// Same as [`Self::switch_connection`], with the context limit discovered
    /// or configured for the selected profile. This value feeds the kernel's
    /// context compiler on the very next turn.
    pub fn switch_connection_with_context(
        &self,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        max_ctx: Option<u32>,
    ) {
        *self.connection.lock().unwrap() = Connection {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            max_ctx,
        };
        // Exact-token routes are endpoint-specific; discover again after a
        // profile switch instead of carrying an incompatible cached route.
        *self.count_probe.lock().unwrap() = ProbeState::Unknown;
    }

    pub fn active_model(&self) -> String {
        self.connection.lock().unwrap().model.clone()
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
        let connection = self.connection.lock().unwrap().clone();
        let (url, body, field) = match route {
            CountRoute::VllmTokenize => (
                vllm_tokenize_url(&connection.base_url),
                vllm_tokenize_body(&connection.model, messages),
                "count",
            ),
            CountRoute::Anthropic => (
                format!(
                    "{}/messages/count_tokens",
                    connection.base_url.trim_end_matches('/')
                ),
                anthropic_count_body(&connection.model, messages),
                "input_tokens",
            ),
        };
        let resp = with_bearer(self.http.post(&url), &connection.api_key)
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

// ── non-streaming response shapes ───────────────────────────────────────────
// One aggregated `message` per choice. Some gateways only populate
// `reasoning_content` here (not in streamed deltas), so this path is the way to
// surface reasoning on those hosts.

#[derive(Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    choices: Vec<CompletionChoice>,
    #[serde(default)]
    usage: Option<UsageRaw>,
    #[serde(default)]
    error: Option<StreamError>,
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
            let Some(rest) = rest.strip_prefix(':') else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix('"') else {
                continue;
            };
            // Scan to the first UNESCAPED quote (K19): a command like
            // `echo \"hi\"` must not be cut at its inner escaped quote.
            let mut val = String::new();
            let mut chars = rest.chars();
            let mut closed = false;
            while let Some(c) = chars.next() {
                match c {
                    '\\' => match chars.next() {
                        Some('n') | Some('t') => val.push(' '),
                        Some(e) => val.push(e),
                        None => break, // escape straddles a chunk boundary — incomplete
                    },
                    '"' => {
                        closed = true;
                        break;
                    }
                    c => val.push(c),
                }
            }
            if closed && !val.is_empty() {
                return Some(val);
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
/// A tag counts only at the start of a line (see `line_has_content`); this is
/// a fallback for servers running without a reasoning parser — when the
/// server provides `reasoning_content`, that path is preferred and this
/// filter never fires. Stateful because a tag can straddle two SSE chunks.
#[derive(Default)]
struct ThinkTagFilter {
    buf: String,
    in_think: bool,
    /// The current visible output line already carries non-whitespace text.
    /// Thinking blocks are template-structural: every inline-thinking chat
    /// template emits `<think>` at the start of its own line (stream start,
    /// or right after a newline). A tag surfacing MID-line is therefore the
    /// model *writing about* the tag — docs, code samples — and must stay
    /// literal. Matching it anywhere used to reroute the rest of a reply
    /// into hidden reasoning the moment the answer mentioned the tag.
    line_has_content: bool,
}

const OPEN_TAG: &str = "<think>";
const CLOSE_TAG: &str = "</think>";

impl ThinkTagFilter {
    /// Track whether the visible line still open after emitting `s` has any
    /// non-whitespace on it (drives the line-start rule for later chunks).
    fn note_emitted(&mut self, s: &str) {
        match s.rfind('\n') {
            Some(i) => self.line_has_content = !s[i + 1..].trim().is_empty(),
            None => self.line_has_content = self.line_has_content || !s.trim().is_empty(),
        }
    }

    /// True when a tag found right after `prefix` sits at a line start —
    /// everything on its line so far (buffered here or already emitted) is
    /// whitespace.
    fn at_line_start(&self, prefix: &str) -> bool {
        match prefix.rfind('\n') {
            Some(i) => prefix[i + 1..].trim().is_empty(),
            None => !self.line_has_content && prefix.trim().is_empty(),
        }
    }

    /// Feed newly-arrived content text; returns the blocks safe to emit now.
    /// A tail that might still be a partial tag is held back in `buf`.
    fn feed(&mut self, chunk: &str) -> Vec<Block> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        loop {
            if self.in_think {
                match self.buf.find(CLOSE_TAG) {
                    Some(pos) => {
                        let head = self.buf[..pos].to_string();
                        self.buf = self.buf[pos + CLOSE_TAG.len()..].to_string();
                        if !head.is_empty() {
                            out.push(Block::Reasoning(head));
                        }
                        self.in_think = false;
                        // A stripped block leaves its line visibly empty, so a
                        // back-to-back follow-up block still opens at a line start.
                        self.line_has_content = false;
                    }
                    None => {
                        self.hold_margin(&mut out);
                        break;
                    }
                }
            } else {
                // Open only at a line start; mid-line occurrences are literal
                // content and are skipped (a later line-start one still opens).
                let opener = self
                    .buf
                    .match_indices(OPEN_TAG)
                    .map(|(pos, _)| pos)
                    .find(|&pos| self.at_line_start(&self.buf[..pos]));
                match opener {
                    Some(pos) => {
                        let head = self.buf[..pos].to_string();
                        self.buf = self.buf[pos + OPEN_TAG.len()..].to_string();
                        if !head.is_empty() {
                            self.note_emitted(&head);
                            out.push(Block::Text(head));
                        }
                        self.in_think = true;
                    }
                    None => {
                        self.hold_margin(&mut out);
                        break;
                    }
                }
            }
        }
        out
    }

    /// Emit everything except a tail long enough to hide a partial tag —
    /// "<thi" arriving now and "nk>" next chunk must still match. The split
    /// lands on a char boundary.
    fn hold_margin(&mut self, out: &mut Vec<Block>) {
        let margin = CLOSE_TAG.len().max(OPEN_TAG.len()) - 1;
        if self.buf.len() > margin {
            let split = self.buf.len() - margin;
            let split = (0..=split)
                .rev()
                .find(|&i| self.buf.is_char_boundary(i))
                .unwrap_or(0);
            let emit = self.buf[..split].to_string();
            self.buf = self.buf[split..].to_string();
            if !emit.is_empty() {
                if self.in_think {
                    out.push(Block::Reasoning(emit));
                } else {
                    self.note_emitted(&emit);
                    out.push(Block::Text(emit));
                }
            }
        }
    }

    /// Flush any remaining buffered text at end of stream.
    fn flush(&mut self) -> Option<Block> {
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
    let resp = with_bearer(client.get(&url), api_key)
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
        .map(|m| ModelInfo {
            id: m.id,
            context_length: m.context_length,
        })
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

/// Translate canonical messages → OpenAI/vLLM chat shape, hoisting ALL system
/// content into a single leading message. vLLM/LiteLLM reject a `system` message
/// that isn't at the very start. Upstream keeps system content at index 0 (the
/// compactor no longer inserts mid-array system messages), but this enforces the
/// wire-format invariant at the boundary that owns it — a durable guarantee
/// against any future upstream regression. Non-system messages keep their exact
/// order, so tool-call ↔ tool-result pairing is untouched.
///
/// Tool-call names in assistant history are also sanitized to the wire form
/// (`fs.edit` → `fs_edit`): strict OpenAI-compat backends validate names in the
/// message history, not just the tool defs, so a prior dotted call would 400 on
/// the next turn.
fn build_chat_messages(messages: &[Message]) -> Vec<ChatMsg> {
    let mut system = String::new();
    let mut rest: Vec<ChatMsg> = Vec::with_capacity(messages.len());
    for m in messages {
        if m.role == Role::System {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(&m.content);
            continue;
        }
        rest.push(ChatMsg {
            role: role_str(&m.role),
            content: m.content.clone(),
            tool_calls: m
                .tool_calls
                .iter()
                .map(|tc| OutToolCall {
                    id: tc.id.clone(),
                    typ: "function",
                    function: OutFn {
                        name: wire_tool_name(&tc.tool),
                        arguments: tc.args.to_string(),
                    },
                })
                .collect(),
            tool_call_id: m.tool_call_id.clone(),
        });
    }
    let mut out = Vec::with_capacity(rest.len() + 1);
    if !system.is_empty() {
        out.push(ChatMsg {
            role: "system",
            content: system,
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    out.extend(rest);
    out
}

/// vLLM's `/tokenize` lives at the server root, a sibling of `/v1` — not under
/// it. Derive the root from the (usually `…/v1`) chat base URL.
fn vllm_tokenize_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    format!("{root}/tokenize")
}

/// vLLM `/tokenize` body: messages rendered through the model's chat template.
/// Keeps every role (system/tool/assistant) and mirrors the same system-hoist
/// as `build_chat_messages`, so the count matches what the chat endpoint would
/// actually send — and a strict template can't reject a mid-array `system`.
fn vllm_tokenize_body(model: &str, messages: &[Message]) -> serde_json::Value {
    let mut system = String::new();
    let mut msgs: Vec<serde_json::Value> = Vec::with_capacity(messages.len());
    for m in messages {
        if m.role == Role::System {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(&m.content);
            continue;
        }
        msgs.push(serde_json::json!({ "role": role_str(&m.role), "content": m.content }));
    }
    if !system.is_empty() {
        msgs.insert(0, serde_json::json!({ "role": "system", "content": system }));
    }
    serde_json::json!({ "model": model, "messages": msgs, "add_generation_prompt": true })
}

/// Anthropic-style `/messages/count_tokens` body. That API takes `system`
/// separately and only user/assistant turns, so we hoist system messages out
/// and fold tool results into user turns — a close (not byte-exact) mapping,
/// which is fine: it's an estimate the post-turn `usage` later corrects.
fn anthropic_count_body(model: &str, messages: &[Message]) -> serde_json::Value {
    let mut system = String::new();
    let mut msgs: Vec<serde_json::Value> = Vec::new();
    let non_empty = |c: &str| {
        if c.is_empty() {
            " ".to_string()
        } else {
            c.to_string()
        }
    };
    for m in messages {
        match m.role {
            Role::System => {
                if !system.is_empty() {
                    system.push('\n');
                }
                system.push_str(&m.content);
            }
            Role::Assistant => msgs
                .push(serde_json::json!({ "role": "assistant", "content": non_empty(&m.content) })),
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

    fn context_window(&self) -> Option<u32> {
        self.connection
            .lock()
            .unwrap()
            .max_ctx
            .or(self.caps.max_ctx)
    }

    fn set_reasoning(&self, config: ReasoningConfig) {
        *self.reasoning.lock().unwrap() = config;
    }

    fn reasoning(&self) -> ReasoningConfig {
        self.reasoning.lock().unwrap().clone()
    }

    fn set_streaming(&self, on: bool) {
        self.streaming.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    fn streaming(&self) -> bool {
        self.streaming.load(std::sync::atomic::Ordering::Relaxed)
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
            ProbeState::Unavailable(n) if n < REPROBE_AFTER => {
                *self.count_probe.lock().unwrap() = ProbeState::Unavailable(n + 1);
                None
            }
            ProbeState::Route(route) => self.count_via(route, messages).await,
            // Unknown, or Unavailable long enough that it's worth re-probing.
            ProbeState::Unknown | ProbeState::Unavailable(_) => {
                for route in [CountRoute::VllmTokenize, CountRoute::Anthropic] {
                    if let Some(n) = self.count_via(route, messages).await {
                        *self.count_probe.lock().unwrap() = ProbeState::Route(route);
                        return Some(n);
                    }
                }
                *self.count_probe.lock().unwrap() = ProbeState::Unavailable(0);
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
        // System content is hoisted to a single leading message (vLLM requires
        // `system` at the very beginning; compaction can insert a mid-array one).
        let messages = build_chat_messages(&ctx.messages);

        // Expose the K2 capability sheath as OpenAI tool definitions, with names
        // sanitized to the strict OpenAI contract ([a-zA-Z0-9_-]) — canonical
        // dotted names (`fs.edit`) 400 on strict backends (NVIDIA NIM, OpenAI,
        // most hosted OpenAI-compat gateways). `names` maps the wire form back
        // when the model calls a tool; vLLM (permissive) is unaffected.
        let names = wire_name_map(&ctx.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>());
        let tools: Vec<ToolDef> = ctx
            .tools
            .iter()
            .map(|t| ToolDef {
                typ: "function",
                function: FnDef {
                    name: wire_tool_name(&t.name),
                    description: t.description.clone(),
                    parameters: t.schema.clone(),
                },
            })
            .collect();

        let connection = self.connection.lock().unwrap().clone();
        let model = if ctx.model.is_empty() {
            connection.model.clone()
        } else {
            ctx.model.clone()
        };

        // Real SSE streaming: text deltas surface token-by-token; tool calls
        // arrive as fragments keyed by index and are reassembled, then emitted
        // once complete (§4.4).
        let streaming = self.streaming();
        let chat_template_kwargs = self.chat_template_kwargs(!tools.is_empty());
        let req = ChatReq {
            model,
            messages,
            stream: streaming,
            tools,
            // `stream_options` is only meaningful with streaming on; some strict
            // servers reject it on a non-streamed request.
            stream_options: streaming.then_some(StreamOptions { include_usage: true }),
            chat_template_kwargs,
        };
        let url = format!(
            "{}/chat/completions",
            connection.base_url.trim_end_matches('/')
        );

        // Direct proof of what actually goes over the wire — no guessing, no
        // separate curl experiment needed. Set MEDHA_DEBUG_HTTP=1 to see the
        // exact outgoing JSON (including chat_template_kwargs) for every call.
        if std::env::var("MEDHA_DEBUG_HTTP").is_ok_and(|v| v == "1") {
            let body = serde_json::to_string_pretty(&req).unwrap_or_default();
            eprintln!("\n[MEDHA_DEBUG_HTTP] POST {url}\n{body}\n");
        }

        let resp = with_bearer(self.http.post(&url), &connection.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Status(status.as_u16(), body));
        }

        // Non-streaming: one blocking body, parsed and yielded as a single batch
        // of blocks (reasoning → text → tool intents → usage). The kernel loop
        // is identical; only the arrival shape differs.
        if !streaming {
            let body = resp.text().await.map_err(|e| ProviderError::Transport(e.to_string()))?;
            let blocks = parse_completion(&body, &names)?;
            return Ok(futures::stream::iter(blocks.into_iter().map(Ok)).boxed());
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
                        // Surface what the filter is still holding before the
                        // error — otherwise the reply's tail silently vanishes.
                        if let Some(b) = think_filter.flush() {
                            yield Ok(b);
                        }
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
            for intent in finalize_tool_calls(accum, &names) {
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

/// Canonical tool name → the name sent on the wire. Strict OpenAI-compatible
/// validators (NVIDIA API among them) enforce `[a-zA-Z0-9_-]+` for function
/// names, so the dotted canonical names (`fs.edit`) are mapped (`fs_edit`) at
/// this boundary — the rest of the system never sees wire names.
fn wire_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// Wire→canonical map for the tool names exposed this request. Collisions get a
/// trailing `_` so two canonical names can never share a wire name.
fn wire_name_map(canonical: &[String]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for name in canonical {
        let mut wire = wire_tool_name(name);
        while map.contains_key(&wire) {
            wire.push('_');
        }
        map.insert(wire, name.clone());
    }
    map
}

/// Parse a non-streamed chat completion into the same blocks the SSE path
/// yields, in the same order (reasoning → text → tool intents → usage). Wire
/// tool names map back to canonical, matching `finalize_tool_calls`.
fn parse_completion(
    body: &str,
    names: &std::collections::HashMap<String, String>,
) -> Result<Vec<Block>, ProviderError> {
    let parsed: ChatCompletion = serde_json::from_str(body)
        .map_err(|e| ProviderError::Stream(format!("non-streaming response parse: {e}")))?;
    if let Some(err) = parsed.error {
        let msg = err.message.or(err.kind).unwrap_or_else(|| "provider error".into());
        return Err(ProviderError::Stream(msg));
    }
    let mut out = Vec::new();
    if let Some(choice) = parsed.choices.into_iter().next() {
        let msg = choice.message;
        if let Some(r) = msg.reasoning_content.filter(|s| !s.is_empty()) {
            out.push(Block::Reasoning(r));
        }
        if let Some(c) = msg.content.filter(|s| !s.is_empty()) {
            // A model that emits `<think>` tags inline (no reasoning_content
            // field) still gets split into reasoning/text.
            let mut filter = ThinkTagFilter::default();
            out.extend(filter.feed(&c));
            if let Some(b) = filter.flush() {
                out.push(b);
            }
        }
        for (i, tc) in msg.tool_calls.into_iter().enumerate() {
            let Some(f) = tc.function else { continue };
            let Some(name) = f.name.filter(|n| !n.is_empty()) else { continue };
            let id = tc.id.filter(|s| !s.is_empty()).unwrap_or_else(|| format!("call_{i}"));
            let parsed_args = match f.arguments {
                Some(a) if !a.trim().is_empty() => {
                    serde_json::from_str(&a).unwrap_or_else(|_| serde_json::json!({ "_raw": a }))
                }
                _ => serde_json::json!({}),
            };
            out.push(Block::ToolIntent(ToolIntent {
                id,
                tool: names.get(&name).cloned().unwrap_or(name),
                args: repair_args(parsed_args),
            }));
        }
    }
    if let Some(u) = parsed.usage {
        out.push(Block::Usage(Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        }));
    }
    Ok(out)
}

/// Turn accumulated tool-call fragments into final intents, in issue order.
/// Synthesizes `call_{idx}` for any call the stream never gave an id — some
/// gateways (llama.cpp and others) omit it, and an empty `tool_call_id` 400s on
/// strict backends when the result is sent back. Wire names map back to their
/// canonical (dotted) form; an unmapped name passes through for the kernel's
/// deny-first policy to handle.
fn finalize_tool_calls(
    accum: std::collections::BTreeMap<u32, (String, String, String)>,
    names: &std::collections::HashMap<String, String>,
) -> Vec<ToolIntent> {
    let mut out = Vec::new();
    for (idx, (id, name, args)) in accum {
        if name.is_empty() {
            continue;
        }
        let id = if id.is_empty() {
            format!("call_{idx}")
        } else {
            id
        };
        let parsed = if args.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&args).unwrap_or_else(|_| serde_json::json!({ "_raw": args }))
        };
        out.push(ToolIntent {
            id,
            tool: names.get(&name).cloned().unwrap_or(name),
            args: repair_args(parsed),
        });
    }
    out
}

/// Repair the argument shapes models actually emit instead of a plain object.
/// Two real-world failure modes, both otherwise fatal to the tool call:
///   1. double-encoded — the whole object arrives as a JSON *string*:
///      `"{\"path\": \"src/main.rs\"}"`
///   2. wrapper key — the object is nested under a lone envelope key whose
///      value is the (possibly stringified) real object:
///      `{"arguments": "{\"path\": ...}"}`
///
/// Anything unrecognized passes through untouched — never guess beyond these.
fn repair_args(args: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    // Case 1: the entire args value is a stringified JSON object.
    if let Value::String(s) = &args {
        if let Ok(inner @ Value::Object(_)) = serde_json::from_str::<Value>(s) {
            return inner;
        }
        return args;
    }
    // Case 2: a single envelope key holding the real object (or its string).
    if let Value::Object(map) = &args {
        if map.len() == 1 {
            let (key, val) = map.iter().next().expect("len checked");
            if matches!(key.as_str(), "arguments" | "input" | "parameters" | "args") {
                match val {
                    Value::Object(_) => return val.clone(),
                    Value::String(s) => {
                        if let Ok(inner @ Value::Object(_)) = serde_json::from_str::<Value>(s) {
                            return inner;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    args
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
                                out.push(Block::ToolStarted {
                                    name: n,
                                    target: None,
                                });
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
                                out.push(Block::ToolStarted {
                                    name: e.1.clone(),
                                    target: Some(t),
                                });
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
mod repair_args_tests {
    use super::repair_args;
    use serde_json::json;

    #[test]
    fn well_formed_args_pass_through_untouched() {
        let args = json!({"path": "src/main.rs", "limit": 5});
        assert_eq!(repair_args(args.clone()), args);
    }

    #[test]
    fn double_encoded_args_are_unwrapped() {
        // The whole object arrived as a JSON string — the exact shape behind
        // the "expected string 'path'" failures.
        let args = json!("{\"path\": \"/w/PROGRESS.md\"}");
        assert_eq!(repair_args(args), json!({"path": "/w/PROGRESS.md"}));
    }

    #[test]
    fn envelope_keys_are_unwrapped_object_or_stringified() {
        assert_eq!(
            repair_args(json!({"arguments": {"path": "a.md"}})),
            json!({"path": "a.md"})
        );
        assert_eq!(
            repair_args(json!({"input": "{\"path\": \"a.md\"}"})),
            json!({"path": "a.md"})
        );
    }

    #[test]
    fn plain_strings_and_real_single_keys_are_not_mangled() {
        // A non-JSON string stays as-is (still fails honestly downstream).
        assert_eq!(repair_args(json!("just text")), json!("just text"));
        // A single key that isn't an envelope name is a real argument.
        assert_eq!(
            repair_args(json!({"path": "arguments.md"})),
            json!({"path": "arguments.md"})
        );
        // An envelope-named key holding a plain string is a real argument too.
        assert_eq!(
            repair_args(json!({"input": "hello"})),
            json!({"input": "hello"})
        );
    }
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
        assert_eq!(
            sniff_target(r#"{"file_path":"src/main.rs"}"#),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn command_and_unknown_keys() {
        assert_eq!(
            sniff_target(r#"{"command":"cargo build"}"#),
            Some("cargo build".to_string())
        );
        assert_eq!(sniff_target(r#"{"pattern":"foo"}"#), None);
    }

    #[test]
    fn escaped_quotes_do_not_cut_the_value_short() {
        // K19: an escaped quote inside the value is content, not the closer.
        assert_eq!(
            sniff_target(r#"{"command":"echo \"hello world\" > f.txt"}"#),
            Some(r#"echo "hello world" > f.txt"#.to_string())
        );
        // Escaped backslash before the real closing quote still closes correctly.
        assert_eq!(
            sniff_target(r#"{"path":"dir\\file.rs"}"#),
            Some(r"dir\file.rs".to_string())
        );
        // Still-open value ending mid-escape is incomplete, not a hit.
        assert_eq!(sniff_target(r#"{"command":"echo \"unfinished"#), None);
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

    /// Drive several SSE content records through the real parsing path (record
    /// → delta → think filter), flushing at end-of-stream like the driver does.
    fn drive_many(deltas: &[&str]) -> Vec<Block> {
        let mut accum = BTreeMap::new();
        let mut tf = ThinkTagFilter::default();
        let mut announced = HashSet::new();
        let mut blocks = Vec::new();
        for d in deltas {
            let record = format!(
                "data: {}\n",
                serde_json::json!({"choices":[{"delta":{"content": d}}]})
            );
            blocks
                .extend(process_sse_record(&record, &mut accum, &mut tf, &mut announced).unwrap());
        }
        if let Some(b) = tf.flush() {
            blocks.push(b);
        }
        blocks
    }

    fn join(blocks: &[Block]) -> (String, String) {
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
    fn answer_mentioning_think_tags_streams_fully_visible() {
        // End-to-end regression for the reported bug: the model streams an
        // answer that DOCUMENTS think tags ("Shape 2: `<think>` tags"), with
        // the tag split across SSE deltas exactly as a real stream does. The
        // whole reply must arrive as visible text; nothing may be rerouted
        // into hidden reasoning.
        let full = "Reasoning support (Shape 1: `reasoning_content` field; Shape 2: `<think>` tags)\n- Tool call strategies: `Native`, `Guided` (planned)\n- `models.dev` integration for pricing";
        let deltas = [
            "Reasoning support (Shape 1: `reasoning_content` field; Shape 2: `<th",
            "ink>` tags)\n- Tool call strategies: `Nati",
            "ve`, `Guided` (planned)\n- `models.dev` integration for pricing",
        ];
        let (text, reasoning) = join(&drive_many(&deltas));
        assert_eq!(
            reasoning, "",
            "no part of the answer may become hidden reasoning"
        );
        assert_eq!(text, full, "the full answer must stay visible");
    }

    #[test]
    fn genuine_leading_thinking_still_separates_from_answer() {
        // The same path with REAL inline thinking: block stripped into the
        // reasoning lane, answer intact.
        let deltas = ["<think>weigh the", " options</think>", "The answer is 42."];
        let (text, reasoning) = join(&drive_many(&deltas));
        assert_eq!(reasoning, "weigh the options");
        assert_eq!(text, "The answer is 42.");
    }

    #[test]
    fn crlf_record_is_parsed_not_dropped() {
        // A CRLF-delimited content frame must yield its text (previously the
        // whole response was silently dropped on CRLF servers).
        let blocks = drive("data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\r\n");
        let text: String = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "hello",
            "CRLF record must be parsed, not dropped: {blocks:?}"
        );
    }

    // ── K13: a tool call the server never gave an id gets a synthesized one ─────
    #[test]
    fn missing_tool_call_id_is_synthesized() {
        let mut accum = BTreeMap::new();
        // index 0, a name, no id (empty) — as a lax gateway streams it.
        accum.insert(
            0u32,
            (
                String::new(),
                "fs.read".to_string(),
                r#"{"path":"a"}"#.to_string(),
            ),
        );
        let intents = finalize_tool_calls(accum, &std::collections::HashMap::new());
        assert_eq!(intents.len(), 1);
        assert_eq!(
            intents[0].id, "call_0",
            "empty id must be synthesized, not left blank"
        );
        assert_eq!(intents[0].tool, "fs.read");
        assert_eq!(intents[0].args["path"], "a");
    }

    #[test]
    fn present_tool_call_id_is_preserved() {
        let mut accum = BTreeMap::new();
        accum.insert(
            0u32,
            ("real-id".to_string(), "fs.read".to_string(), String::new()),
        );
        let intents = finalize_tool_calls(accum, &std::collections::HashMap::new());
        assert_eq!(intents[0].id, "real-id");
        assert_eq!(
            intents[0].args,
            serde_json::json!({}),
            "empty args → empty object"
        );
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
    fn strips_a_leading_think_block() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("<think>hmm let me consider</think> after");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, " after");
        assert_eq!(reasoning, "hmm let me consider");
    }

    #[test]
    fn leading_tag_split_across_chunks_still_matches() {
        let mut f = ThinkTagFilter::default();
        let mut all = Vec::new();
        // Whitespace before the tag is fine; "<think>"/"</think>" split mid-tag.
        all.extend(f.feed("  <thi"));
        all.extend(f.feed("nk>reasoning here</th"));
        all.extend(f.feed("ink> end"));
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "   end");
        assert_eq!(reasoning, "reasoning here");
    }

    #[test]
    fn literal_think_mid_answer_is_content_not_a_tag() {
        // The reported bug: the model writes documentation ABOUT `<think>`
        // tags mid-answer — everything after got hidden as collapsed
        // reasoning and the user saw their reply "cut off".
        let mut f = ThinkTagFilter::default();
        let mut all = Vec::new();
        all.extend(f.feed("Reasoning support (Shape 2: `<think>"));
        all.extend(f.feed("` tags). The rest of the answer must stay visible."));
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(
            reasoning, "",
            "mid-answer literal must not open a think block"
        );
        assert_eq!(
            text,
            "Reasoning support (Shape 2: `<think>` tags). The rest of the answer must stay visible."
        );
    }

    #[test]
    fn unclosed_literal_tag_at_answer_end_is_not_swallowed() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("use <think");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "use <think");
        assert_eq!(reasoning, "");
    }

    #[test]
    fn mid_response_block_at_line_start_is_reasoning() {
        // Chat templates emit think blocks on their own line — one arriving
        // mid-response after a newline is real thinking, not content.
        let mut f = ThinkTagFilter::default();
        let mut all = Vec::new();
        all.extend(f.feed("para one\n\n<think>plan the "));
        all.extend(f.feed("next step</think>\npara two"));
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "para one\n\n\npara two");
        assert_eq!(reasoning, "plan the next step");
    }

    #[test]
    fn literal_tag_pair_mid_line_stays_content() {
        // Both tags mentioned inline (docs/code sample) — nothing is stripped.
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("wrap reasoning in `<think>` and `</think>` in templates");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(
            text,
            "wrap reasoning in `<think>` and `</think>` in templates"
        );
        assert_eq!(reasoning, "");
    }

    #[test]
    fn back_to_back_leading_blocks_both_strip() {
        let mut f = ThinkTagFilter::default();
        let mut all = f.feed("<think>a</think><think>b</think>hello");
        all.extend(f.flush());
        let (text, reasoning) = collect(&all);
        assert_eq!(text, "hello");
        assert_eq!(reasoning, "ab");
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
        assert_eq!(
            vllm_tokenize_url("https://h.example/v1"),
            "https://h.example/tokenize"
        );
        assert_eq!(
            vllm_tokenize_url("https://h.example/v1/"),
            "https://h.example/tokenize"
        );
        // No /v1 suffix → append at the given root.
        assert_eq!(
            vllm_tokenize_url("https://h.example"),
            "https://h.example/tokenize"
        );
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
        let msgs = vec![
            Message::system("s"),
            Message::user("u"),
            Message::tool_result("c1", "t"),
        ];
        let body = vllm_tokenize_body("m", &msgs);
        assert_eq!(body["add_generation_prompt"], serde_json::json!(true));
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[2]["role"], "tool");
    }

    #[test]
    fn vllm_body_hoists_a_mid_array_system_like_the_chat_path() {
        let msgs = vec![
            Message::system("SYS"),
            Message::user("u"),
            Message::system("earlier summary"), // mid-array (compaction)
            Message::new(Role::Assistant, "ok"),
        ];
        let body = vllm_tokenize_body("m", &msgs);
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[0]["content"], "SYS\n\nearlier summary");
        assert_eq!(arr[1]["role"], "user");
        assert_eq!(arr[2]["role"], "assistant");
    }

    #[test]
    fn build_chat_hoists_and_merges_system_to_the_front() {
        // Regression: vLLM rejects a `system` message that isn't first, and
        // compaction can insert a summary as a mid-array system message. All
        // system content must merge into one leading message, rest kept in order.
        let msgs = vec![
            Message::system("SYS PROMPT"),
            Message::user("do X"),
            Message::system("earlier conversation summary"), // mid-array (compaction)
            Message::new(Role::Assistant, "ok"),
            Message::tool_result("c1", "out"),
        ];
        let built = build_chat_messages(&msgs);
        assert_eq!(built[0].role, "system");
        assert!(built[0].content.starts_with("SYS PROMPT"));
        assert!(built[0].content.contains("earlier conversation summary"));
        assert_eq!(built.iter().filter(|m| m.role == "system").count(), 1);
        // non-system messages keep their exact order (tool pairing untouched)
        assert_eq!(built[1].role, "user");
        assert_eq!(built[2].role, "assistant");
        assert_eq!(built[3].role, "tool");
    }
}

#[cfg(test)]
mod wire_tool_name_tests {
    use super::*;
    use kernel::ToolIntent;

    fn is_strict_valid(name: &str) -> bool {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    #[test]
    fn dotted_names_become_strict_valid() {
        for n in ["fs.edit", "shell.exec", "memory.write", "web.fetch"] {
            let wire = wire_tool_name(n);
            assert!(is_strict_valid(&wire), "{n} -> {wire} still invalid");
        }
        assert_eq!(wire_tool_name("fs.edit"), "fs_edit");
        // Already-valid names pass through unchanged.
        assert_eq!(wire_tool_name("read_artifact"), "read_artifact");
    }

    #[test]
    fn map_round_trips_wire_back_to_canonical() {
        let canonical = vec!["fs.edit".to_string(), "shell.exec".to_string()];
        let map = wire_name_map(&canonical);
        assert_eq!(map.get("fs_edit").unwrap(), "fs.edit");
        assert_eq!(map.get("shell_exec").unwrap(), "shell.exec");
    }

    #[test]
    fn colliding_wire_names_stay_distinct() {
        // `fs.edit` and `fs-edit` both sanitize toward `fs_edit`/`fs-edit`;
        // force a real collision with two dot variants.
        let canonical = vec!["a.b".to_string(), "a_b".to_string()];
        let map = wire_name_map(&canonical);
        assert_eq!(map.len(), 2, "collision must not drop a tool");
        let mut targets: Vec<_> = map.values().cloned().collect();
        targets.sort();
        assert_eq!(targets, vec!["a.b", "a_b"]);
    }

    #[test]
    fn finalize_maps_wire_call_back_to_dotted_tool() {
        let mut accum = std::collections::BTreeMap::new();
        accum.insert(0u32, ("id".to_string(), "fs_edit".to_string(), r#"{"path":"x"}"#.to_string()));
        let map = wire_name_map(&["fs.edit".to_string()]);
        let intents: Vec<ToolIntent> = finalize_tool_calls(accum, &map);
        assert_eq!(intents[0].tool, "fs.edit", "kernel must see the canonical name");
    }

    #[test]
    fn parse_completion_yields_reasoning_text_tools_and_usage_in_order() {
        // The exact shape the Crystal gateway returns non-streamed: separate
        // reasoning_content, plus a wire-named tool call.
        let body = r#"{
          "choices": [{"message": {
            "content": "The answer is 391.",
            "reasoning_content": "17*23 = 17*(20+3) = 340+51 = 391",
            "tool_calls": [{"id":"c1","function":{"name":"fs_read","arguments":"{\"path\":\"a\"}"}}]
          }}],
          "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
        }"#;
        let map = wire_name_map(&["fs.read".to_string()]);
        let blocks = parse_completion(body, &map).unwrap();
        // Reasoning first, then any text, then the tool intent, then usage last.
        assert!(matches!(&blocks[0], Block::Reasoning(r) if r.contains("391")));
        let text: String = blocks.iter().filter_map(|b| match b {
            Block::Text(t) => Some(t.clone()),
            _ => None,
        }).collect();
        assert!(text.contains("391"), "text was: {text:?}");
        let intent = blocks.iter().find_map(|b| match b {
            Block::ToolIntent(it) => Some(it),
            _ => None,
        }).expect("a tool intent");
        assert_eq!(intent.tool, "fs.read", "wire name mapped back to canonical");
        assert_eq!(intent.args["path"], "a");
        assert!(matches!(blocks.last(), Some(Block::Usage(u)) if u.total_tokens == 30));
    }

    #[test]
    fn parse_completion_splits_inline_think_tags_when_no_reasoning_field() {
        let body = r#"{"choices":[{"message":{"content":"<think>weighing it</think>Done."}}]}"#;
        let blocks = parse_completion(body, &std::collections::HashMap::new()).unwrap();
        assert!(blocks.iter().any(|b| matches!(b, Block::Reasoning(r) if r.contains("weighing"))));
        assert!(blocks.iter().any(|b| matches!(b, Block::Text(t) if t.contains("Done"))));
    }

    #[test]
    fn parse_completion_surfaces_an_error_body() {
        let body = r#"{"error":{"message":"rate limited","type":"rate_limit"}}"#;
        assert!(parse_completion(body, &std::collections::HashMap::new()).is_err());
    }

    #[test]
    fn assistant_history_tool_calls_are_sanitized() {
        let intent = ToolIntent { id: "c1".into(), tool: "fs.edit".into(), args: serde_json::json!({}) };
        let msgs = vec![Message::assistant_calls("", vec![intent])];
        let built = build_chat_messages(&msgs);
        assert_eq!(built[0].tool_calls[0].function.name, "fs_edit", "history name must be wire-valid");
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
        let p = OpenAiCompat::new("http://x", "", "m").with_reasoning(ReasoningConfig {
            enabled: Some(true),
            effort: None,
        });
        let kwargs = p
            .chat_template_kwargs(false)
            .expect("must be Some when enabled");
        assert_eq!(kwargs["enable_thinking"], serde_json::json!(true));
    }

    #[test]
    fn think_off_produces_enable_thinking_false() {
        let p = OpenAiCompat::new("http://x", "", "m").with_reasoning(ReasoningConfig {
            enabled: Some(false),
            effort: None,
        });
        let kwargs = p
            .chat_template_kwargs(false)
            .expect("must be Some when explicitly off");
        assert_eq!(kwargs["enable_thinking"], serde_json::json!(false));
    }

    #[test]
    fn untouched_reasoning_sends_no_kwargs_at_all() {
        let p = OpenAiCompat::new("http://x", "", "m");
        assert!(p.chat_template_kwargs(false).is_none());
    }

    #[test]
    fn switching_connection_updates_model_and_context_for_the_next_turn() {
        let p = OpenAiCompat::new("http://one", "", "first").with_max_ctx(8_192);
        assert_eq!(p.active_model(), "first");
        assert_eq!(p.context_window(), Some(8_192));

        p.switch_connection_with_context("http://two", "secret", "second", Some(32_768));
        assert_eq!(p.active_model(), "second");
        assert_eq!(p.context_window(), Some(32_768));
    }

    #[test]
    fn auth_header_is_omitted_when_empty_and_normalizes_bearer_prefix() {
        let client = reqwest::Client::new();
        let no_key = with_bearer(client.get("http://localhost"), "")
            .build()
            .unwrap();
        assert!(
            no_key
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );

        let key = with_bearer(client.get("http://localhost"), "Bearer secret")
            .build()
            .unwrap();
        assert_eq!(
            key.headers().get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer secret"
        );
    }

    #[test]
    fn full_request_json_actually_contains_the_field() {
        // End-to-end through serde, exactly as it would serialize onto the wire.
        let p = OpenAiCompat::new("http://x", "", "m").with_reasoning(ReasoningConfig {
            enabled: Some(true),
            effort: Some(ReasoningEffort::Medium),
        });
        let req = ChatReq {
            model: "m".into(),
            messages: vec![],
            stream: true,
            tools: vec![],
            stream_options: None,
            chat_template_kwargs: p.chat_template_kwargs(false),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            json.contains("chat_template_kwargs"),
            "field missing from wire JSON: {json}"
        );
        assert!(json.contains("enable_thinking"));
        // The standard string effort knob (GLM/Qwen/OpenAI-compatible), not a
        // vendor-specific boolean.
        assert!(json.contains("reasoning_effort"), "wire JSON: {json}");
        assert!(json.contains("\"medium\""), "wire JSON: {json}");
    }
}
