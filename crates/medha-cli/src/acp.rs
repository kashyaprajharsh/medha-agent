//! Editor bridge for one kernel session over line-delimited JSON-RPC 2.0 on
//! stdio. It accepts messages, approvals, and cancellation while streaming
//! event and approval notifications.

use kernel::{Budget, EventLog, Kernel, Message, Provider, Session, StopReason};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

/// Byte-bounded output queue for synchronous kernel callbacks; saturation
/// closes the connection.
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

    fn event(&self, kind: &str, mut params: Value) -> bool {
        if let Value::Object(ref mut m) = params {
            m.insert("kind".into(), json!(kind));
        }
        self.notify("event", params)
    }

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

/// Agent Client Protocol major version this bridge negotiates.
pub(crate) const ACP_PROTOCOL_VERSION: i64 = 1;

/// Which dialect the connected peer speaks. Chosen once, at `initialize`: a
/// peer sending `protocolVersion` is an ACP client, anything else is a caller
/// of Medha's original bridge, which stays supported.
#[derive(Clone)]
pub(crate) struct Peer {
    acp: Arc<AtomicBool>,
    workspace: Arc<PathBuf>,
    session_id: Arc<Mutex<Option<String>>>,
}

impl Peer {
    #[cfg(test)]
    fn new() -> Self {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::for_workspace(workspace)
    }

    fn for_workspace(workspace: PathBuf) -> Self {
        let workspace = workspace.canonicalize().unwrap_or(workspace);
        Self {
            acp: Arc::new(AtomicBool::new(false)),
            workspace: Arc::new(workspace),
            session_id: Arc::new(Mutex::new(None)),
        }
    }

    fn select_acp(&self) {
        self.acp.store(true, Ordering::Release);
    }

    pub(crate) fn is_acp(&self) -> bool {
        self.acp.load(Ordering::Acquire)
    }

    fn session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn start_session(&self, cwd: &str, mcp_servers: Option<&Value>) -> Result<String, String> {
        let requested = Path::new(cwd);
        if !requested.is_absolute() {
            return Err("session/new cwd must be an absolute path".into());
        }
        let requested = requested
            .canonicalize()
            .map_err(|error| format!("session/new cwd cannot be opened: {error}"))?;
        if requested != *self.workspace {
            return Err(format!(
                "session/new cwd {} does not match Medha workspace {}",
                requested.display(),
                self.workspace.display()
            ));
        }
        if let Some(servers) = mcp_servers {
            let servers = servers
                .as_array()
                .ok_or("session/new mcpServers must be an array")?;
            if !servers.is_empty() {
                return Err(
                    "per-session MCP servers are not supported; configure MCP before starting Medha"
                        .into(),
                );
            }
        }
        let mut session = self
            .session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if session.is_some() {
            return Err("this Medha process already owns an ACP session".into());
        }
        let id = format!("medha-{}", ulid::Ulid::new());
        *session = Some(id.clone());
        Ok(id)
    }

    fn validate_session(&self, params: &Value) -> Result<String, &'static str> {
        let requested = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("sessionId must be a string")?;
        let active = self
            .session_id()
            .ok_or("session/new must be called first")?;
        if requested != active {
            return Err("sessionId does not belong to this Medha process");
        }
        Ok(active)
    }

    /// Emit one `session/update` notification carrying `update`.
    fn update(&self, writer: &Writer, update: Value) -> bool {
        let Some(session_id) = self.session_id() else {
            return false;
        };
        writer.notify(
            "session/update",
            json!({ "sessionId": session_id, "update": update }),
        )
    }
}

pub(crate) type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<kernel::Approval>>>>;

fn lock_pending(
    pending: &Pending,
) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<kernel::Approval>>> {
    pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct Bridge {
    pub(crate) writer: Arc<Writer>,
    pub(crate) pending: Pending,
    pub(crate) peer: Peer,
    writer_task: WriterTask,
}

pub(crate) fn bridge(workspace: PathBuf) -> Bridge {
    bridge_with_output_in(tokio::io::stdout(), OUTBOUND_FRAMES, workspace)
}

#[cfg(test)]
fn bridge_with_output<W>(output: W, capacity: usize) -> Bridge
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    bridge_with_output_in(output, capacity, workspace)
}

fn bridge_with_output_in<W>(output: W, capacity: usize, workspace: PathBuf) -> Bridge
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
        peer: Peer::for_workspace(workspace),
        writer_task: WriterTask {
            handle: Some(handle),
            cancelled,
        },
    }
}

/// Human approval gate over JSON-RPC.
pub struct AcpGate {
    writer: Arc<Writer>,
    pending: Pending,
    peer: Peer,
    next_id: AtomicU64,
    always: Mutex<HashSet<String>>,
}

impl AcpGate {
    pub(crate) fn new(writer: Arc<Writer>, pending: Pending, peer: Peer) -> Self {
        Self {
            writer,
            pending,
            peer,
            next_id: AtomicU64::new(1),
            always: Mutex::new(HashSet::new()),
        }
    }

    /// ACP carries the request as a real JSON-RPC request the client answers by
    /// `optionId`; an escalated action is never offered a remembering tier.
    fn request_acp_permission(
        &self,
        gate_id: u64,
        action: &str,
        detail: Option<&str>,
        escalated: bool,
    ) -> bool {
        let mut options = vec![json!({
            "optionId": "allow-once",
            "name": "Allow once",
            "kind": "allow_once",
        })];
        if !escalated {
            options.push(json!({
                "optionId": "allow-always",
                "name": "Allow always",
                "kind": "allow_always",
            }));
        }
        options.push(json!({
            "optionId": "reject-once",
            "name": "Reject",
            "kind": "reject_once",
        }));
        self.writer.write_value(&json!({
            "jsonrpc": "2.0",
            "id": acp_gate_request_id(gate_id),
            "method": "session/request_permission",
            "params": {
                "sessionId": self.peer.session_id(),
                "toolCall": {
                    "toolCallId": acp_gate_request_id(gate_id),
                    "title": action,
                    "kind": "other",
                    "status": "pending",
                    "rawInput": { "detail": detail },
                },
                "options": options,
            },
        }))
    }
}

