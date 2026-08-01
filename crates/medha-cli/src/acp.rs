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
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

const OUTBOUND_FRAMES: usize = 256;
const MAX_OUTBOUND_FRAME: usize = 2 * 1024 * 1024;
const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;
const WRITER_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const TURN_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const TURN_ABORT_GRACE: Duration = Duration::from_secs(2);

enum Outbound {
    Frame(Vec<u8>),
    Close(oneshot::Sender<io::Result<()>>),
}

/// Nonblocking producer side of ACP output.
///
/// Kernel stream callbacks are synchronous, so they cannot await stdout. They
/// enqueue into a byte-bounded channel instead. Saturation means the editor is
/// no longer consuming the protocol; fail the connection closed rather than
/// pinning a Tokio worker or retaining unbounded deltas.
pub struct Writer {
    tx: mpsc::Sender<Outbound>,
    queued_bytes: Arc<AtomicUsize>,
    cancelled: CancellationToken,
}

impl Writer {
    fn write_value(&self, value: &Value) -> bool {
        if self.cancelled.is_cancelled() {
            return false;
        }
        let Ok(mut frame) = serde_json::to_vec(value) else {
            self.cancelled.cancel();
            return false;
        };
        frame.push(b'\n');
        let frame_len = frame.len();
        if frame_len > MAX_OUTBOUND_FRAME
            || self
                .queued_bytes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                    queued
                        .checked_add(frame_len)
                        .filter(|total| *total <= MAX_QUEUED_BYTES)
                })
                .is_err()
        {
            self.cancelled.cancel();
            return false;
        }
        if self.tx.try_send(Outbound::Frame(frame)).is_err() {
            self.queued_bytes.fetch_sub(frame_len, Ordering::AcqRel);
            self.cancelled.cancel();
            return false;
        }
        true
    }

    /// Emit a JSON-RPC notification (no id, no response expected).
    pub fn notify(&self, method: &str, params: Value) -> bool {
        self.write_value(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Emit a domain event under the `event` method (the server→client stream).
    fn event(&self, kind: &str, mut params: Value) -> bool {
        if let Value::Object(ref mut m) = params {
            m.insert("kind".into(), json!(kind));
        }
        self.notify("event", params)
    }

    /// Reply to a request that carried an `id`.
    fn respond(&self, id: Value, result: Value) -> bool {
        self.write_value(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn error(&self, id: Value, code: i32, message: impl Into<String>) -> bool {
        self.write_value(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message.into() }
        }))
    }

    async fn cancelled(&self) {
        self.cancelled.cancelled().await;
    }
}

async fn writer_loop<W>(
    mut output: W,
    mut rx: mpsc::Receiver<Outbound>,
    queued_bytes: Arc<AtomicUsize>,
    cancelled: CancellationToken,
) where
    W: AsyncWrite + Unpin,
{
    loop {
        let outbound = tokio::select! {
            _ = cancelled.cancelled() => break,
            outbound = rx.recv() => outbound,
        };
        let Some(outbound) = outbound else {
            break;
        };
        match outbound {
            Outbound::Frame(frame) => {
                let len = frame.len();
                let result = tokio::select! {
                    _ = cancelled.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "ACP connection cancelled")),
                    result = output.write_all(&frame) => result,
                };
                let result = match result {
                    Ok(()) => tokio::select! {
                        _ = cancelled.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "ACP connection cancelled")),
                        result = output.flush() => result,
                    },
                    Err(error) => Err(error),
                };
                queued_bytes.fetch_sub(len, Ordering::AcqRel);
                if result.is_err() {
                    cancelled.cancel();
                    break;
                }
            }
            Outbound::Close(ack) => {
                let result = tokio::select! {
                    _ = cancelled.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "ACP connection cancelled")),
                    result = output.flush() => result,
                };
                let failed = result.is_err();
                let _ = ack.send(result);
                if failed {
                    cancelled.cancel();
                }
                break;
            }
        }
    }
}

struct WriterTask {
    handle: Option<JoinHandle<()>>,
    cancelled: CancellationToken,
}

