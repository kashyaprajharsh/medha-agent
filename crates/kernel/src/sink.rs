//! Live-output sink. The kernel pushes streamable updates — model text deltas,
//! tool-call notices, compaction notices — to a sink as they happen, so a
//! surface can render token-by-token without the kernel knowing about surfaces
//! (P9). All methods default to no-ops, so headless callers pass `NullSink`.

use serde_json::Value;

pub trait StreamSink: Send + Sync {
    /// A fragment of model text as it streams in.
    fn text(&self, _delta: &str) {}
    /// A fragment of reasoning/thinking content, when the model streams one
    /// (vLLM/DeepSeek-R1-style `reasoning_content`). Scratch content — shown
    /// live for transparency, never fed back into subsequent-turn history.
    fn reasoning(&self, _delta: &str) {}
    /// A tool call has begun streaming: its name (and often the target file/command,
    /// sniffed from partial args) is known while arguments are still arriving. Lets a
    /// surface show "writing medha.html…" during a large call.
    fn tool_started(&self, _tool: &str, _target: Option<&str>) {}
    /// A tool call the model just requested (about to execute).
    fn tool_call(&self, _tool: &str, _args: &Value) {}
    /// A tool's result, after it ran — lets a surface render diffs, errors, etc.
    fn tool_result(&self, _tool: &str, _ok: bool, _payload: &Value) {}
    /// Real token usage for the turn, reported by the provider (authoritative).
    fn usage(&self, _prompt_tokens: u32, _total_tokens: u32) {}
    /// A deterministic verifier result after file-modifying edits.
    fn verify(&self, _ok: bool, _summary: &str) {}
    /// Compaction just fired. `summarized` = true for a summarize pass, false
    /// for a cheap prune-only pass.
    fn compaction(&self, _before: u32, _after: u32, _summarized: bool) {}
}

/// Discards every update — the headless default.
pub struct NullSink;
impl StreamSink for NullSink {}