/// Gate ids share the outbound request-id space; the prefix keeps an editor's
/// reply unambiguous without a second table.
pub(crate) fn acp_gate_request_id(gate_id: u64) -> String {
    format!("medha-gate-{gate_id}")
}

/// The gate id inside an ACP permission response id, if it is one of ours.
pub(crate) fn acp_gate_id_from(value: &Value) -> Option<u64> {
    value
        .as_str()?
        .strip_prefix("medha-gate-")?
        .parse::<u64>()
        .ok()
}

/// Removes a pending approval if its await is cancelled.
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
        if !escalated
            && self
                .always
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(action)
        {
            return kernel::Approval::Always;
        }
        let gate_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock_pending(&self.pending).insert(gate_id, tx);
        let _guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            gate_id,
        };
        let sent = if self.peer.is_acp() {
            self.request_acp_permission(gate_id, action, detail, escalated)
        } else {
            self.writer.notify(
                "approval",
                json!({ "gate_id": gate_id, "action": action, "detail": detail, "escalated": escalated }),
            )
        };
        if !sent {
            return kernel::Approval::Deny;
        }
        // Disconnect or no response denies the action. Remembering applies to
        // this live bridge only; it never writes authority into the repository.
        let approval = tokio::select! {
            result = rx => result.unwrap_or(kernel::Approval::Deny),
            _ = self.writer.cancelled() => kernel::Approval::Deny,
        };
        if approval == kernel::Approval::Always && !escalated {
            self.always
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(action.to_string());
        }
        approval
    }
}

fn deny_pending(pending: &Pending) -> usize {
    let approvals = lock_pending(pending)
        .drain()
        .map(|(_, sender)| sender)
        .collect::<Vec<_>>();
    let count = approvals.len();
    for sender in approvals {
        let _ = sender.send(kernel::Approval::Deny);
    }
    count
}

/// Streams kernel updates as JSON-RPC `event` notifications.
struct AcpSink {
    writer: Arc<Writer>,
    peer: Peer,
}

/// Map a Medha tool to the closest ACP `ToolKind`, so an editor can pick an icon.
fn acp_tool_kind(tool: &str) -> &'static str {
    match tool {
        "read" | "ls" | "skill" => "read",
        "edit" => "edit",
        "grep" | "glob" | "code" | "sessions.search" => "search",
        "shell.exec" | "git" | "diagnostics" => "execute",
        "web" => "fetch",
        "update_plan" | "clarify" => "think",
        _ => "other",
    }
}

