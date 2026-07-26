//! `medha acp` — the editor bridge (Vol 4 §5). Exposes one session over
//! line-delimited JSON-RPC 2.0 on stdio so an editor extension (VS Code, Zed,
//! JetBrains) can embed MEDHA: the kernel streams `event` notifications out and
//! the editor drives it with `message.send` / `approval.respond` / `cancel`.
//!
//! It's the same kernel the TUI runs — only the surface differs (P9). The gate
//! and sink here are thin adapters that (de)serialize to the wire; the kernel
//! never learns an editor exists.
//!
//! Wire format: one JSON object per line, both directions.
//!   → (in)  {"jsonrpc":"2.0","id":1,"method":"message.send","params":{"content":"…"}}
//!   ← (out) {"jsonrpc":"2.0","method":"event","params":{"kind":"model.text","delta":"…"}}
//!   ← (out) {"jsonrpc":"2.0","method":"approval","params":{"gate_id":3,"action":"fs.edit","detail":"…"}}
//!   → (in)  {"jsonrpc":"2.0","method":"approval.respond","params":{"gate_id":3,"approve":true}}

use kernel::{Budget, EventLog, Kernel, Message, Provider, Session, StopReason};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Serializes every outbound frame to stdout. A single lock keeps interleaved
/// writes (sink deltas + approval requests from concurrent tasks) line-atomic.
pub struct Writer {
    out: Mutex<std::io::Stdout>,
}

