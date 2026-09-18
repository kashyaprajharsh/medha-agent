//! Live-output sink with no-op defaults for headless callers.

use serde_json::Value;

pub trait StreamSink: Send + Sync {
    /// Compiler pressure for the request being prepared, including its limit
    /// and count quality. Surfaces must not recompute a competing budget.
    fn context_pressure(&self, _pressure: crate::context::ContextPressure) {}
    /// How the turn is being run, when that differs from what was asked —
    /// an image described instead of sent, for instance. Not model output.
    fn notice(&self, _text: &str) {}
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
    /// A tool call with its provider-assigned correlation id. Older surfaces
    /// keep working through the name-only callback.
    fn tool_call_with_id(&self, _id: &str, tool: &str, args: &Value) {
        self.tool_call(tool, args);
    }
    /// A tool's result, after it ran — lets a surface render diffs, errors, etc.
    fn tool_result(&self, _tool: &str, _ok: bool, _payload: &Value) {}
    /// A tool result carrying the same correlation id as its call.
    fn tool_result_with_id(&self, _id: &str, tool: &str, ok: bool, payload: &Value) {
        self.tool_result(tool, ok, payload);
    }
    /// Real token usage for the turn, reported by the provider (authoritative).
    fn usage(&self, _prompt_tokens: u32, _total_tokens: u32) {}
    /// Session cost so far, when pricing is known. `indicative` means the
    /// figure comes from a list price (models.dev) rather than the operator's
    /// own configured rate — surfaces show it as "~$0.42 est.".
    fn cost(&self, _total_usd: f64, _indicative: bool) {}
    /// A deterministic verifier result after file-modifying edits.
    fn verify(&self, _ok: bool, _summary: &str) {}
    /// Compaction is running (`true`) or finished (`false`) — lets a surface show
    /// a live "compacting…" indicator while a summarize pass calls the model.
    fn compacting(&self, _active: bool) {}
    /// Compaction just fired. `summarized` = true for a summarize pass, false for
    /// a cheap prune-only pass; `summary` is the handoff text (Full only).
    fn compaction(&self, _before: u32, _after: u32, _summarized: bool, _summary: Option<&str>) {}
    /// A queued steer message was applied at a turn boundary.
    fn steered(&self, _text: &str) {}
    /// The session was cancelled with steers still queued — they were NOT
    /// applied; the surface should give them back to the user (input box).
    fn steers_returned(&self, _texts: &[String]) {}
    /// The turn is being retried after a transient failure, and whatever this
    /// sink already rendered for it is about to be streamed again. Drop that
    /// partial output: keeping it duplicates the reply. Without this a surface
    /// could only choose between a doubled answer and never retrying a stream
    /// that died mid-flight.
    fn restarted(&self) {}
    /// Whether [`Self::restarted`] can actually retract or explicitly reset
    /// already rendered output. Irreversible stdout-like sinks return false so
    /// the kernel fails once instead of printing a duplicated retry.
    fn supports_restart(&self) -> bool {
        false
    }
    /// What this session is doing now. The one hook that carries liveness, so a
    /// watcher can tell a model that is thinking from a connection that has
    /// died — a distinction no amount of reading the event log recovers, because
    /// a session composing a reply writes nothing.
    fn phase(&self, _phase: crate::progress::Phase) {}
    /// The stream produced something within its current phase. Separate from
    /// [`Self::phase`] because a slow reply and a dead one are the same phase.
    fn phase_ticked(&self) {}
}

/// Discards every update — the headless default.
pub struct NullSink;
impl StreamSink for NullSink {
    fn supports_restart(&self) -> bool {
        true
    }
}