impl kernel::StreamSink for AcpSink {
    fn text(&self, delta: &str) {
        if self.peer.is_acp() {
            self.peer.update(
                &self.writer,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": delta },
                }),
            );
            return;
        }
        self.writer.event("model.text", json!({ "delta": delta }));
    }
    fn reasoning(&self, delta: &str) {
        if self.peer.is_acp() {
            self.peer.update(
                &self.writer,
                json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": delta },
                }),
            );
            return;
        }
        self.writer
            .event("model.reasoning", json!({ "delta": delta }));
    }
    fn tool_call(&self, tool: &str, args: &Value) {
        if self.peer.is_acp() {
            self.peer.update(
                &self.writer,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": tool,
                    "title": tool,
                    "kind": acp_tool_kind(tool),
                    "status": "in_progress",
                    "rawInput": args,
                }),
            );
            return;
        }
        self.writer
            .event("tool.call", json!({ "tool": tool, "args": args }));
    }
    fn tool_call_with_id(&self, id: &str, tool: &str, args: &Value) {
        if self.peer.is_acp() {
            self.peer.update(
                &self.writer,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "title": tool,
                    "kind": acp_tool_kind(tool),
                    "status": "in_progress",
                    "rawInput": args,
                }),
            );
            return;
        }
        self.writer
            .event("tool.call", json!({ "id": id, "tool": tool, "args": args }));
    }
    fn tool_result(&self, tool: &str, ok: bool, payload: &Value) {
        if self.peer.is_acp() {
            let text = serde_json::to_string(payload).unwrap_or_default();
            self.peer.update(
                &self.writer,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool,
                    "status": if ok { "completed" } else { "failed" },
                    "content": [{
                        "type": "content",
                        "content": { "type": "text", "text": text },
                    }],
                }),
            );
            return;
        }
        self.writer.event(
            "tool.observation",
            json!({ "tool": tool, "ok": ok, "payload": payload }),
        );
    }
    fn tool_result_with_id(&self, id: &str, tool: &str, ok: bool, payload: &Value) {
        if self.peer.is_acp() {
            let text = serde_json::to_string(payload).unwrap_or_default();
            self.peer.update(
                &self.writer,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": if ok { "completed" } else { "failed" },
                    "content": [{
                        "type": "content",
                        "content": { "type": "text", "text": text },
                    }],
                }),
            );
            return;
        }
        self.writer.event(
            "tool.observation",
            json!({ "id": id, "tool": tool, "ok": ok, "payload": payload }),
        );
    }
    fn usage(&self, usage: &kernel::Usage) {
        self.writer.event(
            "usage",
            json!({
                "prompt_tokens": usage.prompt_tokens,
                "total_tokens": usage.total_tokens,
                // Omitted rather than zeroed when the route does not report it:
                // an editor must be able to tell "no cache" from "not measured".
                "cached_prompt_tokens": usage.cached_prompt_tokens,
            }),
        );
    }
    fn context_pressure(&self, pressure: kernel::ContextPressure) {
        self.writer.event(
            "context_pressure",
            json!({
                "input_tokens": pressure.input_tokens,
                "input_limit": pressure.input_limit,
                "usable_input_tokens": pressure.usable_input_tokens,
                "quality": pressure.quality,
                "percent": pressure.percent(),
            }),
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
    fn restarted(&self) {
        self.writer.event("model.restarted", json!({}));
    }
    fn supports_restart(&self) -> bool {
        true
    }
}

enum TurnDone {
    Ok(Vec<Message>, StopReason),
    Err(String),
}

/// Bounds memory retained for one peer frame.
const MAX_FRAME: u64 = 16 * 1024 * 1024;

/// Retains partial input across cancellation; oversized frames disconnect.
async fn read_frame(
    stdin: &mut tokio::io::Take<BufReader<tokio::io::Stdin>>,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<String>> {
    let n = stdin.read_until(b'\n', buf).await?;
    if n == 0 && buf.is_empty() {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') || n == 0 {
        let line = String::from_utf8_lossy(buf).into_owned();
        buf.clear();
        stdin.set_limit(MAX_FRAME);
        return Ok(Some(line));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "frame exceeds the 16 MiB limit",
    ))
}

#[derive(Debug, PartialEq, Eq)]
enum RpcAction {
    None,
    /// `reply_to` is set for `session/prompt`, whose JSON-RPC response is the
    /// turn's `stopReason` and therefore cannot be sent until the turn settles.
    StartTurn {
        content: String,
        /// Admitted into the artifact store as the turn starts, so pixels never
        /// sit in the transcript or the log.
        images: Vec<AcpImage>,
        reply_to: Option<Value>,
    },
    Shutdown,
}

impl RpcAction {
    fn turn(content: String) -> Self {
        Self::StartTurn {
            content,
            images: Vec::new(),
            reply_to: None,
        }
    }
}

/// Decode, normalise and store what the editor attached. Errors name the image
/// by position, because the editor sends bytes rather than a path.
async fn admit_acp_images(
    images: Vec<AcpImage>,
    artifacts: &std::sync::Arc<dyn kernel::ArtifactStore>,
) -> Result<Vec<kernel::MediaPart>, String> {
    use base64::Engine;
    if images.is_empty() {
        return Ok(Vec::new());
    }
    let artifacts = artifacts.clone();
    tokio::task::spawn_blocking(move || {
        images
            .into_iter()
            .enumerate()
            .map(|(index, image)| {
                let position = index + 1;
                let label = image.label(position);
                let bytes = match &image {
                    AcpImage::Inline(_, data) => base64::engine::general_purpose::STANDARD
                        .decode(data.as_bytes())
                        .map_err(|error| {
                            format!("image {position} is not valid base64: {error}")
                        })?,
                    AcpImage::Linked(path) => std::fs::read(path)
                        .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
                };
                crate::attachments::admit(bytes, label, &artifacts)
                    .map(|attachment| attachment.part)
                    .map_err(|error| format!("image {position}: {error:#}"))
            })
            .collect()
    })
    .await
    .map_err(|error| format!("image admission task failed: {error}"))?
}

/// One editor prompt: its text, and any images the editor attached.
#[derive(Default, Debug, PartialEq)]
struct AcpPrompt {
    text: String,
    images: Vec<AcpImage>,
}

/// An image an editor attached. It arrives either as bytes in the prompt or as
/// a link to a file on this machine — dragging a file into the composer usually
/// produces the link, so reading only the inline form loses the attachment.
#[derive(Debug, PartialEq, Eq)]
enum AcpImage {
    /// `(mime type, base64 payload)`.
    Inline(String, String),
    Linked(std::path::PathBuf),
}

impl AcpImage {
    fn label(&self, position: usize) -> String {
        match self {
            Self::Inline(mime, _) => format!("image {position} ({mime})"),
            Self::Linked(path) => crate::attachments::label_for(path),
        }
    }
}

impl AcpPrompt {
    fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }

    fn push_text(&mut self, part: &str) {
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(part);
    }
}