impl Writer {
    fn write_value(&self, v: &Value) {
        if let Ok(mut o) = self.out.lock() {
            let _ = writeln!(o, "{v}");
            let _ = o.flush();
        }
    }
    /// Emit a JSON-RPC notification (no id, no response expected).
    pub fn notify(&self, method: &str, params: Value) {
        self.write_value(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }
    /// Emit a domain event under the `event` method (the server→client stream).
    fn event(&self, kind: &str, mut params: Value) {
        if let Value::Object(ref mut m) = params {
            m.insert("kind".into(), json!(kind));
        }
        self.notify("event", params);
    }
    /// Reply to a request that carried an `id`.
    fn respond(&self, id: Value, result: Value) {
        self.write_value(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }
}

/// Approval requests awaiting an `approval.respond`, keyed by gate id.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<bool>>>>;

/// Build the shared writer + pending-approval map. Created before the kernel so
/// the gate (wired into the kernel at construction) shares them with the loop.
pub fn bridge() -> (Arc<Writer>, Pending) {
    (
        Arc::new(Writer {
            out: Mutex::new(std::io::stdout()),
        }),
        Arc::new(Mutex::new(HashMap::new())),
    )
}

/// Human gate over the wire: emit an `approval` request carrying a fresh
/// gate id, then park on a oneshot the stdin reader fulfills when the editor
/// answers with `approval.respond`.
pub struct AcpGate {
    writer: Arc<Writer>,
    pending: Pending,
    next_id: AtomicU64,
}

impl AcpGate {
    pub fn new(writer: Arc<Writer>, pending: Pending) -> Self {
        Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
        }
    }
}

#[async_trait::async_trait]
impl kernel::HumanGate for AcpGate {
    async fn confirm(
        &self,
        action: &str,
        detail: Option<&str>,
        escalated: bool,
    ) -> kernel::Approval {
        let gate_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(gate_id, tx);
        self.writer.notify(
            "approval",
            json!({ "gate_id": gate_id, "action": action, "detail": detail, "escalated": escalated }),
        );
        // Editor disconnected or never answered → treat as a rejection (P5:
        // never commit an unapproved action). An editor approval is "allow once":
        // it never silently persists a path to medha.lock.
        if rx.await.unwrap_or(false) {
            kernel::Approval::Once
        } else {
            kernel::Approval::Deny
        }
    }
}

/// Streams kernel updates out as `event` notifications. Tool observations carry
/// the raw payload so the editor can open a native diff when `old`/`new`/`path`
/// are present (the "changes opened in your editor" experience).
struct AcpSink {
    writer: Arc<Writer>,
}

impl kernel::StreamSink for AcpSink {
    fn text(&self, delta: &str) {
        self.writer.event("model.text", json!({ "delta": delta }));
    }
    fn reasoning(&self, delta: &str) {
        self.writer
            .event("model.reasoning", json!({ "delta": delta }));
    }
    fn tool_call(&self, tool: &str, args: &Value) {
        self.writer
            .event("tool.call", json!({ "tool": tool, "args": args }));
    }
    fn tool_result(&self, tool: &str, ok: bool, payload: &Value) {
        self.writer.event(
            "tool.observation",
            json!({ "tool": tool, "ok": ok, "payload": payload }),
        );
    }
    fn usage(&self, prompt_tokens: u32, total_tokens: u32) {
        self.writer.event(
            "usage",
            json!({ "prompt_tokens": prompt_tokens, "total_tokens": total_tokens }),
        );
    }
    fn verify(&self, ok: bool, summary: &str) {
        self.writer
            .event("verify", json!({ "ok": ok, "summary": summary }));
    }
    fn compacting(&self, active: bool) {
        self.writer.event("compacting", json!({ "active": active }));
    }
    fn compaction(&self, before: u32, after: u32, summarized: bool, summary: Option<&str>) {
        self.writer.event(
            "compaction",
            json!({ "before": before, "after": after, "summarized": summarized, "summary": summary }),
        );
    }
    fn steered(&self, text: &str) {
        self.writer
            .event("message.steered", json!({ "content": text }));
    }
    fn steers_returned(&self, texts: &[String]) {
        self.writer
            .event("message.returned", json!({ "contents": texts }));
    }
}

/// Result of a spawned turn, delivered back to the main loop.
enum TurnDone {
    Ok(Vec<Message>, StopReason),
    Err(String),
}

/// Upper bound for one JSON-RPC frame — far beyond any legitimate message,
/// small enough that a runaway peer can't balloon the process.
const MAX_FRAME: u64 = 16 * 1024 * 1024;

/// Read one newline-terminated frame with the size cap enforced. Cancel-safe:
/// `read_until` accumulates into `buf` across `select!` cancellations, and the
/// buffer is only drained once a full line has arrived. Returns `Ok(None)` on
/// EOF; an oversized frame is an error (protocol violation — disconnect).
async fn read_frame(
    stdin: &mut tokio::io::Take<BufReader<tokio::io::Stdin>>,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<String>> {
    let n = stdin.read_until(b'\n', buf).await?;
    if n == 0 && buf.is_empty() {
        return Ok(None); // clean EOF
    }
    if buf.last() == Some(&b'\n') || n == 0 {
        let line = String::from_utf8_lossy(buf).into_owned();
        buf.clear();
        stdin.set_limit(MAX_FRAME); // fresh cap for the next frame
        return Ok(Some(line));
    }
    // No newline and the reader stopped: the cap was exhausted mid-frame.
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "frame exceeds the 16 MiB limit",
    ))
}