impl WriterTask {
    async fn finish(mut self, writer: &Writer) {
        if !self.cancelled.is_cancelled() {
            let (ack_tx, ack_rx) = oneshot::channel();
            let close = tokio::time::timeout(
                WRITER_SHUTDOWN_GRACE,
                writer.tx.send(Outbound::Close(ack_tx)),
            )
            .await;
            if matches!(close, Ok(Ok(()))) {
                let _ = tokio::time::timeout(WRITER_SHUTDOWN_GRACE, ack_rx).await;
            } else {
                self.cancelled.cancel();
            }
        }
        if let Some(mut handle) = self.handle.take()
            && tokio::time::timeout(WRITER_SHUTDOWN_GRACE, &mut handle)
                .await
                .is_err()
        {
            self.cancelled.cancel();
            handle.abort();
            let _ = tokio::time::timeout(WRITER_SHUTDOWN_GRACE, handle).await;
        }
    }
}

impl Drop for WriterTask {
    fn drop(&mut self) {
        self.cancelled.cancel();
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

/// Approval requests awaiting an `approval.respond`, keyed by gate id.
pub(crate) type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<bool>>>>;

fn lock_pending(
    pending: &Pending,
) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<bool>>> {
    pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct Bridge {
    pub(crate) writer: Arc<Writer>,
    pub(crate) pending: Pending,
    writer_task: WriterTask,
}

/// Build the shared writer + pending-approval map. Created before the kernel so
/// the gate (wired into the kernel at construction) shares them with the loop.
pub(crate) fn bridge() -> Bridge {
    bridge_with_output(tokio::io::stdout(), OUTBOUND_FRAMES)
}

fn bridge_with_output<W>(output: W, capacity: usize) -> Bridge
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(capacity.max(1));
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let cancelled = CancellationToken::new();
    let handle = tokio::spawn(writer_loop(
        output,
        rx,
        Arc::clone(&queued_bytes),
        cancelled.clone(),
    ));
    Bridge {
        writer: Arc::new(Writer {
            tx,
            queued_bytes,
            cancelled: cancelled.clone(),
        }),
        pending: Arc::new(Mutex::new(HashMap::new())),
        writer_task: WriterTask {
            handle: Some(handle),
            cancelled,
        },
    }
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

/// Removes an approval registration whenever its awaiting future is cancelled.
///
/// A surface disconnect can drop `confirm` at any await point. Without this
/// guard the sender remained in the map forever, retaining the abandoned gate.
struct PendingGuard {
    pending: Pending,
    gate_id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        lock_pending(&self.pending).remove(&self.gate_id);
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
        lock_pending(&self.pending).insert(gate_id, tx);
        let _guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            gate_id,
        };
        if !self.writer.notify(
            "approval",
            json!({ "gate_id": gate_id, "action": action, "detail": detail, "escalated": escalated }),
        ) {
            return kernel::Approval::Deny;
        }
        // Editor disconnected or never answered → treat as a rejection (P5:
        // never commit an unapproved action). An editor approval is "allow once":
        // it never silently persists a path to medha.lock.
        let approved = tokio::select! {
            result = rx => result.unwrap_or(false),
            _ = self.writer.cancelled() => false,
        };
        if approved {
            kernel::Approval::Once
        } else {
            kernel::Approval::Deny
        }
    }
}

fn deny_pending(pending: &Pending) -> usize {
    let approvals = lock_pending(pending)
        .drain()
        .map(|(_, sender)| sender)
        .collect::<Vec<_>>();
    let count = approvals.len();
    for sender in approvals {
        let _ = sender.send(false);
    }
    count
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

#[derive(Debug, PartialEq, Eq)]
enum RpcAction {
    None,
    StartTurn(String),
    Shutdown,
}

fn rpc_result(writer: &Writer, id: &Option<Value>, result: Value) {
    if let Some(id) = id {
        writer.respond(id.clone(), result);
    }
}

fn rpc_error(writer: &Writer, id: &Option<Value>, code: i32, message: impl Into<String>) {
    if let Some(id) = id {
        writer.error(id.clone(), code, message);
    }
}

/// Validate and dispatch one decoded JSON-RPC frame.
///
/// Every valid request (an object containing `id`) takes exactly one result or
/// error arm. Valid notifications execute the same action but emit no response.
fn dispatch_rpc(
    message: Value,
    model: &str,
    running: bool,
    interrupt: Option<&kernel::InterruptHandle>,
    pending: &Pending,
    writer: &Writer,
) -> RpcAction {
    let Some(object) = message.as_object() else {
        writer.error(Value::Null, -32600, "invalid JSON-RPC request");
        return RpcAction::None;
    };
    let id = object.get("id").cloned();
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        writer.error(id.unwrap_or(Value::Null), -32600, "jsonrpc must be \"2.0\"");
        return RpcAction::None;
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        writer.error(
            id.unwrap_or(Value::Null),
            -32600,
            "request method must be a string",
        );
        return RpcAction::None;
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" | "hello" => {
            rpc_result(
                writer,
                &id,
                json!({ "proto": "1.0", "model": model, "caps": { "cards": ["approval", "diff"] } }),
            );
            RpcAction::None
        }
        "message.send" => {
            let Some(content) = params
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|content| !content.trim().is_empty())
            else {
                rpc_error(writer, &id, -32602, "content must be a non-empty string");
                return RpcAction::None;
            };
            if running {
                if let Some(handle) = interrupt {
                    handle.steer(content);
                    writer.event("message.queued", json!({}));
                    rpc_result(writer, &id, json!({ "accepted": true, "steered": true }));
                } else {
                    rpc_error(writer, &id, -32000, "a turn is already running");
                }
                RpcAction::None
            } else {
                rpc_result(writer, &id, json!({ "accepted": true, "steered": false }));
                RpcAction::StartTurn(content)
            }
        }
        "approval.respond" => {
            let gate_id = params.get("gate_id").and_then(Value::as_u64);
            let decision = match (
                params.get("approve").and_then(Value::as_bool),
                params.get("decision").and_then(Value::as_str),
            ) {
                (Some(approve), _) => Some(approve),
                (None, Some("approve")) => Some(true),
                (None, Some("deny")) => Some(false),
                _ => None,
            };
            let (Some(gate_id), Some(approve)) = (gate_id, decision) else {
                rpc_error(
                    writer,
                    &id,
                    -32602,
                    "gate_id and an approve/deny decision are required",
                );
                return RpcAction::None;
            };
            let sender = lock_pending(pending).remove(&gate_id);
            if let Some(sender) = sender {
                let _ = sender.send(approve);
                rpc_result(writer, &id, json!({ "accepted": true }));
            } else {
                rpc_error(writer, &id, -32001, "approval is not pending");
            }
            RpcAction::None
        }
        "cancel" | "interrupt" => {
            let cancelled_turn = if let Some(handle) = interrupt {
                handle.cancel_turn();
                true
            } else {
                false
            };
            let denied = deny_pending(pending);
            rpc_result(
                writer,
                &id,
                json!({ "cancelled": cancelled_turn || denied > 0 }),
            );
            RpcAction::None
        }
        "shutdown" | "exit" => {
            if let Some(handle) = interrupt {
                handle.cancel_turn();
            }
            deny_pending(pending);
            rpc_result(writer, &id, json!({ "shutting_down": true }));
            RpcAction::Shutdown
        }
        _ => {
            rpc_error(writer, &id, -32601, format!("unknown method: {method}"));
            RpcAction::None
        }
    }
}