/// The local file a `file://` URI names, if it is one.
fn acp_file_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///path` and `file://localhost/path` both address this machine.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(rest);
    Some(std::path::PathBuf::from(decoded))
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match u8::from_str_radix(&raw[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_image_mime(mime: Option<&str>) -> bool {
    mime.is_some_and(|mime| {
        mime.split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .starts_with("image/")
    })
}

/// Split ACP prompt content blocks into what the kernel consumes. Image blocks
/// were previously dropped here; an editor that attached a screenshot got an
/// answer about the text alone and no indication anything was missing.
fn acp_prompt(prompt: Option<&Value>) -> AcpPrompt {
    let mut parsed = AcpPrompt::default();
    for block in prompt.and_then(Value::as_array).into_iter().flatten() {
        let mime = block.get("mimeType").and_then(Value::as_str);
        match block.get("type").and_then(Value::as_str) {
            Some("image") => {
                if let Some(data) = block.get("data").and_then(Value::as_str) {
                    parsed.images.push(AcpImage::Inline(
                        mime.unwrap_or("image/png").to_string(),
                        data.to_string(),
                    ));
                }
            }
            // A file dragged into the composer. The editor sends where it is,
            // not what is in it, and only names the type some of the time — so
            // the extension decides when the header does not.
            Some("resource_link") => {
                let uri = block.get("uri").and_then(Value::as_str).unwrap_or_default();
                if let Some(path) = acp_file_path(uri).filter(|path| {
                    is_image_mime(mime)
                        || crate::attachments::refs::has_image_extension(&path.to_string_lossy())
                }) {
                    parsed.images.push(AcpImage::Linked(path));
                } else {
                    parsed.push_text(&format!("[attached resource: {uri}]"));
                }
            }
            Some("resource") => {
                let resource = block.get("resource");
                let resource_mime = resource
                    .and_then(|resource| resource.get("mimeType"))
                    .and_then(Value::as_str);
                let blob = resource
                    .and_then(|resource| resource.get("blob"))
                    .and_then(Value::as_str);
                match blob.filter(|_| is_image_mime(resource_mime)) {
                    Some(blob) => parsed.images.push(AcpImage::Inline(
                        resource_mime.unwrap_or("image/png").to_string(),
                        blob.to_string(),
                    )),
                    None => {
                        if let Some(text) = resource
                            .and_then(|resource| resource.get("text"))
                            .and_then(Value::as_str)
                        {
                            parsed.push_text(text);
                        }
                    }
                }
            }
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parsed.push_text(text);
                }
            }
            _ => {}
        }
    }
    parsed
}

/// Medha's stop reasons in ACP's vocabulary.
fn acp_stop_reason(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::Finished => "end_turn",
        StopReason::Interrupted => "cancelled",
        StopReason::VerificationFailed | StopReason::Blocked => "refusal",
        StopReason::Budget(kernel::BudgetStop::Tokens) => "max_tokens",
        StopReason::Budget(_) => "max_turn_requests",
    }
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