/// Run the bridge until stdin closes. Single session, one turn at a time — a
/// `message.send` arriving mid-turn becomes a STEER (injected at the next
/// turn boundary); `cancel`/`interrupt` stops gracefully via the kernel's
/// interrupt handle (in-flight tools settle; never a mid-tool kill).
#[allow(clippy::too_many_arguments)]
pub async fn run<P, L>(
    kernel: Arc<Kernel<P, L>>,
    session: Session,
    system: String,
    model: String,
    base_budget: Budget,
    agent_budget: kernel::BudgetHandle,
    writer: Arc<Writer>,
    pending: Pending,
) -> anyhow::Result<()>
where
    P: Provider + 'static,
    L: EventLog + 'static,
{
    // Announce readiness + capabilities (Vol 4 §6 handshake, editor-initiated
    // handshakes also answered below).
    writer.notify(
        "ready",
        json!({ "proto": "1.0", "model": model, "caps": { "cards": ["approval", "diff"] } }),
    );

    let mut transcript: Vec<Message> = vec![Message::system(system)];
    // Frame reads are capped: `lines()` would buffer a single unterminated
    // "line" without bound, so a runaway peer could grow memory indefinitely.
    let mut stdin = tokio::io::AsyncReadExt::take(BufReader::new(tokio::io::stdin()), MAX_FRAME);
    let mut frame_buf: Vec<u8> = Vec::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<TurnDone>();
    let mut running = false;
    let mut interrupt: Option<kernel::InterruptHandle> = None;

    loop {
        tokio::select! {
            line = read_frame(&mut stdin, &mut frame_buf) => {
                let Ok(Some(line)) = line else { break }; // stdin closed / oversized frame → exit
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                    writer.event("error", json!({ "message": "invalid JSON" }));
                    continue;
                };
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                match method {
                    "initialize" | "hello" => {
                        if let Some(id) = msg.get("id") {
                            writer.respond(
                                id.clone(),
                                json!({ "proto": "1.0", "model": model, "caps": { "cards": ["approval", "diff"] } }),
                            );
                        }
                    }
                    "message.send" => {
                        let content = params.get("content").and_then(Value::as_str).unwrap_or("").to_string();
                        if content.trim().is_empty() {
                            continue;
                        }
                        if running {
                            // Mid-turn message = steer: the kernel injects it
                            // at the next turn boundary of the SAME session.
                            if let Some(h) = &interrupt {
                                h.steer(content);
                                writer.event("message.queued", json!({}));
                            } else {
                                writer.event("error", json!({ "message": "a turn is already running" }));
                            }
                            continue;
                        }
                        transcript.push(Message::user(content));
                        running = true;
                        let (handle, queue) = kernel::InterruptQueue::pair();
                        interrupt = Some(handle);
                        let kernel = kernel.clone();
                        let session = session.clone();
                        let messages = transcript.clone();
                        // One editor message starts one task. Publish its fresh
                        // pool before the turn so descendants share this task,
                        // not the spend accumulated by an earlier message.
                        let budget = crate::task_budget(&base_budget, &agent_budget);
                        let writer = writer.clone();
                        let done_tx = done_tx.clone();
                        tokio::spawn(async move {
                            let sink = AcpSink { writer };
                            let result = kernel
                                .run_session(&session, messages, budget, &sink, Some(queue))
                                .await;
                            let _ = done_tx.send(match result {
                                Ok((updated, reason)) => TurnDone::Ok(updated, reason),
                                Err(e) => TurnDone::Err(e.to_string()),
                            });
                        });
                    }
                    "approval.respond" => {
                        let gate_id = params.get("gate_id").and_then(Value::as_u64).unwrap_or(0);
                        // Accept either {approve:bool} or {decision:"approve"|"deny"}.
                        let approve = params.get("approve").and_then(Value::as_bool).unwrap_or_else(|| {
                            params.get("decision").and_then(Value::as_str) == Some("approve")
                        });
                        if let Some(tx) = pending.lock().unwrap().remove(&gate_id) {
                            let _ = tx.send(approve);
                        }
                    }
                    "cancel" | "interrupt" => {
                        // Graceful: in-flight tools settle, the turn returns
                        // Done(Interrupted) with a consistent transcript.
                        if let Some(h) = &interrupt {
                            h.cancel_turn();
                        }
                    }
                    "shutdown" | "exit" => break,
                    other => {
                        writer.event("error", json!({ "message": format!("unknown method: {other}") }));
                    }
                }
            }
            Some(done) = done_rx.recv() => {
                running = false;
                interrupt = None;
                match done {
                    TurnDone::Ok(updated, reason) => {
                        transcript = updated;
                        match reason {
                            StopReason::Interrupted => writer.event("turn.cancelled", json!({})),
                            StopReason::Budget(s) => writer.event("turn.done", json!({ "stopped": s.label() })),
                            StopReason::Finished => writer.event("turn.done", json!({ "stopped": Value::Null })),
                        }
                    }
                    TurnDone::Err(e) => writer.event("turn.error", json!({ "message": e })),
                }
            }
        }
    }
    Ok(())
}