fn dispatch_line(
    line: &str,
    model: &str,
    running: bool,
    interrupt: Option<&kernel::InterruptHandle>,
    pending: &Pending,
    writer: &Writer,
) -> RpcAction {
    match serde_json::from_str::<Value>(line) {
        Ok(message) => dispatch_rpc(message, model, running, interrupt, pending, writer),
        Err(_) => {
            // A malformed peer message cannot approve safely. Reject any gate
            // waiting on that peer instead of retaining it indefinitely.
            deny_pending(pending);
            writer.error(Value::Null, -32700, "invalid JSON");
            RpcAction::None
        }
    }
}

async fn settle_turn(
    interrupt: &mut Option<kernel::InterruptHandle>,
    pending: &Pending,
    turns: &mut JoinSet<TurnDone>,
    grace: Duration,
) {
    if let Some(handle) = interrupt.take() {
        handle.cancel_turn();
    }
    deny_pending(pending);

    if turns.is_empty() {
        return;
    }
    if tokio::time::timeout(grace, turns.join_next())
        .await
        .is_err()
    {
        turns.abort_all();
        let settle = async { while turns.join_next().await.is_some() {} };
        let _ = tokio::time::timeout(TURN_ABORT_GRACE, settle).await;
    }
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
    resumed: Vec<Message>,
    bridge: Bridge,
) -> anyhow::Result<()>
where
    P: Provider + 'static,
    L: EventLog + 'static,
{
    let Bridge {
        writer,
        pending,
        writer_task,
    } = bridge;
    // Announce readiness + capabilities (Vol 4 §6 handshake, editor-initiated
    // handshakes also answered below).
    writer.notify(
        "ready",
        json!({ "proto": "1.0", "model": model, "caps": { "cards": ["approval", "diff"] } }),
    );

    let mut transcript = crate::session_transcript(system, resumed);
    // Frame reads are capped: `lines()` would buffer a single unterminated
    // "line" without bound, so a runaway peer could grow memory indefinitely.
    let mut stdin = tokio::io::AsyncReadExt::take(BufReader::new(tokio::io::stdin()), MAX_FRAME);
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut turns = JoinSet::new();
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
                match dispatch_line(trimmed, &model, running, interrupt.as_ref(), &pending, &writer) {
                    RpcAction::None => {}
                    RpcAction::Shutdown => break,
                    RpcAction::StartTurn(content) => {
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
                        turns.spawn(async move {
                            let sink = AcpSink { writer };
                            let result = kernel
                                .run_session(&session, messages, budget, &sink, Some(queue))
                                .await;
                            match result {
                                Ok((updated, reason)) => TurnDone::Ok(updated, reason),
                                Err(e) => TurnDone::Err(e.to_string()),
                            }
                        });
                    }
                }
            }
            joined = turns.join_next(), if running => {
                running = false;
                interrupt = None;
                // No approval belongs past the turn that requested it. This
                // also releases a gate whose task ended with an error before
                // consuming its response.
                deny_pending(&pending);
                match joined {
                    Some(Ok(TurnDone::Ok(updated, reason))) => {
                        transcript = updated;
                        match reason {
                            StopReason::Interrupted => writer.event("turn.cancelled", json!({})),
                            StopReason::Budget(s) => writer.event("turn.done", json!({ "stopped": s.label() })),
                            StopReason::Finished => writer.event("turn.done", json!({ "stopped": Value::Null })),
                        };
                    }
                    Some(Ok(TurnDone::Err(e))) => {
                        writer.event("turn.error", json!({ "message": e }));
                    }
                    Some(Err(error)) => {
                        writer.event("turn.error", json!({ "message": format!("turn task failed: {error}") }));
                    }
                    None => {
                        writer.event("turn.error", json!({ "message": "turn task disappeared" }));
                    }
                }
            }
            _ = writer.cancelled() => break,
        }
    }

    settle_turn(&mut interrupt, &pending, &mut turns, TURN_SHUTDOWN_GRACE).await;
    deny_pending(&pending);
    writer_task.finish(&writer).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::{Approval, HumanGate, Role};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::AsyncReadExt;

    fn capture_writer(capacity: usize) -> (Arc<Writer>, mpsc::Receiver<Outbound>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Arc::new(Writer {
                tx,
                queued_bytes: Arc::new(AtomicUsize::new(0)),
                cancelled: CancellationToken::new(),
            }),
            rx,
        )
    }

    fn captured_values(rx: &mut mpsc::Receiver<Outbound>) -> Vec<Value> {
        let mut values = Vec::new();
        while let Ok(outbound) = rx.try_recv() {
            match outbound {
                Outbound::Frame(frame) => {
                    values.push(serde_json::from_slice(&frame).expect("valid JSON frame"));
                }
                Outbound::Close(_) => panic!("capture writer unexpectedly closed"),
            }
        }
        values
    }

    fn assert_exactly_one_response(values: &[Value], id: Value) {
        let responses = values
            .iter()
            .filter(|value| value.get("id").is_some())
            .collect::<Vec<_>>();
        assert_eq!(
            responses.len(),
            1,
            "expected exactly one response, got {values:?}"
        );
        assert_eq!(responses[0].get("id"), Some(&id), "{values:?}");
        assert_ne!(
            responses[0].get("result").is_some(),
            responses[0].get("error").is_some(),
            "response must contain exactly one of result/error: {values:?}"
        );
    }

    fn request(
        message: Value,
        running: bool,
        interrupt: Option<&kernel::InterruptHandle>,
        pending: &Pending,
    ) -> (RpcAction, Vec<Value>) {
        let (writer, mut rx) = capture_writer(32);
        let action = dispatch_rpc(message, "test-model", running, interrupt, pending, &writer);
        (action, captured_values(&mut rx))
    }

    #[test]
    fn json_rpc_requests_receive_exactly_one_result_or_error() {
        let empty_pending = || Arc::new(Mutex::new(HashMap::new()));

        for (id, method) in [(1, "initialize"), (2, "hello")] {
            let (_, values) = request(
                json!({"jsonrpc": "2.0", "id": id, "method": method}),
                false,
                None,
                &empty_pending(),
            );
            assert_exactly_one_response(&values, json!(id));
        }

        let (action, values) = request(
            json!({"jsonrpc": "2.0", "id": 3, "method": "message.send", "params": {"content": "hello"}}),
            false,
            None,
            &empty_pending(),
        );
        assert_eq!(action, RpcAction::StartTurn("hello".into()));
        assert_exactly_one_response(&values, json!(3));

        let (steer, _queue) = kernel::InterruptQueue::pair();
        let (_, values) = request(
            json!({"jsonrpc": "2.0", "id": 4, "method": "message.send", "params": {"content": "change course"}}),
            true,
            Some(&steer),
            &empty_pending(),
        );
        assert_exactly_one_response(&values, json!(4));

        let pending = empty_pending();
        let (approval_tx, mut approval_rx) = oneshot::channel();
        lock_pending(&pending).insert(41, approval_tx);
        let (_, values) = request(
            json!({"jsonrpc": "2.0", "id": 5, "method": "approval.respond", "params": {"gate_id": 41, "approve": true}}),
            true,
            None,
            &pending,
        );
        assert_eq!(approval_rx.try_recv(), Ok(true));
        assert_exactly_one_response(&values, json!(5));

        for (id, method) in [(6, "cancel"), (7, "interrupt")] {
            let (handle, queue) = kernel::InterruptQueue::pair();
            let pending = empty_pending();
            let (approval_tx, mut approval_rx) = oneshot::channel();
            lock_pending(&pending).insert(id, approval_tx);
            let (_, values) = request(
                json!({"jsonrpc": "2.0", "id": id, "method": method}),
                true,
                Some(&handle),
                &pending,
            );
            assert!(queue.cancel_requested());
            assert_eq!(approval_rx.try_recv(), Ok(false));
            assert!(lock_pending(&pending).is_empty());
            assert_exactly_one_response(&values, json!(id));
        }

        for (id, method) in [(8, "shutdown"), (9, "exit")] {
            let (handle, queue) = kernel::InterruptQueue::pair();
            let pending = empty_pending();
            let (approval_tx, mut approval_rx) = oneshot::channel();
            lock_pending(&pending).insert(id, approval_tx);
            let (action, values) = request(
                json!({"jsonrpc": "2.0", "id": id, "method": method}),
                true,
                Some(&handle),
                &pending,
            );
            assert_eq!(action, RpcAction::Shutdown);
            assert!(queue.cancel_requested());
            assert_eq!(approval_rx.try_recv(), Ok(false));
            assert_exactly_one_response(&values, json!(id));
        }
    }

    #[test]
    fn invalid_busy_and_unknown_requests_receive_one_error() {
        let cases = [
            json!({"jsonrpc": "1.0", "id": 10, "method": "hello"}),
            json!({"jsonrpc": "2.0", "id": 11}),
            json!({"jsonrpc": "2.0", "id": 12, "method": "message.send", "params": {"content": ""}}),
            json!({"jsonrpc": "2.0", "id": 13, "method": "approval.respond", "params": {"gate_id": 1}}),
            json!({"jsonrpc": "2.0", "id": 14, "method": "approval.respond", "params": {"gate_id": 999, "approve": true}}),
            json!({"jsonrpc": "2.0", "id": 15, "method": "does.not.exist"}),
        ];
        for message in cases {
            let id = message["id"].clone();
            let (_, values) = request(message, false, None, &Arc::new(Mutex::new(HashMap::new())));
            assert_exactly_one_response(&values, id);
            assert!(values[0].get("error").is_some(), "{values:?}");
        }

        let (_, values) = request(
            json!({"jsonrpc": "2.0", "id": 16, "method": "message.send", "params": {"content": "busy"}}),
            true,
            None,
            &Arc::new(Mutex::new(HashMap::new())),
        );
        assert_exactly_one_response(&values, json!(16));
        assert!(values[0].get("error").is_some());

        let (writer, mut rx) = capture_writer(4);
        assert_eq!(
            dispatch_line(
                "{not json",
                "model",
                false,
                None,
                &Arc::new(Mutex::new(HashMap::new())),
                &writer,
            ),
            RpcAction::None
        );
        assert_exactly_one_response(&captured_values(&mut rx), Value::Null);

        let (_, values) = request(
            json!(["not", "an", "object"]),
            false,
            None,
            &Arc::new(Mutex::new(HashMap::new())),
        );
        assert_exactly_one_response(&values, Value::Null);
    }

    #[test]
    fn valid_notifications_never_receive_a_response() {
        let (writer, mut rx) = capture_writer(32);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (approval_tx, mut approval_rx) = oneshot::channel();
        lock_pending(&pending).insert(7, approval_tx);
        let (handle, _queue) = kernel::InterruptQueue::pair();

        for message in [
            json!({"jsonrpc": "2.0", "method": "initialize"}),
            json!({"jsonrpc": "2.0", "method": "message.send", "params": {"content": "hello"}}),
            json!({"jsonrpc": "2.0", "method": "message.send", "params": {"content": "steer"}}),
            json!({"jsonrpc": "2.0", "method": "approval.respond", "params": {"gate_id": 7, "decision": "deny"}}),
            json!({"jsonrpc": "2.0", "method": "cancel"}),
            json!({"jsonrpc": "2.0", "method": "shutdown"}),
            json!({"jsonrpc": "2.0", "method": "unknown"}),
        ] {
            let running = message["params"]["content"] == "steer";
            dispatch_rpc(
                message,
                "model",
                running,
                running.then_some(&handle),
                &pending,
                &writer,
            );
        }

        assert_eq!(approval_rx.try_recv(), Ok(false));
        let values = captured_values(&mut rx);
        assert!(
            values.iter().all(|value| value.get("id").is_none()),
            "notifications produced a response: {values:?}"
        );
    }

    #[test]
    fn resumed_transcript_is_preserved_exactly_after_the_current_system_prompt() {
        let resumed = vec![
            Message::user("prior user"),
            Message::new(Role::Assistant, "prior answer"),
            Message::tool_result("call-1", r#"{"ok":true}"#),
        ];
        let expected = resumed
            .iter()
            .map(|message| serde_json::to_value(message).unwrap())
            .collect::<Vec<_>>();

        let actual = crate::session_transcript("current system".into(), resumed);
        assert_eq!(actual.len(), expected.len() + 1);
        assert_eq!(actual[0].role, Role::System);
        assert_eq!(actual[0].content, "current system");
        assert_eq!(
            actual[1..]
                .iter()
                .map(|message| serde_json::to_value(message).unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[tokio::test]
    async fn approval_entries_are_raii_scoped_and_disconnect_denies_them() {
        let (writer, mut rx) = capture_writer(8);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let gate = Arc::new(AcpGate::new(Arc::clone(&writer), Arc::clone(&pending)));

        let dropped_gate = Arc::clone(&gate);
        let dropped =
            tokio::spawn(async move { dropped_gate.confirm("fs.edit", None, false).await });
        let _approval_frame = rx.recv().await.expect("approval notification");
        assert_eq!(lock_pending(&pending).len(), 1);
        dropped.abort();
        let _ = dropped.await;
        assert!(
            lock_pending(&pending).is_empty(),
            "dropped approval future leaked its map entry"
        );

        let disconnected_gate = Arc::clone(&gate);
        let disconnected =
            tokio::spawn(async move { disconnected_gate.confirm("shell.exec", None, false).await });
        let _approval_frame = rx.recv().await.expect("approval notification");
        assert_eq!(lock_pending(&pending).len(), 1);
        writer.cancelled.cancel();
        assert_eq!(disconnected.await.unwrap(), Approval::Deny);
        assert!(lock_pending(&pending).is_empty());
    }

    #[tokio::test]
    async fn protocol_error_and_shutdown_drain_pending_approvals() {
        let (writer, mut rx) = capture_writer(8);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (malformed_tx, mut malformed_rx) = oneshot::channel();
        lock_pending(&pending).insert(1, malformed_tx);
        dispatch_line("{broken", "model", true, None, &pending, &writer);
        assert_eq!(malformed_rx.try_recv(), Ok(false));
        assert!(lock_pending(&pending).is_empty());
        captured_values(&mut rx);

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        lock_pending(&pending).insert(2, shutdown_tx);
        let (handle, queue) = kernel::InterruptQueue::pair();
        assert_eq!(
            dispatch_rpc(
                json!({"jsonrpc": "2.0", "id": 20, "method": "shutdown"}),
                "model",
                true,
                Some(&handle),
                &pending,
                &writer,
            ),
            RpcAction::Shutdown
        );
        assert!(queue.cancel_requested());
        assert_eq!(shutdown_rx.try_recv(), Ok(false));
        assert!(lock_pending(&pending).is_empty());
    }

    #[test]
    fn turn_completion_denies_and_clears_pending_approvals() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (first_tx, mut first_rx) = oneshot::channel();
        let (second_tx, mut second_rx) = oneshot::channel();
        lock_pending(&pending).insert(1, first_tx);
        lock_pending(&pending).insert(2, second_tx);

        assert_eq!(deny_pending(&pending), 2);
        assert_eq!(first_rx.try_recv(), Ok(false));
        assert_eq!(second_rx.try_recv(), Ok(false));
        assert!(lock_pending(&pending).is_empty());
    }

    #[tokio::test]
    async fn shutdown_settles_active_turn_and_approval_before_returning() {
        let (handle, queue) = kernel::InterruptQueue::pair();
        let cancellation = queue.token();
        let mut interrupt = Some(handle);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (approval_tx, approval_rx) = oneshot::channel();
        lock_pending(&pending).insert(99, approval_tx);
        let (settled_tx, settled_rx) = oneshot::channel();
        let mut turns = JoinSet::new();
        turns.spawn(async move {
            cancellation.cancelled().await;
            let denied = approval_rx.await.unwrap_or(true);
            let _ = settled_tx.send(denied);
            TurnDone::Err("cancelled for shutdown".into())
        });

        settle_turn(&mut interrupt, &pending, &mut turns, Duration::from_secs(1)).await;

        assert!(!settled_rx.await.unwrap());
        assert!(interrupt.is_none());
        assert!(lock_pending(&pending).is_empty());
        assert!(turns.is_empty());
    }

    struct BrokenOutput;

    impl AsyncWrite for BrokenOutput {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed")))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn backpressure_never_blocks_runtime_and_cancels_only_that_connection() {
        // One byte of socket capacity ensures the first JSON frame parks the
        // writer task while the client deliberately never reads.
        let (blocked_output, _non_reading_client) = tokio::io::duplex(1);
        let Bridge {
            writer: blocked_writer,
            pending: _,
            writer_task: blocked_task,
        } = bridge_with_output(blocked_output, 2);

        let heartbeat = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            "runtime alive"
        });
        let mut saturated = false;
        for sequence in 0..32 {
            if !blocked_writer.notify("event", json!({"sequence": sequence})) {
                saturated = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(saturated, "bounded output queue did not apply backpressure");
        tokio::time::timeout(Duration::from_millis(250), blocked_writer.cancelled())
            .await
            .expect("backpressure must cancel the connection");
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(250), heartbeat)
                .await
                .expect("blocked output stalled runtime timers")
                .unwrap(),
            "runtime alive"
        );

        // A separate ACP output remains independently usable.
        let (healthy_output, mut healthy_client) = tokio::io::duplex(1024);
        let Bridge {
            writer: healthy_writer,
            pending: _,
            writer_task: healthy_task,
        } = bridge_with_output(healthy_output, 2);
        assert!(healthy_writer.notify("healthy", json!({"ok": true})));
        healthy_task.finish(&healthy_writer).await;
        let mut bytes = Vec::new();
        healthy_client.read_to_end(&mut bytes).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["method"], "healthy");

        blocked_task.finish(&blocked_writer).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broken_pipe_cancels_writer_connection() {
        let Bridge {
            writer,
            pending: _,
            writer_task,
        } = bridge_with_output(BrokenOutput, 2);
        assert!(writer.notify("event", json!({"delta": "x"})));
        tokio::time::timeout(Duration::from_millis(250), writer.cancelled())
            .await
            .expect("broken pipe did not cancel connection");
        writer_task.finish(&writer).await;
    }
}