/// Requests receive one response; notifications receive none.
fn dispatch_rpc(
    message: Value,
    model: &str,
    running: bool,
    interrupt: Option<&kernel::InterruptHandle>,
    pending: &Pending,
    writer: &Writer,
    peer: &Peer,
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
    // An ACP client answers `session/request_permission` with a response frame,
    // which carries no method. Route it to the waiting gate.
    if object.get("method").is_none()
        && let Some(gate_id) = id.as_ref().and_then(acp_gate_id_from)
    {
        let chosen = object
            .get("result")
            .and_then(|result| result.get("outcome"))
            .and_then(|outcome| {
                (outcome.get("outcome").and_then(Value::as_str) == Some("selected"))
                    .then(|| outcome.get("optionId").and_then(Value::as_str))
                    .flatten()
            });
        let approval = match chosen {
            Some("allow-once") => kernel::Approval::Once,
            Some("allow-always") => kernel::Approval::Always,
            _ => kernel::Approval::Deny,
        };
        if let Some(sender) = lock_pending(pending).remove(&gate_id) {
            let _ = sender.send(approval);
        }
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
        // A peer that names a protocolVersion is an ACP client; everything else
        // is a caller of Medha's original bridge, which keeps working unchanged.
        "initialize" if params.get("protocolVersion").is_some() => {
            peer.select_acp();
            rpc_result(
                writer,
                &id,
                json!({
                    "protocolVersion": ACP_PROTOCOL_VERSION,
                    "agentInfo": { "name": "medha", "version": env!("CARGO_PKG_VERSION") },
                    "agentCapabilities": {
                        "loadSession": false,
                        "promptCapabilities": {
                            "image": true,
                            "audio": false,
                            "embeddedContext": true,
                        },
                    },
                    "authMethods": [],
                }),
            );
            RpcAction::None
        }
        "authenticate" => {
            rpc_result(writer, &id, json!({}));
            RpcAction::None
        }
        "session/new" => {
            if !peer.is_acp() {
                rpc_error(
                    writer,
                    &id,
                    -32000,
                    "initialize must be called before session/new",
                );
                return RpcAction::None;
            }
            let Some(cwd) = params.get("cwd").and_then(Value::as_str) else {
                rpc_error(writer, &id, -32602, "session/new cwd must be a string");
                return RpcAction::None;
            };
            match peer.start_session(cwd, params.get("mcpServers")) {
                Ok(session_id) => rpc_result(writer, &id, json!({ "sessionId": session_id })),
                Err(error) => rpc_error(writer, &id, -32602, error),
            }
            RpcAction::None
        }
        "session/prompt" => {
            if let Err(error) = peer.validate_session(&params) {
                rpc_error(writer, &id, -32602, error);
                return RpcAction::None;
            }
            let prompt = acp_prompt(params.get("prompt"));
            if prompt.is_empty() {
                rpc_error(writer, &id, -32602, "prompt must contain text or an image");
                return RpcAction::None;
            }
            if prompt.images.len() > crate::attachments::MAX_PER_MESSAGE {
                rpc_error(
                    writer,
                    &id,
                    -32602,
                    format!(
                        "at most {} images per prompt",
                        crate::attachments::MAX_PER_MESSAGE
                    ),
                );
                return RpcAction::None;
            }
            if running {
                // A steer carries text only. Refusing is the honest answer:
                // accepting would drop the image the editor just attached.
                if !prompt.images.is_empty() {
                    rpc_error(
                        writer,
                        &id,
                        -32000,
                        "images cannot be added to a turn that is already running",
                    );
                } else if let Some(handle) = interrupt {
                    handle.steer(prompt.text);
                    rpc_result(writer, &id, json!({ "stopReason": "end_turn" }));
                } else {
                    rpc_error(writer, &id, -32000, "a turn is already running");
                }
                return RpcAction::None;
            }
            RpcAction::StartTurn {
                content: if prompt.text.trim().is_empty() {
                    crate::attachments::IMAGE_ONLY_PROMPT.to_string()
                } else {
                    prompt.text
                },
                images: prompt.images,
                reply_to: id,
            }
        }
        "session/cancel" => {
            if let Err(error) = peer.validate_session(&params) {
                rpc_error(writer, &id, -32602, error);
                return RpcAction::None;
            }
            if let Some(handle) = interrupt {
                handle.cancel_turn();
            }
            deny_pending(pending);
            RpcAction::None
        }
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
                RpcAction::turn(content)
            }
        }
        "approval.respond" => {
            let gate_id = params.get("gate_id").and_then(Value::as_u64);
            let decision = match (
                params.get("approve").and_then(Value::as_bool),
                params.get("decision").and_then(Value::as_str),
            ) {
                (Some(true), _) => Some(kernel::Approval::Once),
                (Some(false), _) => Some(kernel::Approval::Deny),
                (None, Some("approve")) => Some(kernel::Approval::Once),
                (None, Some("deny")) => Some(kernel::Approval::Deny),
                _ => None,
            };
            let (Some(gate_id), Some(approval)) = (gate_id, decision) else {
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
                let _ = sender.send(approval);
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
    peer: &Peer,
) -> RpcAction {
    match serde_json::from_str::<Value>(line) {
        Ok(message) => dispatch_rpc(message, model, running, interrupt, pending, writer, peer),
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

/// Mid-turn messages steer at the next boundary; cancellation lets in-flight
/// tools settle through the kernel interrupt handle.
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
        peer,
        writer_task,
    } = bridge;
    let mut prompt_reply: Option<Value> = None;
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
                match dispatch_line(trimmed, &model, running, interrupt.as_ref(), &pending, &writer, &peer) {
                    RpcAction::None => {}
                    RpcAction::Shutdown => break,
                    RpcAction::StartTurn { content, images, reply_to } => {
                        prompt_reply = reply_to;
                        let mut prompt = Message::user(content);
                        match admit_acp_images(images, &kernel.artifacts).await {
                            Ok(attachments) => prompt.attachments = attachments,
                            Err(error) => {
                                // The editor attached an image Medha cannot
                                // read. Answering the text alone would look
                                // like it was seen, so the turn does not start.
                                if let Some(id) = prompt_reply.take() {
                                    rpc_error(&writer, &Some(id), -32602, &error);
                                }
                                continue;
                            }
                        }
                        transcript.push(prompt);
                        running = true;
                        let (handle, queue) = kernel::InterruptQueue::pair();
                        interrupt = Some(handle);
                        let kernel = kernel.clone();
                        let session = session.clone();
                        let messages = transcript.clone();
                        // Each editor message gets a fresh task budget shared
                        // with descendants spawned during that turn.
                        let budget = crate::task_budget(&base_budget, &agent_budget);
                        let writer = writer.clone();
                        let peer = peer.clone();
                        turns.spawn(async move {
                            let sink = AcpSink { writer, peer };
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
                // `session/prompt` is answered here, not at dispatch: its result
                // is the turn's stopReason.
                let reply = prompt_reply.take();
                match joined {
                    Some(Ok(TurnDone::Ok(updated, reason))) => {
                        transcript = updated;
                        if let Some(id) = reply {
                            writer.respond(id, json!({ "stopReason": acp_stop_reason(&reason) }));
                        } else {
                            match reason {
                                StopReason::VerificationFailed => writer.event("turn.done", json!({ "stopped": "verification_failed" })),
                                StopReason::Interrupted => writer.event("turn.cancelled", json!({})),
                                StopReason::Budget(s) => writer.event("turn.done", json!({ "stopped": s.label() })),
                                StopReason::Finished => writer.event("turn.done", json!({ "stopped": Value::Null })),
                                StopReason::Blocked => writer.event("turn.done", json!({ "stopped": "blocked_by_hook" })),
                            };
                        }
                    }
                    Some(Ok(TurnDone::Err(e))) => {
                        match reply {
                            Some(id) => { writer.error(id, -32000, e); }
                            None => { writer.event("turn.error", json!({ "message": e })); }
                        }
                    }
                    Some(Err(error)) => {
                        let message = format!("turn task failed: {error}");
                        match reply {
                            Some(id) => { writer.error(id, -32000, message); }
                            None => { writer.event("turn.error", json!({ "message": message })); }
                        }
                    }
                    None => {
                        match reply {
                            Some(id) => { writer.error(id, -32000, "turn task disappeared"); }
                            None => { writer.event("turn.error", json!({ "message": "turn task disappeared" })); }
                        }
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
    use kernel::{Approval, HumanGate, Role, StreamSink};
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
        let action = dispatch_rpc(
            message,
            "test-model",
            running,
            interrupt,
            pending,
            &writer,
            &Peer::new(),
        );
        (action, captured_values(&mut rx))
    }

    /// An ACP client's whole opening exchange, in the order an editor sends it.
    #[test]
    fn an_acp_client_completes_the_standard_session_handshake() {
        let peer = Peer::new();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (writer, mut rx) = capture_writer(32);
        let send = |message: Value, running: bool| -> RpcAction {
            dispatch_rpc(message, "m", running, None, &pending, &writer, &peer)
        };

        send(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": 1, "clientCapabilities": {}}}),
            false,
        );
        assert!(
            peer.is_acp(),
            "protocolVersion selects the standard dialect"
        );
        let initialized = captured_values(&mut rx);
        assert_eq!(initialized[0]["result"]["protocolVersion"], json!(1));
        assert_eq!(
            initialized[0]["result"]["agentCapabilities"]["loadSession"],
            json!(false)
        );

        let cwd = peer.workspace.display().to_string();
        send(
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
                   "params": {"cwd": cwd, "mcpServers": []}}),
            false,
        );
        let created = captured_values(&mut rx);
        let session_id = created[0]["result"]["sessionId"].clone();
        assert_eq!(session_id, json!(peer.session_id()));

        let action = send(
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                   "params": {"sessionId": session_id,
                              "prompt": [{"type": "text", "text": "fix the test"}]}}),
            false,
        );
        // The response is the turn's stopReason, so it must be deferred.
        assert_eq!(
            action,
            RpcAction::StartTurn {
                content: "fix the test".into(),
                images: Vec::new(),
                reply_to: Some(json!(3)),
            }
        );
        assert!(
            captured_values(&mut rx).is_empty(),
            "session/prompt must not answer before the turn ends"
        );
    }

    #[test]
    fn acp_rejects_duplicate_foreign_and_unimplemented_session_inputs() {
        fn initialize(peer: &Peer, writer: &Writer, pending: &Pending) {
            dispatch_rpc(
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                       "params": {"protocolVersion": 1}}),
                "m",
                false,
                None,
                pending,
                writer,
                peer,
            );
        }

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (writer, mut rx) = capture_writer(32);
        let peer = Peer::new();
        initialize(&peer, &writer, &pending);
        captured_values(&mut rx);
        let cwd = peer.workspace.display().to_string();
        dispatch_rpc(
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
                   "params": {"cwd": cwd, "mcpServers": []}}),
            "m",
            false,
            None,
            &pending,
            &writer,
            &peer,
        );
        let created = captured_values(&mut rx);
        let session_id = created[0]["result"]["sessionId"].clone();

        dispatch_rpc(
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/new",
                   "params": {"cwd": peer.workspace.display().to_string(), "mcpServers": []}}),
            "m",
            false,
            None,
            &pending,
            &writer,
            &peer,
        );
        assert!(captured_values(&mut rx)[0].get("error").is_some());

        dispatch_rpc(
            json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt",
                   "params": {"sessionId": "foreign", "prompt": [{"type": "text", "text": "x"}]}}),
            "m",
            false,
            None,
            &pending,
            &writer,
            &peer,
        );
        assert!(captured_values(&mut rx)[0].get("error").is_some());

        let second = Peer::new();
        initialize(&second, &writer, &pending);
        captured_values(&mut rx);
        dispatch_rpc(
            json!({"jsonrpc": "2.0", "id": 5, "method": "session/new",
                   "params": {"cwd": second.workspace.display().to_string(),
                              "mcpServers": [{"name": "repo-server"}]}}),
            "m",
            false,
            None,
            &pending,
            &writer,
            &second,
        );
        assert!(captured_values(&mut rx)[0].get("error").is_some());
        assert_eq!(peer.session_id().map(Value::String), Some(session_id));
    }

    #[test]
    fn an_acp_permission_response_reaches_the_waiting_gate() {
        let peer = Peer::new();
        peer.select_acp();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (writer, mut rx) = capture_writer(8);
        let (tx, mut answered) = oneshot::channel();
        lock_pending(&pending).insert(7, tx);

        dispatch_rpc(
            json!({"jsonrpc": "2.0", "id": acp_gate_request_id(7),
                   "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}}),
            "m",
            true,
            None,
            &pending,
            &writer,
            &peer,
        );
        assert_eq!(answered.try_recv(), Ok(Approval::Once));
        assert!(lock_pending(&pending).is_empty());

        let (tx, mut cancelled) = oneshot::channel();
        lock_pending(&pending).insert(8, tx);
        dispatch_rpc(
            json!({"jsonrpc": "2.0", "id": acp_gate_request_id(8),
                   "result": {"outcome": {"outcome": "cancelled"}}}),
            "m",
            true,
            None,
            &pending,
            &writer,
            &peer,
        );
        assert_eq!(
            cancelled.try_recv(),
            Ok(Approval::Deny),
            "cancelled is not consent"
        );
        captured_values(&mut rx);
    }

    #[test]
    fn escalated_permission_options_omit_the_remembering_tier() {
        let peer = Peer::new();
        peer.select_acp();
        let (writer, mut rx) = capture_writer(8);
        let gate = AcpGate::new(
            Arc::clone(&writer),
            Arc::new(Mutex::new(HashMap::new())),
            peer,
        );

        gate.request_acp_permission(1, "shell.exec", Some("cargo test"), true);
        let escalated = captured_values(&mut rx);
        assert_eq!(
            escalated[0]["params"]["toolCall"]["rawInput"]["detail"],
            "cargo test"
        );
        let kinds: Vec<&str> = escalated[0]["params"]["options"]
            .as_array()
            .expect("options array")
            .iter()
            .filter_map(|option| option["kind"].as_str())
            .collect();
        assert_eq!(kinds, ["allow_once", "reject_once"]);

        gate.request_acp_permission(2, "shell.exec", None, false);
        let plain = captured_values(&mut rx);
        assert!(
            plain[0]["params"]["options"]
                .as_array()
                .expect("options array")
                .iter()
                .any(|option| option["kind"] == "allow_always")
        );
    }

    #[tokio::test]
    async fn acp_always_is_remembered_for_the_live_non_escalated_action() {
        let peer = Peer::new();
        peer.select_acp();
        peer.start_session(&peer.workspace.display().to_string(), Some(&json!([])))
            .unwrap();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (writer, mut rx) = capture_writer(8);
        let gate = Arc::new(AcpGate::new(
            Arc::clone(&writer),
            Arc::clone(&pending),
            peer.clone(),
        ));
        let waiting = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.confirm("edit", Some("a.rs"), false).await })
        };
        let frame = rx.recv().await.expect("permission request");
        let request: Value = match frame {
            Outbound::Frame(frame) => serde_json::from_slice(&frame).unwrap(),
            Outbound::Close(_) => panic!("writer closed"),
        };
        dispatch_rpc(
            json!({"jsonrpc": "2.0", "id": request["id"].clone(),
                   "result": {"outcome": {"outcome": "selected", "optionId": "allow-always"}}}),
            "m",
            true,
            None,
            &pending,
            &writer,
            &peer,
        );
        assert_eq!(waiting.await.unwrap(), Approval::Always);
        assert_eq!(
            gate.confirm("edit", Some("b.rs"), false).await,
            Approval::Always
        );
        assert!(rx.try_recv().is_err(), "remembered approval prompted again");
    }

    #[test]
    fn acp_tool_updates_keep_the_provider_call_id() {
        let peer = Peer::new();
        peer.select_acp();
        peer.start_session(&peer.workspace.display().to_string(), Some(&json!([])))
            .unwrap();
        let (writer, mut rx) = capture_writer(8);
        let sink = AcpSink { writer, peer };
        sink.tool_call_with_id("call-17", "read", &json!({"path": "a.rs"}));
        sink.tool_result_with_id("call-17", "read", true, &json!({"content": "x"}));
        let updates = captured_values(&mut rx);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0]["params"]["update"]["toolCallId"], "call-17");
        assert_eq!(updates[1]["params"]["update"]["toolCallId"], "call-17");
    }

    #[test]
    fn medha_stop_reasons_map_onto_the_acp_vocabulary() {
        assert_eq!(acp_stop_reason(&StopReason::Finished), "end_turn");
        assert_eq!(acp_stop_reason(&StopReason::Interrupted), "cancelled");
        assert_eq!(acp_stop_reason(&StopReason::VerificationFailed), "refusal");
        assert_eq!(
            acp_stop_reason(&StopReason::Budget(kernel::BudgetStop::Tokens)),
            "max_tokens"
        );
    }

    #[test]
    fn acp_prompt_blocks_keep_text_resources_and_images() {
        let prompt = json!([
            {"type": "text", "text": "explain"},
            {"type": "resource", "resource": {"uri": "file:///a.rs", "text": "fn main() {}"}},
            {"type": "image", "mimeType": "image/jpeg", "data": "QUJD"},
            {"type": "audio", "data": "not supported"},
        ]);

        let parsed = acp_prompt(Some(&prompt));

        assert_eq!(parsed.text, "explain\nfn main() {}");
        assert_eq!(
            parsed.images,
            [AcpImage::Inline("image/jpeg".into(), "QUJD".into())]
        );
        assert_eq!(acp_prompt(None), AcpPrompt::default());
        assert!(acp_prompt(None).is_empty());
    }

    /// Dragging a file into an editor's composer usually sends a link to it,
    /// not its bytes. Reading only the inline form loses the attachment with no
    /// sign anything was missing.
    #[test]
    fn a_linked_image_file_is_an_attachment() {
        let by_mime = acp_prompt(Some(&json!([
            {"type": "resource_link", "uri": "file:///tmp/a%20shot.png",
             "name": "a shot.png", "mimeType": "image/png"},
        ])));
        assert_eq!(
            by_mime.images,
            [AcpImage::Linked("/tmp/a shot.png".into())],
            "a percent-encoded file URI names a real path"
        );

        // Editors do not always send a mimeType; the extension decides then.
        let by_extension = acp_prompt(Some(&json!([
            {"type": "resource_link", "uri": "file:///tmp/diagram.JPEG"},
        ])));
        assert_eq!(
            by_extension.images,
            [AcpImage::Linked("/tmp/diagram.JPEG".into())]
        );
    }

    /// A linked file that is not an image still has to be visible: saying so is
    /// what stops the model answering as though nothing was attached.
    #[test]
    fn a_linked_non_image_is_named_rather_than_dropped() {
        let parsed = acp_prompt(Some(&json!([
            {"type": "resource_link", "uri": "file:///tmp/report.pdf", "mimeType": "application/pdf"},
            {"type": "resource_link", "uri": "https://example.com/remote.png", "mimeType": "image/png"},
        ])));

        assert!(parsed.images.is_empty(), "no bytes to attach");
        assert_eq!(
            parsed.text,
            "[attached resource: file:///tmp/report.pdf]\n\
             [attached resource: https://example.com/remote.png]"
        );
    }

    #[test]
    fn an_embedded_blob_resource_is_an_image_and_text_resources_still_are_not() {
        let parsed = acp_prompt(Some(&json!([
            {"type": "resource", "resource": {"uri": "file:///a.png", "mimeType": "image/png", "blob": "QUJD"}},
            {"type": "resource", "resource": {"uri": "file:///a.rs", "mimeType": "text/rust", "text": "fn main() {}"}},
        ])));

        assert_eq!(
            parsed.images,
            [AcpImage::Inline("image/png".into(), "QUJD".into())]
        );
        assert_eq!(parsed.text, "fn main() {}");
    }

    #[test]
    fn an_image_only_prompt_is_a_prompt() {
        let parsed = acp_prompt(Some(&json!([
            {"type": "image", "data": "QUJD"},
        ])));

        assert!(!parsed.is_empty(), "an image alone is enough to answer");
        assert_eq!(
            parsed.images,
            [AcpImage::Inline("image/png".into(), "QUJD".into())]
        );
    }

    #[tokio::test]
    async fn an_image_only_prompt_carries_the_shared_default_text() {
        let peer = Peer::new();
        let (writer, mut rx) = capture_writer(8);
        let session = peer
            .start_session(&peer.workspace.display().to_string(), Some(&json!([])))
            .unwrap();

        let action = dispatch_line(
            &json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt",
                    "params": {"sessionId": session,
                               "prompt": [{"type": "image", "mimeType": "image/png", "data": "QUJD"}]}})
                .to_string(),
            "m",
            false,
            None,
            &Default::default(),
            &writer,
            &peer,
        );

        assert_eq!(
            action,
            RpcAction::StartTurn {
                content: crate::attachments::IMAGE_ONLY_PROMPT.to_string(),
                images: vec![AcpImage::Inline("image/png".into(), "QUJD".into())],
                reply_to: Some(json!(4)),
            }
        );
        let _ = captured_values(&mut rx);
    }

    /// Steering carries text only, so an image arriving mid-turn is refused
    /// rather than quietly dropped from the message that mentions it.
    #[tokio::test]
    async fn an_image_cannot_join_a_running_turn() {
        let peer = Peer::new();
        let (writer, mut rx) = capture_writer(8);
        let session = peer
            .start_session(&peer.workspace.display().to_string(), Some(&json!([])))
            .unwrap();
        let (handle, _queue) = kernel::InterruptQueue::pair();

        let action = dispatch_line(
            &json!({"jsonrpc": "2.0", "id": 5, "method": "session/prompt",
                    "params": {"sessionId": session,
                               "prompt": [{"type": "text", "text": "and this"},
                                          {"type": "image", "data": "QUJD"}]}})
            .to_string(),
            "m",
            true,
            Some(&handle),
            &Default::default(),
            &writer,
            &peer,
        );

        assert_eq!(action, RpcAction::None);
        let replies = captured_values(&mut rx);
        let message = replies
            .iter()
            .filter_map(|value| value.pointer("/error/message").and_then(Value::as_str))
            .collect::<String>();
        assert!(message.contains("already running"), "{message}");
    }

    #[tokio::test]
    async fn editors_are_told_images_are_accepted() {
        let peer = Peer::new();
        let (writer, mut rx) = capture_writer(8);

        dispatch_line(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": ACP_PROTOCOL_VERSION}})
            .to_string(),
            "m",
            false,
            None,
            &Default::default(),
            &writer,
            &peer,
        );

        let replies = captured_values(&mut rx);
        assert_eq!(
            replies[0].pointer("/result/agentCapabilities/promptCapabilities/image"),
            Some(&json!(true))
        );
    }

    /// The original bridge must keep working for callers that already use it.
    #[test]
    fn a_legacy_initialize_does_not_switch_dialects() {
        let peer = Peer::new();
        let (writer, mut rx) = capture_writer(8);
        dispatch_rpc(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
            "m",
            false,
            None,
            &Arc::new(Mutex::new(HashMap::new())),
            &writer,
            &peer,
        );
        assert!(!peer.is_acp());
        assert_eq!(captured_values(&mut rx)[0]["result"]["proto"], json!("1.0"));
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
        assert_eq!(action, RpcAction::turn("hello".into()));
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
        assert_eq!(approval_rx.try_recv(), Ok(Approval::Once));
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
            assert_eq!(approval_rx.try_recv(), Ok(Approval::Deny));
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
            assert_eq!(approval_rx.try_recv(), Ok(Approval::Deny));
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
                &Peer::new(),
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
                &Peer::new(),
            );
        }

        assert_eq!(approval_rx.try_recv(), Ok(Approval::Deny));
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
        let gate = Arc::new(AcpGate::new(
            Arc::clone(&writer),
            Arc::clone(&pending),
            Peer::new(),
        ));

        let dropped_gate = Arc::clone(&gate);
        let dropped = tokio::spawn(async move { dropped_gate.confirm("edit", None, false).await });
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
        dispatch_line(
            "{broken",
            "model",
            true,
            None,
            &pending,
            &writer,
            &Peer::new(),
        );
        assert_eq!(malformed_rx.try_recv(), Ok(Approval::Deny));
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
                &Peer::new(),
            ),
            RpcAction::Shutdown
        );
        assert!(queue.cancel_requested());
        assert_eq!(shutdown_rx.try_recv(), Ok(Approval::Deny));
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
        assert_eq!(first_rx.try_recv(), Ok(Approval::Deny));
        assert_eq!(second_rx.try_recv(), Ok(Approval::Deny));
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
            let denied = approval_rx.await.unwrap_or(Approval::Deny);
            let _ = settled_tx.send(denied);
            TurnDone::Err("cancelled for shutdown".into())
        });

        settle_turn(&mut interrupt, &pending, &mut turns, Duration::from_secs(1)).await;

        assert_eq!(settled_rx.await.unwrap(), Approval::Deny);
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
        let (blocked_output, _non_reading_client) = tokio::io::duplex(1);
        let Bridge {
            writer: blocked_writer,
            pending: _,
            peer: _,
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

        let (healthy_output, mut healthy_client) = tokio::io::duplex(1024);
        let Bridge {
            writer: healthy_writer,
            pending: _,
            peer: _,
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
            peer: _,
            writer_task,
        } = bridge_with_output(BrokenOutput, 2);
        assert!(writer.notify("event", json!({"delta": "x"})));
        tokio::time::timeout(Duration::from_millis(250), writer.cancelled())
            .await
            .expect("broken pipe did not cancel connection");
        writer_task.finish(&writer).await;
    }
}
