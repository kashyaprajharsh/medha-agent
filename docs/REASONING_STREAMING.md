# Streaming reasoning / "thinking" over OpenAI-compatible APIs

Research note behind MEDHA's reasoning support. Grounds the parsing in
`crates/providers/src/openai_compat.rs` and the request-side controls in
`crates/lockfile` / the unified `/reasoning` command. Written so the next
person doesn't have to re-derive which field a given server uses.

## TL;DR

There is **no single standard**. Across OpenAI-compatible servers, reasoning
content arrives over the streaming (SSE) chat-completions endpoint in one of
**three shapes**, and a harness that wants to work against "whatever the user
points it at" has to handle all three:

1. **A separate delta field** — `choices[].delta.reasoning_content` (most
   self-hosted reasoning stacks) or `choices[].delta.reasoning` (some
   gateways). Same idea, different spelling.
2. **Inline `<think>…</think>` tags inside the normal `content` stream** — the
   model emits thinking as ordinary text wrapped in sentinel tags; no separate
   field exists.
3. **Nothing** — the server strips reasoning server-side, or the model doesn't
   reason. We must degrade cleanly, not wait for a field that never comes.

MEDHA handles (1) and (2) explicitly and treats (3) as the normal no-reasoning
path.

## The transport

All of this rides the standard `POST /v1/chat/completions` with
`"stream": true`. The response is Server-Sent Events: lines prefixed with
`data: `, one JSON object per event, terminated by a literal `data: [DONE]`.
Each event is a chunk whose `choices[].delta` carries *incremental* fields.

A typical text delta:

```
data: {"choices":[{"delta":{"content":"Hel"}}]}
data: {"choices":[{"delta":{"content":"lo"}}]}
```

## Shape 1 — separate `reasoning_content` delta

Reasoning-capable self-hosted servers (vLLM run with a reasoning parser,
SGLang, and DeepSeek-R1-style deployments) split thinking into its own delta
field so the client can render or hide it independently of the answer:

```
data: {"choices":[{"delta":{"reasoning_content":"Let me"}}]}
data: {"choices":[{"delta":{"reasoning_content":" check the types"}}]}
data: {"choices":[{"delta":{"content":"The answer is 42."}}]}
```

The two streams interleave: a run of `reasoning_content` deltas, then the
`content` deltas for the final answer. Some gateways spell the field
`reasoning` instead of `reasoning_content` — semantically identical.

In code (`Delta` in `openai_compat.rs`):

```rust
#[serde(default, alias = "reasoning")]
reasoning_content: Option<String>,
```

The `alias = "reasoning"` makes one struct accept both spellings. Reasoning
deltas are routed to `Block::Reasoning`, answer deltas to `Block::Text`; the
sink surfaces them on separate channels (`StreamSink::reasoning` vs `text`) so
the TUI can style thinking distinctly and toggle it (Ctrl-T).

## Shape 2 — inline `<think>` tags in `content`

Many models (Ollama, llama.cpp's server, DeepSeek-R1 without a server-side
reasoning parser) don't have a separate field at all — they emit thinking as
plain text inside the normal `content` stream, delimited by sentinel tags:

```
data: {"choices":[{"delta":{"content":"<think>"}}]}
data: {"choices":[{"delta":{"content":"the user wants"}}]}
data: {"choices":[{"delta":{"content":"</think>"}}]}
data: {"choices":[{"delta":{"content":"Here's the fix:"}}]}
```

The wrinkle: a tag can straddle two SSE chunks (`<thi` in one, `nk>` in the
next), so you can't match per-chunk. `ThinkTagFilter` in `openai_compat.rs` is
a small stateful parser that buffers just enough tail to detect a partial tag,
flips an `in_think` flag on `<think>` / `</think>`, and emits the text between
them as `Block::Reasoning` and everything else as `Block::Text`. Result: shape
2 is normalized to look exactly like shape 1 to the rest of the system.

## Shape 3 — no reasoning

If the server strips reasoning or the model doesn't think, neither a
`reasoning_content` field nor `<think>` tags appear — only ordinary `content`.
Nothing special is needed; the reasoning channel simply stays empty. We never
block waiting for reasoning that isn't coming.

## Requesting reasoning (the other half)

Getting reasoning *out* often requires asking for it *in*, and here too there
is no single knob:

- Some servers gate it behind a template variable passed via
  `chat_template_kwargs` (e.g. an `enable_thinking` / effort flag consumed by
  the model's chat template).
- Some accept an OpenAI-style `reasoning_effort` parameter.
- Some always reason and only let you toggle server-side visibility.

MEDHA models this as a `ReasoningConfig { enabled, effort }`
(`kernel::ReasoningConfig`), seeded from `medha.lock`'s `[reasoning]` section
and adjustable live with `/reasoning`. The adapter maps it to
whatever the endpoint understands and **silently omits** knobs a given server
can't map (an effort tier it doesn't support is not faked). This matches the
project's rule: never fabricate a capability the backend doesn't actually have.

## Practical guidance

- **Accept both `reasoning_content` and `reasoning`** — one `serde` alias.
- **Also strip `<think>` tags from `content`**, statefully, because tag-based
  models are common in local setups and the tag can split across chunks.
- **Keep reasoning on a separate channel from the answer** so the UI can hide
  it by default (it's scratch content) and reveal on demand.
- **Treat "no reasoning" as normal**, never as an error or a stall.
- **On the request side, degrade** — send the reasoning controls the endpoint
  supports and drop the rest rather than erroring.
