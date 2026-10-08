//! Backend chat execution over the shared application protocol. External editor
//! JSON-RPC is translated by `editor`, which never constructs a kernel.

use kernel::{Budget, EventLog, Kernel, Message, Session, StopReason};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

const OUTBOUND_FRAMES: usize = 256;
// Leave room for the backend's session-event envelope.
const MAX_OUTBOUND_FRAME: usize = wire::MAX_FRAME - 1024;
const MAX_QUEUED_BYTES: usize = 32 * 1024 * 1024;
const WRITER_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const TURN_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const TURN_ABORT_GRACE: Duration = Duration::from_secs(2);
const ROSTER_INTERVAL: Duration = Duration::from_millis(500);
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
    progress: Arc<Notify>,
    failure: Arc<Mutex<Option<&'static str>>>,
    presentation: Mutex<crate::chat_presentation::Presentation>,
    presentation_enabled: AtomicBool,
}

impl Writer {
    pub(crate) fn enable_presentation(&self) {
        self.presentation_enabled.store(true, Ordering::Release);
    }

    fn presents(&self) -> bool {
        self.presentation_enabled.load(Ordering::Acquire)
    }
    fn fail(&self, reason: &'static str) {
        let mut failure = self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        failure.get_or_insert(reason);
        self.cancelled.cancel();
    }

    /// Synchronous stream callbacks cannot wait, but incoming RPCs can. Leave
    /// headroom for their notifications and reply instead of ending a healthy
    /// chat when a fast reader supplies a burst of small requests.
    pub(crate) async fn wait_for_room(&self) {
        let spare = 8.min(self.tx.max_capacity().div_ceil(2));
        loop {
            let changed = self.progress.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.cancelled.is_cancelled()
                || (self.tx.capacity() >= spare
                    && self.queued_bytes.load(Ordering::Acquire)
                        <= MAX_QUEUED_BYTES - MAX_OUTBOUND_FRAME)
            {
                return;
            }
            tokio::select! {
                _ = self.cancelled.cancelled() => return,
                _ = changed => {}
            }
        }
    }

    pub(crate) fn write_value(&self, value: &Value) -> bool {
        if self.cancelled.is_cancelled() {
            return false;
        }
        let Ok(mut frame) = serde_json::to_vec(value) else {
            self.fail("Could not encode the chat's output.");
            return false;
        };
        if frame.len() + 1 > MAX_OUTBOUND_FRAME
            && let Some(id) = value.get("id")
            && (value.get("result").is_some() || value.get("error").is_some())
        {
            frame = serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32001, "message": "The chat's answer is too large."}}))
            .expect("a JSON value can be serialized");
        }
        self.enqueue(frame)
    }

    fn enqueue(&self, mut frame: Vec<u8>) -> bool {
        frame.push(b'\n');
        let frame_len = frame.len();
        if frame_len > MAX_OUTBOUND_FRAME {
            self.fail("The chat's output exceeded the frame limit.");
            return false;
        }
        if self
            .queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued
                    .checked_add(frame_len)
                    .filter(|total| *total <= MAX_QUEUED_BYTES)
            })
            .is_err()
        {
            self.fail("The chat's output queue is full; its viewer could not keep up.");
            return false;
        }
        if self.tx.try_send(Outbound::Frame(frame)).is_err() {
            self.queued_bytes.fetch_sub(frame_len, Ordering::AcqRel);
            self.fail("The chat's output queue is full or disconnected.");
            return false;
        }
        true
    }

    /// Emit a JSON-RPC notification (no id, no response expected).
    pub fn notify(&self, method: &str, params: Value) -> bool {
        self.notify_params(method, &params)
    }

    pub(crate) fn notify_params<T: serde::Serialize>(&self, method: &str, params: &T) -> bool {
        #[derive(serde::Serialize)]
        struct Notification<'a, T> {
            jsonrpc: &'static str,
            method: &'a str,
            params: &'a T,
        }
        if self.cancelled.is_cancelled() {
            return false;
        }
        // Serialize directly: typed tool payloads must not be cloned again
        // into an intermediate JSON value merely to add the envelope.
        match serde_json::to_vec(&Notification {
            jsonrpc: "2.0",
            method,
            params,
        }) {
            Ok(frame) => {
                if !self.presents() {
                    return self.enqueue(frame);
                }
                // Projection mutation and enqueue share an ordering barrier.
                // A snapshot cannot include text whose notification is still
                // waiting to be enqueued behind the snapshot's own reply.
                let mut presentation = self
                    .presentation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if matches!(
                    method,
                    "event"
                        | "settings"
                        | "approval"
                        | "approval.resolved"
                        | "question"
                        | "question.answered"
                        | "agent.step"
                ) {
                    let mut value: Value =
                        serde_json::from_slice(&frame).expect("just serialized notification");
                    if let Err(reason) = presentation.observe(method, value["params"].take()) {
                        self.fail(reason);
                        return false;
                    }
                }
                self.enqueue(frame)
            }
            Err(_) => {
                self.fail("Could not encode the chat's output.");
                false
            }
        }
    }

    fn event(&self, kind: &str, mut params: Value) -> bool {
        if let Value::Object(ref mut m) = params {
            m.insert("kind".into(), json!(kind));
        }
        self.notify("event", params)
    }

    fn turn_event(&self, event: protocol::TurnEvent) -> bool {
        self.notify_params("event", &event)
    }

    pub(crate) fn respond(&self, id: Value, result: Value) -> bool {
        self.write_value(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn seed_presentation(&self, session: &Session, transcript: &[Message], settings: Value) {
        if !self.presents() {
            return;
        }
        let Ok(settings) = serde_json::from_value(settings) else {
            return;
        };
        let mut presentation = self
            .presentation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        presentation.seed(session.id.to_string(), transcript, settings);
        let revision = presentation.revision();
        // This reset is part of the same output order as the new baseline.
        self.write_value(&json!({"jsonrpc": "2.0", "method": "event", "params": {"kind": "presentation.reset", "revision": revision}}));
    }

    fn presentation(&self, id: Value) {
        let presentation = self
            .presentation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.respond(
            id,
            serde_json::to_value(presentation.snapshot())
                .expect("presentation snapshot serializes"),
        );
    }

    fn mark_history(&self) {
        self.presentation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mark_history();
    }

    fn history(&self, id: Value, mut page: protocol::HistoryFragment, first: bool, running: bool) {
        let presentation = self
            .presentation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if first && running {
            match presentation.history_tail() {
                Ok(items) => page.live = Some(items),
                Err(error) => {
                    self.error(id, -32002, error);
                    return;
                }
            }
        }
        self.respond(
            id,
            serde_json::to_value(page).expect("history page serializes"),
        );
    }

    fn reset_presentation<P: kernel::Provider>(
        &self,
        provider: &P,
        session: &Session,
        transcript: &[Message],
        active: &str,
        model: &str,
        profiles: &Arc<Mutex<crate::config::Config>>,
    ) {
        if !self.presents() {
            return;
        }
        let cfg = profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.seed_presentation(
            session,
            transcript,
            crate::desktop_controls::settings(provider, session, active, model, &cfg),
        );
    }

    pub(crate) fn error(&self, id: Value, code: i32, message: impl Into<String>) -> bool {
        self.write_value(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message.into() }
        }))
    }

    pub(crate) async fn cancelled(&self) {
        self.cancelled.cancelled().await;
    }
}

async fn writer_loop<W>(
    mut output: W,
    mut rx: mpsc::Receiver<Outbound>,
    queued_bytes: Arc<AtomicUsize>,
    cancelled: CancellationToken,
    progress: Arc<Notify>,
    failure: Arc<Mutex<Option<&'static str>>>,
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
                    _ = cancelled.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "Output connection cancelled")),
                    result = output.write_all(&frame) => result,
                };
                let result = match result {
                    Ok(()) => tokio::select! {
                        _ = cancelled.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "Output connection cancelled")),
                        result = output.flush() => result,
                    },
                    Err(error) => Err(error),
                };
                queued_bytes.fetch_sub(len, Ordering::AcqRel);
                progress.notify_waiters();
                if result.is_err() {
                    if !cancelled.is_cancelled() {
                        failure
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get_or_insert("The chat's output connection failed.");
                    }
                    cancelled.cancel();
                    break;
                }
            }
            Outbound::Close(ack) => {
                let result = tokio::select! {
                    _ = cancelled.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "Output connection cancelled")),
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

pub(crate) struct WriterTask {
    handle: Option<JoinHandle<()>>,
    cancelled: CancellationToken,
}

impl WriterTask {
    pub(crate) async fn finish(mut self, writer: &Writer) {
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

enum GateAnswer {
    Action(oneshot::Sender<kernel::Approval>),
    Access(oneshot::Sender<kernel::NetworkDecision>),
    Path {
        answer: oneshot::Sender<kernel::PathApproval>,
        folder_offered: bool,
    },
}

pub(crate) struct PendingGate {
    answer: GateAnswer,
    escalated: bool,
}

impl From<oneshot::Sender<kernel::Approval>> for PendingGate {
    fn from(answer: oneshot::Sender<kernel::Approval>) -> Self {
        Self {
            answer: GateAnswer::Action(answer),
            escalated: false,
        }
    }
}

impl PendingGate {
    fn send(self, approval: kernel::Approval) -> Result<(), ()> {
        let approval = if self.escalated && approval == kernel::Approval::Always {
            kernel::Approval::Deny
        } else {
            approval
        };
        match self.answer {
            GateAnswer::Action(answer) => answer.send(approval).map_err(|_| ()),
            GateAnswer::Access(answer) => answer
                .send(match approval {
                    kernel::Approval::Once => kernel::NetworkDecision::Once,
                    kernel::Approval::Always => kernel::NetworkDecision::Persistent,
                    kernel::Approval::Deny => kernel::NetworkDecision::Deny,
                })
                .map_err(|_| ()),
            GateAnswer::Path { answer, .. } => answer.send(approval.into()).map_err(|_| ()),
        }
    }

    fn permits(&self, decision: protocol::ApprovalDecision) -> bool {
        use protocol::ApprovalDecision as D;
        if self.escalated && matches!(decision, D::Always | D::Session | D::Persistent) {
            return false;
        }
        match self.answer {
            GateAnswer::Action(_) => matches!(decision, D::Approve | D::Once | D::Always | D::Deny),
            GateAnswer::Access(_) => matches!(
                decision,
                D::Approve | D::Once | D::Session | D::Persistent | D::Deny
            ),
            GateAnswer::Path { folder_offered, .. } => {
                matches!(decision, D::Approve | D::Once | D::Always | D::Deny)
                    || (folder_offered && matches!(decision, D::Folder))
            }
        }
    }

    fn respond(self, decision: protocol::ApprovalDecision) -> Result<(), ()> {
        use protocol::ApprovalDecision as D;
        match self.answer {
            GateAnswer::Action(answer) => answer
                .send(match decision {
                    D::Approve | D::Once => kernel::Approval::Once,
                    D::Always => kernel::Approval::Always,
                    _ => kernel::Approval::Deny,
                })
                .map_err(|_| ()),
            GateAnswer::Access(answer) => answer
                .send(match decision {
                    D::Approve | D::Once => kernel::NetworkDecision::Once,
                    D::Session => kernel::NetworkDecision::Session,
                    D::Persistent => kernel::NetworkDecision::Persistent,
                    _ => kernel::NetworkDecision::Deny,
                })
                .map_err(|_| ()),
            GateAnswer::Path { answer, .. } => answer
                .send(match decision {
                    D::Approve | D::Once => kernel::PathApproval::Once,
                    D::Always => kernel::PathApproval::Path,
                    D::Folder => kernel::PathApproval::Folder,
                    _ => kernel::PathApproval::Deny,
                })
                .map_err(|_| ()),
        }
    }
}

pub(crate) type Pending = Arc<Mutex<HashMap<u64, PendingGate>>>;

fn lock_pending(pending: &Pending) -> std::sync::MutexGuard<'_, HashMap<u64, PendingGate>> {
    pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Acknowledged mid-turn messages not yet read; the run loop, not the queue, routes them.
type Unread = Arc<Mutex<Vec<String>>>;

fn take_unread(unread: &Unread) -> Vec<String> {
    std::mem::take(
        &mut *unread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

/// Removes one acknowledged copy of `text`, if the desktop sent it.
fn settle_unread(unread: &Unread, text: &str) {
    let mut unread = unread
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(index) = unread.iter().position(|queued| queued == text) {
        unread.remove(index);
    }
}

fn admit_steer(unread: &Unread, handle: Option<&kernel::InterruptHandle>, content: &str) -> bool {
    let mut unread = unread
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if unread.len() >= 128
        || crate::chat_presentation::size(&content)
            > (2 * 1024 * 1024usize)
                .saturating_sub(unread.iter().map(crate::chat_presentation::size).sum())
    {
        return false;
    }
    // The sink may run immediately after send. Hold the ledger lock across
    // admission so it cannot settle text before the accepted copy is recorded.
    unread.push(content.to_owned());
    if handle.is_some_and(|handle| handle.steer(content)) {
        true
    } else {
        unread.pop();
        false
    }
}

fn validate_send_intent(
    params: &Value,
    running: bool,
    turn: u64,
    cancelling: bool,
) -> Result<(), &'static str> {
    let Some(intent) = params.get("intent") else {
        return Ok(());
    };
    match serde_json::from_value::<protocol::SendIntent>(intent.clone()) {
        Ok(protocol::SendIntent::Start) if !running => Ok(()),
        Ok(protocol::SendIntent::Steer { turn: target })
            if running && !cancelling && target == turn =>
        {
            Ok(())
        }
        Ok(_) => Err(
            "The turn changed or is stopping. Your message was not accepted; keep it and send again deliberately.",
        ),
        Err(_) => Err("A valid message intent is required."),
    }
}

pub(crate) struct Bridge {
    pub(crate) writer: Arc<Writer>,
    pub(crate) pending: Pending,
    pub(crate) questions: crate::chat_questions::Questions,
    pub(crate) control: Arc<TurnControl>,
    writer_task: WriterTask,
}

/// The backend can cancel the current turn without waiting for an unrelated
/// sign-in/screen operation in the RPC reader, or for a full input pipe.
#[derive(Default)]
pub(crate) struct TurnControl {
    active: Mutex<Option<ActiveTurn>>,
    gates: Mutex<Option<(Pending, crate::chat_questions::Questions)>>,
}

struct ActiveTurn {
    number: u64,
    interrupt: kernel::InterruptHandle,
    task: tokio::task::AbortHandle,
    aborted_at: Option<Instant>,
    slow_reported: bool,
}

impl TurnControl {
    pub(crate) fn cancel(&self) -> bool {
        self.cancel_for(None)
    }

    pub(crate) fn cancel_for(&self, number: Option<u64>) -> bool {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if number.is_some_and(|number| active.as_ref().is_none_or(|active| active.number != number))
        {
            return false;
        }
        let cancelled = active.as_ref().is_some_and(|handle| {
            handle.interrupt.cancel_turn();
            true
        });
        let gates = self
            .gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let denied = gates.is_some_and(|(pending, questions)| {
            crate::chat_questions::clear(&questions);
            deny_pending(&pending) > 0
        });
        drop(active);
        cancelled || denied
    }

    /// A force stop requests abortion, but does not release turn ownership.
    /// The serving loop joins the owner and repairs its transcript before
    /// publishing the settlement barrier to every viewer.
    pub(crate) fn abort(&self) -> bool {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(turn) = active.as_mut().filter(|turn| !turn.task.is_finished()) else {
            return false;
        };
        turn.interrupt.cancel_turn();
        turn.aborted_at.get_or_insert_with(Instant::now);
        turn.task.abort();
        drop(active);
        self.cancel();
        true
    }

    fn own(&self, number: u64, interrupt: kernel::InterruptHandle, task: tokio::task::AbortHandle) {
        *self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ActiveTurn {
            number,
            interrupt,
            task,
            aborted_at: None,
            slow_reported: false,
        });
    }

    fn finish(&self) -> bool {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .is_some_and(|turn| turn.aborted_at.is_some())
    }

    fn report_slow_abort(&self) -> bool {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.as_mut().is_some_and(|turn| {
            let slow = !turn.slow_reported
                && turn
                    .aborted_at
                    .is_some_and(|at| at.elapsed() >= TURN_ABORT_GRACE);
            turn.slow_reported |= slow;
            slow
        })
    }
}

pub(crate) fn bridge_to<W>(output: W) -> Bridge
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    bridge_with_output(output, OUTBOUND_FRAMES)
}

fn bridge_with_output<W>(output: W, capacity: usize) -> Bridge
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (writer, writer_task) = output_writer(output, capacity);
    Bridge {
        writer,
        pending: Arc::default(),
        questions: Arc::default(),
        control: Arc::default(),
        writer_task,
    }
}

pub(crate) fn output_writer<W>(output: W, capacity: usize) -> (Arc<Writer>, WriterTask)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(capacity.max(1));
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let cancelled = CancellationToken::new();
    let progress = Arc::new(Notify::new());
    let failure = Arc::new(Mutex::new(None));
    let handle = tokio::spawn(writer_loop(
        output,
        rx,
        Arc::clone(&queued_bytes),
        cancelled.clone(),
        Arc::clone(&progress),
        Arc::clone(&failure),
    ));
    (
        Arc::new(Writer {
            tx,
            queued_bytes,
            cancelled: cancelled.clone(),
            progress,
            failure,
            presentation: Mutex::default(),
            presentation_enabled: AtomicBool::new(false),
        }),
        WriterTask {
            handle: Some(handle),
            cancelled,
        },
    )
}

/// Human approval gate over JSON-RPC.
pub struct Gate {
    writer: Arc<Writer>,
    pending: Pending,
    next_id: AtomicU64,
    always: Mutex<HashSet<String>>,
}

impl Gate {
    pub(crate) fn new(writer: Arc<Writer>, pending: Pending) -> Self {
        Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            always: Mutex::new(HashSet::new()),
        }
    }
}

/// Removes a pending approval if its await is cancelled.
struct PendingGuard {
    pending: Pending,
    gate_id: u64,
    writer: Arc<Writer>,
    approved: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        lock_pending(&self.pending).remove(&self.gate_id);
        self.writer.notify_params(
            "approval.resolved",
            &protocol::ApprovalResolved {
                gate_id: self.gate_id,
                approved: self.approved,
            },
        );
    }
}

#[async_trait::async_trait]
impl kernel::HumanGate for Gate {
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
        lock_pending(&self.pending).insert(
            gate_id,
            PendingGate {
                answer: GateAnswer::Action(tx),
                escalated,
            },
        );
        let mut guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            gate_id,
            writer: Arc::clone(&self.writer),
            approved: false,
        };
        let sent = {
            let mut choices = vec![protocol::ApprovalDecision::Once];
            if !escalated {
                choices.push(protocol::ApprovalDecision::Always);
            }
            choices.push(protocol::ApprovalDecision::Deny);
            self.writer.notify_params(
                "approval",
                &protocol::ApprovalPrompt {
                    gate_id,
                    action: action.to_owned(),
                    detail: detail.map(str::to_owned),
                    escalated,
                    kind: protocol::ApprovalKind::Action,
                    choices,
                    folder: None,
                    path: None,
                },
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
        // The guard settles every viewer even if this future is cancelled.
        guard.approved = approval.approved();
        if approval == kernel::Approval::Always && !escalated {
            self.always
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(action.to_string());
        }
        approval
    }

    /// Every viewer receives the same path choices. The editor adapter maps
    /// these offered scopes to ACP permission options.
    async fn confirm_path(&self, request: kernel::PathRequest<'_>) -> kernel::PathApproval {
        let gate_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock_pending(&self.pending).insert(
            gate_id,
            PendingGate {
                answer: GateAnswer::Path {
                    answer: tx,
                    folder_offered: request.folder.is_some(),
                },
                escalated: false,
            },
        );
        let mut guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            gate_id,
            writer: Arc::clone(&self.writer),
            approved: false,
        };
        use protocol::ApprovalDecision as D;
        let folder_choice = request.folder.map(|_| D::Folder);
        let choices = [Some(D::Once), Some(D::Always), folder_choice, Some(D::Deny)];
        let prompt = protocol::ApprovalPrompt {
            gate_id,
            action: request.action.to_owned(),
            detail: request.detail.map(str::to_owned),
            escalated: false,
            kind: protocol::ApprovalKind::Path,
            choices: choices.into_iter().flatten().collect(),
            folder: request.folder.map(|folder| folder.display().to_string()),
            path: Some(protocol::ApprovalPath {
                path: request.path.display().to_string(),
                kind: match request.kind {
                    kernel::PathKind::File => protocol::PathKind::File,
                    kernel::PathKind::Directory => protocol::PathKind::Directory,
                    kernel::PathKind::Unknown => protocol::PathKind::Unknown,
                },
                access: match request.access {
                    kernel::PathAccess::Read => protocol::PathAccess::Read,
                    kernel::PathAccess::Write => protocol::PathAccess::Write,
                },
            }),
        };
        if !self.writer.notify_params("approval", &prompt) {
            return kernel::PathApproval::Deny;
        }
        // Disconnect or no response denies it, as for any other approval.
        let approval = tokio::select! {
            result = rx => result.unwrap_or(kernel::PathApproval::Deny),
            _ = self.writer.cancelled() => kernel::PathApproval::Deny,
        };
        guard.approved = approval != kernel::PathApproval::Deny;
        approval
    }

    async fn confirm_network(
        &self,
        detail: Option<&str>,
        escalated: bool,
    ) -> kernel::NetworkDecision {
        self.access("grant network access and retry", detail, escalated)
            .await
    }

    async fn confirm_access(
        &self,
        detail: Option<&str>,
        escalated: bool,
    ) -> kernel::NetworkDecision {
        self.access(
            &format!("command access: {}", detail.unwrap_or_default()),
            detail,
            escalated,
        )
        .await
    }
}

impl Gate {
    async fn access(
        &self,
        action: &str,
        detail: Option<&str>,
        escalated: bool,
    ) -> kernel::NetworkDecision {
        let gate_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        lock_pending(&self.pending).insert(
            gate_id,
            PendingGate {
                answer: GateAnswer::Access(answer),
                escalated,
            },
        );
        let mut guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            gate_id,
            writer: Arc::clone(&self.writer),
            approved: false,
        };
        let mut choices = vec![protocol::ApprovalDecision::Once];
        if !escalated {
            choices.extend([
                protocol::ApprovalDecision::Session,
                protocol::ApprovalDecision::Persistent,
            ]);
        }
        choices.push(protocol::ApprovalDecision::Deny);
        if !self.writer.notify_params(
            "approval",
            &protocol::ApprovalPrompt {
                gate_id,
                action: action.to_owned(),
                detail: detail.map(str::to_owned),
                escalated,
                kind: protocol::ApprovalKind::Access,
                choices,
                folder: None,
                path: None,
            },
        ) {
            return kernel::NetworkDecision::Deny;
        }
        let decision = tokio::select! {
            answer = answered => answer.unwrap_or(kernel::NetworkDecision::Deny),
            _ = self.writer.cancelled() => kernel::NetworkDecision::Deny,
        };
        guard.approved = decision != kernel::NetworkDecision::Deny;
        decision
    }
}

fn respond_gate(
    pending: &Pending,
    gate_id: u64,
    decision: protocol::ApprovalDecision,
) -> Result<(), &'static str> {
    let mut pending = lock_pending(pending);
    let gate = pending.get(&gate_id).ok_or("approval is not pending")?;
    if !gate.permits(decision) {
        return Err("that decision is not offered for this approval");
    }
    pending
        .remove(&gate_id)
        .expect("checked while holding the gate lock")
        .respond(decision)
        .map_err(|_| "approval is no longer waiting")
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
struct Sink {
    writer: Arc<Writer>,
    unread: Unread,
}

impl kernel::StreamSink for Sink {
    fn phase(&self, phase: kernel::progress::Phase) {
        if phase == kernel::progress::Phase::Generating {
            self.writer.turn_event(protocol::TurnEvent::Waiting);
        }
    }
    fn notice(&self, text: &str) {
        self.writer.turn_event(protocol::TurnEvent::Notice {
            text: text.to_owned(),
        });
    }
    fn text(&self, delta: &str) {
        self.writer.turn_event(protocol::TurnEvent::Text {
            delta: delta.to_owned(),
        });
    }
    fn reasoning(&self, delta: &str) {
        self.writer.turn_event(protocol::TurnEvent::Reasoning {
            delta: delta.to_owned(),
        });
    }
    fn tool_started(&self, tool: &str, target: Option<&str>) {
        self.writer.turn_event(protocol::TurnEvent::ToolStarted {
            tool: tool.to_owned(),
            target: target.map(str::to_owned),
        });
    }
    fn cost(&self, total_usd: f64, indicative: bool) {
        self.writer.turn_event(protocol::TurnEvent::Cost {
            total_usd,
            indicative,
        });
    }
    fn tool_call(&self, tool: &str, args: &Value) {
        self.writer.turn_event(protocol::TurnEvent::ToolCall {
            id: None,
            tool: tool.to_owned(),
            args: args.clone(),
        });
    }
    fn tool_call_with_id(&self, id: &str, tool: &str, args: &Value) {
        self.writer.turn_event(protocol::TurnEvent::ToolCall {
            id: Some(id.to_owned()),
            tool: tool.to_owned(),
            args: args.clone(),
        });
    }
    fn tool_result(&self, tool: &str, ok: bool, payload: &Value) {
        self.writer.turn_event(protocol::TurnEvent::ToolResult {
            id: None,
            tool: tool.to_owned(),
            ok,
            payload: payload.clone(),
        });
    }
    fn tool_result_with_id(&self, id: &str, tool: &str, ok: bool, payload: &Value) {
        self.writer.turn_event(protocol::TurnEvent::ToolResult {
            id: Some(id.to_owned()),
            tool: tool.to_owned(),
            ok,
            payload: payload.clone(),
        });
    }
    fn tool_input(&self, id: &str, tool: &str, delta: &str) {
        // Only a server's tool can have a screen to draw this on, and only
        // Medha's own window draws one. Everything else is told at the call.
        if mcp::McpManager::is_mcp_tool(tool) {
            self.writer.turn_event(protocol::TurnEvent::ToolInput {
                id: id.to_owned(),
                tool: tool.to_owned(),
                delta: delta.to_owned(),
            });
        }
    }
    fn tool_screen(&self, id: &str, screen: &Value) {
        // Only Medha's own window draws screens; another editor gets the text result.

        self.writer.turn_event(protocol::TurnEvent::ToolScreen {
            id: id.to_owned(),
            screen: screen.clone(),
        });
    }
    fn usage(&self, usage: &kernel::Usage) {
        self.writer.turn_event(protocol::TurnEvent::Usage {
            prompt_tokens: usage.prompt_tokens,
            total_tokens: usage.total_tokens,
            completion_tokens: Some(usage.completion_tokens),
            cached_prompt_tokens: usage.cached_prompt_tokens,
        });
    }
    fn context_pressure(&self, pressure: kernel::ContextPressure) {
        self.writer
            .turn_event(protocol::TurnEvent::ContextPressure {
                input_tokens: pressure.input_tokens,
                input_limit: pressure.input_limit,
                usable_input_tokens: pressure.usable_input_tokens,
                quality: match pressure.quality {
                    kernel::TokenCountQuality::Authoritative => {
                        protocol::CountQuality::Authoritative
                    }
                    kernel::TokenCountQuality::ProviderEstimate => {
                        protocol::CountQuality::ProviderEstimate
                    }
                    kernel::TokenCountQuality::LocalEstimate => {
                        protocol::CountQuality::LocalEstimate
                    }
                },
                percent: pressure.percent(),
            });
    }
    fn verify(&self, ok: bool, summary: &str) {
        self.writer.turn_event(protocol::TurnEvent::Verify {
            ok,
            summary: summary.to_owned(),
        });
    }
    fn compacting(&self, active: bool) {
        self.writer
            .turn_event(protocol::TurnEvent::Compacting { active });
    }
    fn compaction(&self, before: u32, after: u32, summarized: bool, summary: Option<&str>) {
        self.writer.turn_event(protocol::TurnEvent::Compaction {
            before,
            after,
            summarized,
            summary: summary.map(str::to_owned),
        });
    }
    fn steered(&self, text: &str) {
        settle_unread(&self.unread, text);
        self.writer.turn_event(protocol::TurnEvent::Steered {
            content: text.to_owned(),
        });
    }
    fn steers_returned(&self, texts: &[String]) {
        // Ledgered messages are routed by the run loop once the turn ends.
        let others: Vec<String> = {
            let unread = self
                .unread
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            texts
                .iter()
                .filter(|text| !unread.contains(text))
                .cloned()
                .collect()
        };
        if !others.is_empty() {
            self.writer
                .turn_event(protocol::TurnEvent::Returned { contents: others });
        }
    }
    fn restarted(&self) {
        self.writer.turn_event(protocol::TurnEvent::Restarted);
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
pub(crate) async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    stdin: &mut tokio::io::Take<BufReader<R>>,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<String>> {
    let n = stdin.read_until(b'\n', buf).await?;
    if n == 0 && buf.is_empty() {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') || n == 0 {
        let line = std::str::from_utf8(buf)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .to_owned();
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
    StartTurn {
        content: String,
        /// Admitted into the artifact store as the turn starts, so pixels never
        /// sit in the transcript or the log.
        images: Vec<IncomingImage>,
        admission_reply_to: Option<Value>,
    },
    /// Admission is acknowledged only after the owned queue accepts it.
    Steer {
        content: String,
        reply_to: Option<Value>,
    },
    Shutdown,
}

fn desktop_images(value: Option<&Value>) -> Result<Vec<IncomingImage>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .filter(|items| items.len() <= 4)
        .ok_or("Attach at most four images.")?;
    items
        .iter()
        .map(|image| {
            let mime = image["mime"]
                .as_str()
                .filter(|mime| {
                    matches!(
                        *mime,
                        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
                    )
                })
                .ok_or("Use PNG, JPEG, WebP, or GIF images.")?;
            let data = image["data"]
                .as_str()
                .filter(|data| !data.is_empty() && data.len() <= 3_000_000)
                .ok_or("Image data is missing or too large.")?;
            Ok(IncomingImage::Inline(mime.to_owned(), data.to_owned()))
        })
        .collect()
}

/// Decode, normalise and store what a frontend attached. Errors name the image
/// by position, because the editor sends bytes rather than a path.
async fn admit_images(
    images: Vec<IncomingImage>,
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
                    IncomingImage::Inline(_, data) => {
                        if data.len() > crate::attachments::MAX_SOURCE_BYTES.div_ceil(3) * 4 {
                            return Err(format!(
                                "image {position} exceeds the attachment size limit"
                            ));
                        }
                        base64::engine::general_purpose::STANDARD
                            .decode(data.as_bytes())
                            .map_err(|error| {
                                format!("image {position} is not valid base64: {error}")
                            })?
                    }
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

#[derive(Debug, PartialEq, Eq)]
enum IncomingImage {
    Inline(String, String),
}
impl IncomingImage {
    fn label(&self, position: usize) -> String {
        let Self::Inline(mime, _) = self;
        format!("image {position} ({mime})")
    }
}

fn turn_outcome(reason: &StopReason) -> protocol::TurnOutcome {
    match reason {
        StopReason::Finished => protocol::TurnOutcome::Finished,
        StopReason::Interrupted => protocol::TurnOutcome::Cancelled,
        StopReason::VerificationFailed | StopReason::Blocked => protocol::TurnOutcome::Refused,
        StopReason::Budget(kernel::BudgetStop::Tokens) => protocol::TurnOutcome::TokenLimit,
        StopReason::Budget(_) => protocol::TurnOutcome::RequestLimit,
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
        "hello" => {
            rpc_result(
                writer,
                &id,
                json!({ "proto": "1.0", "model": model, "caps": { "cards": ["approval", "diff"] } }),
            );
            RpcAction::None
        }
        "message.send" => {
            let images = match desktop_images(params.get("images")) {
                Ok(images) => images,
                Err(error) => {
                    rpc_error(writer, &id, -32602, error);
                    return RpcAction::None;
                }
            };
            if running && !images.is_empty() {
                rpc_error(
                    writer,
                    &id,
                    -32000,
                    "Send images after the current turn finishes.",
                );
                return RpcAction::None;
            }
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
                if interrupt.is_none() {
                    rpc_error(writer, &id, -32000, "a turn is already running");
                    return RpcAction::None;
                }
                RpcAction::Steer {
                    content,
                    reply_to: id,
                }
            } else {
                RpcAction::StartTurn {
                    content,
                    images,
                    admission_reply_to: id,
                }
            }
        }
        "approval.respond" => {
            let gate_id = params.get("gate_id").and_then(Value::as_u64);
            let decision = match (
                params.get("approve").and_then(Value::as_bool),
                params.get("decision"),
            ) {
                (Some(true), None) => Some(protocol::ApprovalDecision::Approve),
                (Some(false), None) => Some(protocol::ApprovalDecision::Deny),
                (None, Some(decision)) => serde_json::from_value(decision.clone()).ok(),
                _ => None,
            };
            let (Some(gate_id), Some(approval)) = (gate_id, decision) else {
                rpc_error(
                    writer,
                    &id,
                    -32602,
                    "gate_id and one valid approval decision are required",
                );
                return RpcAction::None;
            };
            match respond_gate(pending, gate_id, approval) {
                Ok(()) => rpc_result(writer, &id, json!({ "accepted": true })),
                Err(error) => rpc_error(writer, &id, -32001, error),
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

/// The desktop's name for why a run stopped; `None` when it simply finished.
fn stopped_label(reason: &StopReason) -> Option<&'static str> {
    match reason {
        StopReason::VerificationFailed => Some("verification_failed"),
        StopReason::Budget(stop) => Some(stop.label()),
        StopReason::Blocked => Some("blocked_by_hook"),
        StopReason::Finished | StopReason::Interrupted => None,
    }
}

/// Mid-turn messages steer at the next boundary; cancellation lets in-flight
/// tools settle through the kernel interrupt handle.
#[allow(clippy::too_many_arguments)]
pub async fn run<P, L, R>(
    kernel: Arc<Kernel<P, L>>,
    mut session: Session,
    system: String,
    mut model: String,
    base_budget: Budget,
    agent_budget: kernel::BudgetHandle,
    resumed: Vec<Message>,
    bridge: Bridge,
    agents: Option<Arc<orchestrator::AgentControl>>,
    model_config: Arc<Mutex<crate::config::Config>>,
    mut active_profile: String,
    extensions: crate::desktop_extensions::Runtime,
    input: R,
    restore: Option<Value>,
    notices: &dyn runtime::Notices,
) -> anyhow::Result<()>
where
    P: crate::desktop_controls::ProfileProvider + 'static,
    L: EventLog + 'static,
    R: tokio::io::AsyncRead + Unpin,
{
    let Bridge {
        writer,
        pending,
        questions,
        control,
        writer_task,
    } = bridge;
    *control
        .gates
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((Arc::clone(&pending), Arc::clone(&questions)));
    let (report_tx, mut report_rx) = mpsc::unbounded_channel();
    if let Some(control) = &agents
        && let Ok(mut slot) = control.notifier_handle().lock()
    {
        *slot = Some(Arc::new(move |owner| {
            let _ = report_tx.send(owner);
        }));
    }
    let mut roster = agents.clone().map(crate::chat_agents::Roster::new);
    let mut roster_tick = tokio::time::interval(ROSTER_INTERVAL);
    roster_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Connection changes are pushed to the window as they happen.
    let mut mcp_changes = extensions.mcp.as_ref().map(|manager| manager.subscribe());
    if let Some(saved) = restore {
        // Its start hooks already added their context to this conversation.
        if !resumed.is_empty() {
            kernel.continue_session(session.id);
        }
        for change in crate::desktop_controls::restore(&saved) {
            if let Some(Err(error)) = crate::desktop_controls::handle(
                "session.configure",
                &change,
                &kernel,
                &mut session,
                &mut model,
                &mut active_profile,
                &model_config,
                &extensions.model_env,
                false,
                agents.as_ref(),
            )
            .await
            {
                notices.say(&format!("note: could not restore {change}: {error}"));
            }
        }
    }
    writer.notify(
        "ready",
        json!({
            "proto": "1.0",
            "model": model,
            "session": session.id.to_string(),
            "caps": { "cards": ["approval", "diff"] },
        }),
    );
    // A server may already be ready before this chat subscribes. Publish its
    // initial snapshot after subscribing so startup and future changes are
    // both observable, regardless of which finishes first.
    if let Some(manager) = &extensions.mcp {
        writer.notify("mcp.status", json!({ "servers": manager.status().await }));
    }

    let mut transcript = crate::session_transcript(system, resumed);
    writer.reset_presentation(
        kernel.provider.as_ref(),
        &session,
        &transcript,
        &active_profile,
        &model,
        &model_config,
    );
    // Frame reads are capped: `lines()` would buffer a single unterminated
    // "line" without bound, so a runaway peer could grow memory indefinitely.
    let mut stdin = tokio::io::AsyncReadExt::take(BufReader::new(input), MAX_FRAME);
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut turns = JoinSet::new();
    let mut running = false;
    let mut interrupt: Option<kernel::InterruptHandle> = None;
    let unread = Unread::default();

    let mut reports_ready = true; // Also collect reports retained across restart.
    let mut turn_requested = false;
    let mut turn_number = 0u64;
    let mut history_head = 0u64;
    let mut history_marked = false;
    let mut history_reader = crate::chat_history::Reader::default();
    loop {
        if !running && (turn_requested || reports_ready) {
            reports_ready = false;
            let mut messages = transcript.clone();
            let taken = match &agents {
                Some(control) => {
                    crate::agents::collect_reports(
                        control,
                        session.id,
                        &kernel.artifacts,
                        &mut messages,
                    )
                    .await
                }
                None => Vec::new(),
            };
            if turn_requested || !taken.is_empty() {
                history_head = kernel
                    .log
                    .checked_history_record(session.id, 0, None, true)
                    .await?
                    .0;
                if !history_marked {
                    writer.mark_history();
                }
                history_marked = false;
                turn_requested = false;
                running = true;
                turn_number += 1;
                if writer.presents() {
                    writer.turn_event(protocol::TurnEvent::Started { turn: turn_number });
                }
                let (handle, queue) = kernel::InterruptQueue::pair();
                if let Some(control) = &agents {
                    control.attend(handle.clone());
                }
                interrupt = Some(handle);
                let kernel = kernel.clone();
                let session = session.clone();
                let budget = crate::task_budget(&base_budget, &agent_budget);
                let sink = Sink {
                    writer: writer.clone(),
                    unread: unread.clone(),
                };
                let agents = agents.clone();
                let owner = turns.spawn(async move {
                    match kernel
                        .run_session(&session, messages, budget, &sink, Some(queue))
                        .await
                    {
                        Ok((updated, reason)) => {
                            if let Some(control) = &agents {
                                control.settle(session.id, &taken).await;
                            }
                            TurnDone::Ok(updated, reason)
                        }
                        Err(error) => TurnDone::Err(error.to_string()),
                    }
                });
                control.own(
                    turn_number,
                    interrupt.as_ref().expect("a running turn").clone(),
                    owner,
                );
            }
        }
        tokio::select! {
            line = async {
                writer.wait_for_room().await;
                read_frame(&mut stdin, &mut frame_buf).await
            } => {
                let Ok(Some(line)) = line else { break }; // stdin closed / oversized frame → exit
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(request) = serde_json::from_str::<Value>(trimmed)
                    && request["jsonrpc"] == "2.0"
                    && let Some(method) = request["method"].as_str()
                {
                    let params = request.get("params").cloned().unwrap_or(Value::Null);
                    if method == "session.history.image" {
                        let Some(id) = request.get("id").cloned() else { continue; };
                        match serde_json::from_value::<protocol::ReadHistoryImage>(params) {
                            Ok(query) => match crate::chat_history::image(&kernel.artifacts, query).await {
                                Ok(image) => { writer.respond(id, json!(image)); }
                                Err(error) => { writer.error(id, -32001, error); }
                            },
                            Err(_) => { writer.error(id, -32602, "Invalid image request"); }
                        }
                        continue;
                    }
                    if method == "session.history" {
                        let Some(id) = request.get("id").cloned() else { continue; };
                        match serde_json::from_value::<protocol::ReadHistory>(params) {
                            Ok(mut query) => {
                                let first = query.through.is_none();
                                if first && (query.after != 0 || query.offset != 0) {
                                    writer.error(id, -32602, "Start history at its beginning"); continue;
                                }
                                if first && running { query.through = Some(history_head); query.conversation = Some(session.id.to_string()); }
                                match history_reader.read(kernel.log.as_ref(), &session, query, &kernel.artifacts).await {
                                    Ok(page) => { writer.history(id, page, first, running); }
                                    Err(error) => { writer.error(id, -32001, error); }
                                }
                            }
                            Err(_) => { writer.error(id, -32602, "Invalid history request"); }
                        }
                        continue;
                    }
                    if method == "message.send" && let Err(error) = validate_send_intent(&params, running, turn_number, interrupt.as_ref().is_some_and(|handle| handle.cancel_requested())) {
                        rpc_error(&writer, &request.get("id").cloned(), -32002, error);
                        continue;
                    }
                    if method == "session.presentation" {
                        if !writer.presents() {
                            rpc_error(&writer, &request.get("id").cloned(), -32601, "Presentation recovery is available on the application backend.");
                        } else if serde_json::from_value::<protocol::GetPresentation>(params).is_err() {
                            rpc_error(&writer, &request.get("id").cloned(), -32602, "Invalid presentation request.");
                        } else if let Some(id) = request.get("id") { writer.presentation(id.clone()); }
                        continue;
                    }
                    let result = if let Some(result) = crate::application_session::handle(&kernel, &session, &mut transcript, &extensions, method, params.clone(), running, &writer, agents.as_ref()).await {
                        Some(result)
                    } else if method == "question.respond" {
                        Some(crate::chat_questions::respond(&questions, &params))
                    } else if method == "session.rewind.points" {
                        Some(crate::desktop_rewind::points(&kernel, &session, extensions.workspace.root()).await)
                    } else if method == "session.rewind" {
                        Some(if running || agents.as_ref().is_some_and(|control| !control.active().is_empty()) || !kernel.executor.background_tasks().is_empty() { Err("Finish or stop active work before rewinding.".into()) } else { crate::desktop_rewind::rewind(&kernel, &mut session, &mut transcript, &extensions, agents.as_ref(), &params).await })
                    } else if method == "mcp.disconnect" {
                        Some(if running || agents.as_ref().is_some_and(|control| !control.active().is_empty()) || !kernel.executor.background_tasks().is_empty() { Err("Finish active work before disconnecting a server.".into()) } else { extensions.disconnect(&params).await })
                    } else if method == "mcp.connect" {
                        Some(if running || agents.as_ref().is_some_and(|control| !control.active().is_empty()) || !kernel.executor.background_tasks().is_empty() { Err("Finish active work before connecting a server.".into()) } else { extensions.connect(&params, &writer).await })
                    } else if method == "mcp.signin" {
                        Some(extensions.sign_in_again(&params, &writer).await)
                    } else if method == "connectors.connect" {
                        Some(if running || agents.as_ref().is_some_and(|control| !control.active().is_empty()) || !kernel.executor.background_tasks().is_empty() { Err("Finish active work before connecting a server.".into()) } else { extensions.connect_connector(&params, &writer).await })
                    } else if method == "mcp.screens" {
                        Some(crate::desktop_screens::offered(&extensions.mcp))
                    } else if method == "mcp.screen" {
                        Some(crate::desktop_screens::page(&extensions.mcp, &params).await)
                    } else if method == "mcp.screen.call" {
                        Some(crate::desktop_screens::call(&extensions.mcp, &params).await)
                    } else if method == "memory.list" {
                        Some(crate::desktop_memory::list(extensions.memory.as_ref()).await)
                    } else if method == "memory.pin" || method == "memory.forget" {
                        Some(crate::desktop_memory::change(kernel.log.as_ref(), &session, extensions.memory.as_ref(), method, &params).await)
                    } else if method == "memory.provenance" {
                        Some(crate::desktop_memory::provenance(kernel.log.as_ref(), extensions.memory.as_ref(), &params).await)
                    } else if method == "tasks.list" {
                        Some(Ok(json!({ "tasks": kernel.executor.background_tasks().iter().map(|task| json!({ "id": task.id, "command": task.command, "running": task.running })).collect::<Vec<_>>() })))
                    } else if method == "extensions.reload" {
                        Some(if running || agents.as_ref().is_some_and(|control| !control.active().is_empty()) || !kernel.executor.background_tasks().is_empty() { Err("Finish or stop active work before reloading extensions.".into()) } else { extensions.reload().await })
                    } else if method == "extensions.catalog" {
                        Some(Ok(extensions.catalog(&kernel).await))
                    } else if method == "session.sleep" {
                        let idle = !running && agents.as_ref().is_none_or(|control| control.active().is_empty() && control.cached_unmerged() == 0) && kernel.executor.background_tasks().is_empty();
                        // A service viewer sleeps by detaching, not by ending
                        // everybody's runtime. The router retires its viewer
                        // after this answer; the last-viewer policy owns exit.
                        if idle {
                            let resumable = !kernel.log.events(session.id).await.is_empty();
                            let settings = crate::desktop_controls::handle("session.settings", &params, &kernel, &mut session, &mut model, &mut active_profile, &model_config, &extensions.model_env, running, agents.as_ref()).await.and_then(Result::ok);
                            Some(Ok(json!({ "slept": true, "session": resumable.then(|| session.id.to_string()), "settings": settings })))
                        } else {
                            Some(Ok(json!({ "slept": false })))
                        }
                    } else {
                        crate::desktop_controls::handle(method, &params, &kernel, &mut session, &mut model, &mut active_profile, &model_config, &extensions.model_env, running, agents.as_ref()).await
                    };
                    if let Some(result) = result {
                        let id = request.get("id").cloned();
                        match result {
                            Ok(value) => {
                                if matches!(method, "session.settings" | "session.configure" | "session.profile.saved") { writer.notify("settings", value.clone()); }
                                if method == "session.rewind" {
                                    if value["code_only"] != true { writer.reset_presentation(kernel.provider.as_ref(), &session, &transcript, &active_profile, &model, &model_config); }
                                    writer.notify("session.rewound", json!({ "session": value["session"], "code_only": value["code_only"] })); roster = agents.clone().map(crate::chat_agents::Roster::new);
                                }
                                rpc_result(&writer, &id, value);
                            }
                            Err(error) => rpc_error(&writer, &id, -32001, error),
                        }
                        continue;
                    }
                    if method == "turn.cancel" {
                        match serde_json::from_value::<protocol::CancelTurn>(params) {
                            Ok(target) => rpc_result(&writer, &request.get("id").cloned(), json!({"cancelled":control.cancel_for(Some(target.turn))})),
                            Err(_) => rpc_error(&writer, &request.get("id").cloned(), -32602, "A valid turn number is required"),
                        }
                        continue;
                    }
                    if method == "turn.abort" {
                        rpc_result(&writer, &request.get("id").cloned(), json!({"accepted": control.abort()}));
                        continue;
                    }
                    if matches!(method, "cancel" | "interrupt" | "shutdown" | "exit") { crate::chat_questions::clear(&questions); }
                }
                match dispatch_line(trimmed, &model, running, interrupt.as_ref(), &pending, &writer) {
                    RpcAction::None => {}
                    RpcAction::Shutdown => break,
                    RpcAction::Steer { content, reply_to } => {
                        if !admit_steer(&unread, interrupt.as_ref(), &content) {
                            rpc_error(&writer, &reply_to, -32002, "The turn ended, is stopping, or its message queue is full. Your message was not accepted.");
                            continue;
                        }
                        writer.turn_event(protocol::TurnEvent::Queued { content: Some(content) });
                        rpc_result(&writer, &reply_to, json!({"accepted": true, "steered": true, "turn": turn_number}));
                    }
                    RpcAction::StartTurn { content, images, admission_reply_to } => {
                        let mut prompt = Message::user(content);
                        match admit_images(images, &kernel.artifacts).await {
                            Ok(attachments) => prompt.attachments = attachments,
                            Err(error) => {
                                // The frontend attached an image Medha cannot
                                // read. Answering the text alone would look
                                // like it was seen, so the turn does not start.
                                if let Some(id) = admission_reply_to.clone() {
                                    rpc_error(&writer, &Some(id), -32602, &error);
                                } else {
                                    writer.event("turn.error", json!({"message":error}));
                                }
                                continue;
                            }
                        }
                        transcript.push(prompt);
                        writer.mark_history();
                        history_marked = true;
                        if writer.presents() {
                            let prompt = transcript.last().expect("accepted prompt");
                            writer.turn_event(protocol::TurnEvent::User {
                                content:prompt.content.clone(), turn:turn_number.saturating_add(1), images:crate::chat_history::images(&prompt.attachments)
                            });
                        }
                        turn_requested = true;
                        if let Some(id) = admission_reply_to { rpc_result(&writer, &Some(id), json!({ "accepted": true, "steered": false, "turn": turn_number.saturating_add(1) })); }
                    }
                }
            }
            joined = turns.join_next(), if running => {
                running = false;
                interrupt = None;
                let force_aborted = control.finish();
                // No approval belongs past the turn that requested it. This
                // also releases a gate whose task ended with an error before
                // consuming its response.
                deny_pending(&pending);
                crate::chat_questions::clear(&questions);
                // Unread messages run next after a normal finish; after a stop they go back.
                let left = take_unread(&unread);
                let completed = !force_aborted && matches!(&joined, Some(Ok(TurnDone::Ok(_, reason))) if *reason != StopReason::Interrupted);
                let carried = if completed { left } else {
                    if !left.is_empty() { writer.event("message.returned", json!({ "contents": left })); }
                    Vec::new()
                };
                if force_aborted {
                    let (message, history) = crate::failed_turn_history(kernel.log.as_ref(), &session, &transcript, "force-stopped".into()).await;
                    if let Some(history) = history { transcript = history; writer.reset_presentation(kernel.provider.as_ref(), &session, &transcript, &active_profile, &model, &model_config); }
                    if message != "force-stopped" { writer.event("notice", json!({"text": message})); }
                    writer.event("turn.cancelled", json!({}));
                    writer.turn_event(protocol::TurnEvent::Settled { completion: protocol::TurnCompletion { turn: turn_number, outcome: protocol::TurnOutcome::Cancelled } });
                    writer.notify_params("event", &protocol::TurnEvent::AbortSettled);
                    continue;
                }
                match joined {
                    Some(Ok(TurnDone::Ok(updated, reason))) => {
                        transcript = updated;
                        writer.turn_event(protocol::TurnEvent::Settled { completion: protocol::TurnCompletion { turn: turn_number, outcome: turn_outcome(&reason) } });
                        if !carried.is_empty() {
                            writer.mark_history();
                            history_marked = true;
                            // The run continues, so the peer sees delivery, not a stop.
                            for content in &carried { writer.event("message.steered", json!({ "content": content })); }
                            // Why the previous run ended still matters, e.g. a failed check.
                            if let Some(stopped) = stopped_label(&reason) { writer.event("turn.continued", json!({ "stopped": stopped })); }
                        } else if reason == StopReason::Interrupted {
                            writer.event("turn.cancelled", json!({}));
                        } else {
                            writer.event("turn.done", json!({ "stopped": stopped_label(&reason) }));
                        }
                    }
                    Some(Ok(TurnDone::Err(e))) => {
                        let (e, history) = crate::failed_turn_history(kernel.log.as_ref(), &session, &transcript, e).await;
                        if let Some(history) = history { transcript = history; writer.reset_presentation(kernel.provider.as_ref(), &session, &transcript, &active_profile, &model, &model_config); }
                        writer.event("turn.error", json!({ "message": e }));
                        writer.turn_event(protocol::TurnEvent::Settled { completion: protocol::TurnCompletion { turn: turn_number, outcome: protocol::TurnOutcome::Failed { message: e } } });
                    }
                    Some(Err(error)) => {
                        let message = format!("turn task failed: {error}");
                        let (message, history) = crate::failed_turn_history(kernel.log.as_ref(), &session, &transcript, message).await;
                        if let Some(history) = history { transcript = history; writer.reset_presentation(kernel.provider.as_ref(), &session, &transcript, &active_profile, &model, &model_config); }
                        writer.event("turn.error", json!({ "message": message }));
                        writer.turn_event(protocol::TurnEvent::Settled { completion: protocol::TurnCompletion { turn: turn_number, outcome: protocol::TurnOutcome::Failed { message } } });
                    }
                    None => {
                        writer.event("turn.error", json!({ "message": "turn task disappeared" }));
                        writer.turn_event(protocol::TurnEvent::Settled { completion: protocol::TurnCompletion { turn: turn_number, outcome: protocol::TurnOutcome::Failed { message: "turn task disappeared".into() } } });
                    }
                }
                if !carried.is_empty() {
                    transcript.extend(carried.into_iter().map(Message::user));
                    turn_requested = true;
                }
            }
            Some(owner) = report_rx.recv(), if agents.is_some() => {
                if owner.is_none() || owner == Some(session.id) { reports_ready = true; }
            }
            _ = roster_tick.tick(), if roster.is_some() || running || history_reader.has_cached() => {
                history_reader.expire(Instant::now());
                if control.report_slow_abort() { writer.notify_params("event", &protocol::TurnEvent::AbortSlow); }
                if let Some(update) = roster.as_mut().and_then(crate::chat_agents::Roster::changed) {
                    writer.notify("agents", update);
                }
            }
            Ok(()) = async { mcp_changes.as_mut().expect("guarded").changed().await }, if mcp_changes.is_some() => {
                if let Some(manager) = &extensions.mcp {
                    writer.notify("mcp.status", json!({ "servers": manager.status().await }));
                }
            }
            _ = writer.cancelled() => break,
        }
    }

    settle_turn(&mut interrupt, &pending, &mut turns, TURN_SHUTDOWN_GRACE).await;
    control.finish();
    deny_pending(&pending);
    crate::chat_questions::clear(&questions);
    kernel
        .observe_hook(
            &session,
            kernel::HookPoint::SessionEnd,
            json!({ "source": "backend" }),
        )
        .await;
    writer_task.finish(&writer).await;
    if let Some(reason) = *writer
        .failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        anyhow::bail!(reason);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn framed_input_rejects_invalid_utf8_and_preserves_a_cancelled_partial_read() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut sender, receiver) = tokio::io::duplex(256);
        let mut reader = BufReader::new(receiver).take(MAX_FRAME);
        let mut partial = Vec::new();
        sender.write_all(b"{\"text\":\"part").await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                read_frame(&mut reader, &mut partial)
            )
            .await
            .is_err()
        );
        sender.write_all("ial 🪕\"}\n".as_bytes()).await.unwrap();
        assert_eq!(
            read_frame(&mut reader, &mut partial)
                .await
                .unwrap()
                .unwrap(),
            "{\"text\":\"partial 🪕\"}\n"
        );
        sender.write_all(b"{\"text\":\"\xff\"}\n").await.unwrap();
        assert_eq!(
            read_frame(&mut reader, &mut partial)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
    use kernel::{Approval, HumanGate, Role};
    use std::path::Path;
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
                progress: Arc::new(Notify::new()),
                failure: Arc::new(Mutex::new(None)),
                presentation: Mutex::default(),
                presentation_enabled: AtomicBool::new(false),
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
    fn read_steers_leave_the_ledger_and_unread_ones_wait_for_the_run_loop() {
        use kernel::StreamSink;
        let (writer, mut rx) = capture_writer(8);
        let unread = Unread::default();
        unread
            .lock()
            .unwrap()
            .extend(["same".to_string(), "same".into(), "late".into()]);
        let sink = Sink {
            writer,
            unread: unread.clone(),
        };
        sink.steered("same");
        sink.steers_returned(&["late".into(), "agent report".into()]);
        let frames = captured_values(&mut rx);
        assert_eq!(frames[0]["params"]["kind"], "message.steered");
        assert_eq!(frames[1]["params"]["contents"], json!(["agent report"]));
        assert_eq!(take_unread(&unread), ["same", "late"], "one copy was read");
    }

    #[test]
    fn desktop_waiting_and_retry_notices_are_emitted_without_extending_standard_acp() {
        use kernel::StreamSink;
        let (writer, mut rx) = capture_writer(8);
        let sink = Sink {
            writer,
            unread: Unread::default(),
        };
        sink.phase(kernel::progress::Phase::Generating);
        sink.notice("Model request failed. Retrying (1/3)…");
        let frames = captured_values(&mut rx);
        assert_eq!(frames[0]["params"]["kind"], "model.waiting");
        assert_eq!(frames[1]["params"]["kind"], "notice");
        assert!(
            frames[1]["params"]["text"]
                .as_str()
                .unwrap()
                .contains("Retrying")
        );
    }

    #[test]
    fn turn_callbacks_preserve_the_typed_stream_contract_and_tui_progress() {
        use kernel::StreamSink;
        let (writer, mut rx) = capture_writer(32);
        let sink = Sink {
            writer,
            unread: Unread::default(),
        };
        sink.phase(kernel::Phase::Generating);
        sink.notice("image described by the vision model");
        sink.text("answer");
        sink.reasoning("thinking");
        sink.tool_started("read", Some("file.rs"));
        sink.tool_call_with_id("call", "read", &json!({"path": "file.rs"}));
        sink.tool_result_with_id("call", "read", true, &json!({"text": "contents"}));
        sink.usage(&kernel::Usage {
            prompt_tokens: 12,
            total_tokens: 15,
            ..Default::default()
        });
        sink.cost(0.25, true);
        sink.context_pressure(kernel::ContextPressure::new(
            12,
            Some(100),
            kernel::TokenCountQuality::LocalEstimate,
        ));
        sink.verify(false, "check failed");
        sink.compacting(true);
        sink.compaction(100, 50, true, Some("summary"));
        sink.steered("next question");
        sink.steers_returned(&["unused text".to_owned()]);
        sink.restarted();

        let frames = captured_values(&mut rx);
        assert_eq!(frames.len(), 16);
        let decoded: Vec<protocol::TurnEvent> = frames
            .into_iter()
            .map(|frame| {
                assert_eq!(frame["jsonrpc"], "2.0");
                assert_eq!(frame["method"], "event");
                serde_json::from_value(frame["params"].clone()).unwrap()
            })
            .collect();
        assert!(
            !decoded
                .iter()
                .any(|event| matches!(event, protocol::TurnEvent::Unknown))
        );
        assert!(
            matches!(&decoded[4], protocol::TurnEvent::ToolStarted { tool, target: Some(target) } if tool == "read" && target == "file.rs")
        );
        assert!(
            matches!(&decoded[6], protocol::TurnEvent::ToolResult { id: Some(id), payload, .. } if id == "call" && payload["text"] == "contents")
        );
        assert!(matches!(
            decoded[7],
            protocol::TurnEvent::Usage {
                cached_prompt_tokens: None,
                ..
            }
        ));
        assert!(matches!(
            decoded[8],
            protocol::TurnEvent::Cost {
                total_usd: 0.25,
                indicative: true
            }
        ));
    }

    #[test]
    fn only_a_plain_finish_or_a_stop_carries_no_desktop_reason() {
        assert_eq!(stopped_label(&StopReason::Finished), None);
        assert_eq!(stopped_label(&StopReason::Interrupted), None);
        assert_eq!(
            stopped_label(&StopReason::VerificationFailed),
            Some("verification_failed")
        );
        assert_eq!(stopped_label(&StopReason::Blocked), Some("blocked_by_hook"));
        assert_eq!(
            stopped_label(&StopReason::Budget(kernel::BudgetStop::Tokens)),
            Some(kernel::BudgetStop::Tokens.label())
        );
    }

    #[test]
    fn a_legacy_initialize_does_not_switch_dialects() {
        let (writer, mut rx) = capture_writer(8);
        dispatch_rpc(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
            "m",
            false,
            None,
            &Arc::new(Mutex::new(HashMap::new())),
            &writer,
        );
        assert_eq!(captured_values(&mut rx)[0]["error"]["code"], -32601);
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
        assert!(
            matches!(action, RpcAction::StartTurn { admission_reply_to: Some(id), .. } if id == json!(3))
        );
        assert!(
            values.is_empty(),
            "admission must wait until the prompt is retained"
        );

        let (steer, _queue) = kernel::InterruptQueue::pair();
        let (action, values) = request(
            json!({"jsonrpc": "2.0", "id": 4, "method": "message.send", "params": {"content": "change course"}}),
            true,
            Some(&steer),
            &empty_pending(),
        );
        assert!(matches!(action, RpcAction::Steer { reply_to: Some(id), .. } if id == json!(4)));
        assert!(
            values.is_empty(),
            "admission must wait for the owned interrupt queue"
        );

        let pending = empty_pending();
        let (approval_tx, mut approval_rx) = oneshot::channel();
        lock_pending(&pending).insert(41, approval_tx.into());
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
            lock_pending(&pending).insert(id, approval_tx.into());
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
            lock_pending(&pending).insert(id, approval_tx.into());
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
    fn steer_admission_never_acknowledges_a_closed_or_full_queue() {
        let unread = Unread::default();
        let (handle, mut queue) = kernel::InterruptQueue::pair();
        for _ in 0..128 {
            assert!(admit_steer(&unread, Some(&handle), "same"));
        }
        assert!(!admit_steer(&unread, Some(&handle), "refused"));
        assert_eq!(take_unread(&unread).len(), 128);
        assert_eq!(queue.drain_steers().len(), 128);
        drop(queue);
        assert!(!admit_steer(&unread, Some(&handle), "after end"));
        assert!(take_unread(&unread).is_empty());
    }

    #[test]
    fn a_delayed_steer_cannot_start_a_new_turn_or_cross_a_cancel_boundary() {
        let intent = json!({"intent": {"kind": "steer", "turn": 7}});
        assert!(validate_send_intent(&intent, true, 7, false).is_ok());
        assert!(validate_send_intent(&intent, false, 7, false).is_err());
        assert!(validate_send_intent(&intent, true, 8, false).is_err());
        assert!(validate_send_intent(&intent, true, 7, true).is_err());
        assert!(
            validate_send_intent(&json!({"intent": {"kind": "start"}}), true, 7, false).is_err()
        );
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
        lock_pending(&pending).insert(7, approval_tx.into());
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
        let gate = Arc::new(Gate::new(Arc::clone(&writer), Arc::clone(&pending)));

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
        let settled = captured_values(&mut rx);
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0]["method"], "approval.resolved");
        assert_eq!(settled[0]["params"]["approved"], false);

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
    async fn command_access_preserves_every_tui_grant_tier_and_settles_all_viewers() {
        use kernel::NetworkDecision as N;
        let (writer, mut output) = capture_writer(32);
        let pending: Pending = Arc::default();
        let gate = Arc::new(Gate::new(Arc::clone(&writer), Arc::clone(&pending)));
        for (choice, expected) in [
            ("once", N::Once),
            ("session", N::Session),
            ("persistent", N::Persistent),
            ("deny", N::Deny),
        ] {
            let asking = Arc::clone(&gate);
            let asked =
                tokio::spawn(
                    async move { asking.confirm_access(Some("read /reviewed"), false).await },
                );
            let frame = tokio::time::timeout(Duration::from_secs(1), output.recv())
                .await
                .unwrap()
                .unwrap();
            let Outbound::Frame(frame) = frame else {
                panic!("expected an approval")
            };
            let frame: Value = serde_json::from_slice(&frame).unwrap();
            let prompt: protocol::ApprovalPrompt =
                serde_json::from_value(frame["params"].clone()).unwrap();
            assert!(matches!(prompt.kind, protocol::ApprovalKind::Access));
            assert_eq!(
                frame["params"]["choices"],
                json!(["once", "session", "persistent", "deny"])
            );
            dispatch_rpc(
                json!({"jsonrpc": "2.0", "id": 90, "method": "approval.respond",
                "params": {"gate_id": prompt.gate_id, "decision": choice}}),
                "model",
                true,
                None,
                &pending,
                &writer,
            );
            assert_eq!(asked.await.unwrap(), expected);
            let replies = captured_values(&mut output);
            assert!(
                replies
                    .iter()
                    .any(|reply| reply["id"] == 90 && reply["result"]["accepted"] == true)
            );
            let settled: Vec<_> = replies
                .iter()
                .filter(|reply| reply["method"] == "approval.resolved")
                .collect();
            assert_eq!(settled.len(), 1);
            assert_eq!(settled[0]["params"]["gate_id"], prompt.gate_id);
            assert_eq!(settled[0]["params"]["approved"], expected != N::Deny);
            assert!(lock_pending(&pending).is_empty());
        }
    }

    /// Remembering a folder is asked as an answer of its own, with the folder
    /// named, and is given only when that is the answer that came back.
    #[tokio::test]
    async fn a_path_is_asked_about_with_its_folder_as_a_choice_of_its_own() {
        use kernel::PathApproval as P;
        let (writer, mut output) = capture_writer(32);
        let pending: Pending = Arc::default();
        let gate = Arc::new(Gate::new(Arc::clone(&writer), Arc::clone(&pending)));
        let folder = Path::new("/work/proj/src");
        for (offered, choice, expected) in [
            (Some(folder), "once", P::Once),
            (Some(folder), "always", P::Path),
            (Some(folder), "folder", P::Folder),
            (Some(folder), "deny", P::Deny),
            (None, "always", P::Path),
        ] {
            let asking = Arc::clone(&gate);
            let asked = tokio::spawn(async move {
                let action = "Read access to /work/proj/src/a.txt";
                asking
                    .confirm_path(kernel::PathRequest {
                        action,
                        detail: None,
                        path: Path::new("/work/proj/src/a.txt"),
                        kind: kernel::PathKind::File,
                        access: kernel::PathAccess::Read,
                        folder: offered,
                    })
                    .await
            });
            let frame = tokio::time::timeout(Duration::from_secs(1), output.recv())
                .await
                .unwrap()
                .unwrap();
            let Outbound::Frame(frame) = frame else {
                panic!("expected an approval")
            };
            let frame: Value = serde_json::from_slice(&frame).unwrap();
            let prompt: protocol::ApprovalPrompt =
                serde_json::from_value(frame["params"].clone()).unwrap();
            assert!(matches!(prompt.kind, protocol::ApprovalKind::Path));
            let choices = match offered {
                Some(_) => json!(["once", "always", "folder", "deny"]),
                None => json!(["once", "always", "deny"]),
            };
            assert_eq!(frame["params"]["choices"], choices);
            assert_eq!(frame["params"]["path"]["kind"], "file");
            assert_eq!(prompt.folder.as_deref(), offered.and_then(Path::to_str));
            if offered.is_none() {
                assert!(
                    respond_gate(&pending, prompt.gate_id, protocol::ApprovalDecision::Folder)
                        .is_err()
                );
                assert_eq!(
                    lock_pending(&pending).len(),
                    1,
                    "invalid answer consumed the prompt"
                );
                assert!(
                    !asked.is_finished(),
                    "an unoffered answer must not authorize access"
                );
            }
            dispatch_rpc(
                json!({"jsonrpc": "2.0", "id": 91, "method": "approval.respond",
                "params": {"gate_id": prompt.gate_id, "decision": choice}}),
                "model",
                true,
                None,
                &pending,
                &writer,
            );
            assert_eq!(asked.await.unwrap(), expected);
            captured_values(&mut output);
        }
    }

    #[tokio::test]
    async fn escalated_access_refuses_remembering_without_consuming_the_prompt() {
        let (writer, mut output) = capture_writer(16);
        let pending: Pending = Arc::default();
        let gate = Arc::new(Gate::new(Arc::clone(&writer), Arc::clone(&pending)));
        let asked =
            tokio::spawn(async move { gate.confirm_access(Some("outside sandbox"), true).await });
        let frame = output.recv().await.unwrap();
        let Outbound::Frame(frame) = frame else {
            panic!("expected an approval")
        };
        let frame: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(frame["params"]["choices"], json!(["once", "deny"]));
        let id = frame["params"]["gate_id"].as_u64().unwrap();
        for decision in [
            protocol::ApprovalDecision::Session,
            protocol::ApprovalDecision::Persistent,
            protocol::ApprovalDecision::Always,
        ] {
            assert!(respond_gate(&pending, id, decision).is_err());
            assert_eq!(lock_pending(&pending).len(), 1);
        }
        respond_gate(&pending, id, protocol::ApprovalDecision::Once).unwrap();
        assert_eq!(asked.await.unwrap(), kernel::NetworkDecision::Once);
        assert!(lock_pending(&pending).is_empty());
        assert_eq!(
            captured_values(&mut output)
                .iter()
                .filter(|frame| frame["method"] == "approval.resolved")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancelling_a_question_settles_the_form_once_and_releases_its_response() {
        let (writer, mut output) = capture_writer(8);
        let pending: crate::chat_questions::Questions = Arc::default();
        let asker = crate::chat_questions::Asker {
            writer,
            pending: Arc::clone(&pending),
            next_id: AtomicU64::new(1),
        };
        let asked = tokio::spawn(async move { kernel::Asker::ask(&asker, vec![]).await });
        let prompt = output.recv().await.unwrap();
        let Outbound::Frame(prompt) = prompt else {
            panic!("expected a question")
        };
        let prompt: Value = serde_json::from_slice(&prompt).unwrap();
        assert_eq!(prompt["method"], "question");
        asked.abort();
        let _ = asked.await;
        assert!(pending.lock().unwrap().is_empty());
        let settled = captured_values(&mut output);
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0]["method"], "question.answered");
        assert_eq!(
            settled[0]["params"]["question_id"],
            prompt["params"]["question_id"]
        );
    }

    #[tokio::test]
    async fn protocol_error_and_shutdown_drain_pending_approvals() {
        let (writer, mut rx) = capture_writer(8);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (malformed_tx, mut malformed_rx) = oneshot::channel();
        lock_pending(&pending).insert(1, malformed_tx.into());
        dispatch_line("{broken", "model", true, None, &pending, &writer);
        assert_eq!(malformed_rx.try_recv(), Ok(Approval::Deny));
        assert!(lock_pending(&pending).is_empty());
        captured_values(&mut rx);

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        lock_pending(&pending).insert(2, shutdown_tx.into());
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
        assert_eq!(shutdown_rx.try_recv(), Ok(Approval::Deny));
        assert!(lock_pending(&pending).is_empty());
    }

    #[test]
    fn turn_completion_denies_and_clears_pending_approvals() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (first_tx, mut first_rx) = oneshot::channel();
        let (second_tx, mut second_rx) = oneshot::channel();
        lock_pending(&pending).insert(1, first_tx.into());
        lock_pending(&pending).insert(2, second_tx.into());

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
        lock_pending(&pending).insert(99, approval_tx.into());
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

    #[tokio::test]
    async fn a_late_cancel_cannot_stop_or_deny_the_next_turn() {
        let control = TurnControl::default();
        let (handle, queue) = kernel::InterruptQueue::pair();
        let task = tokio::spawn(std::future::pending::<()>());
        control.own(2, handle, task.abort_handle());
        let pending: Pending = Arc::default();
        let (answer, mut answered) = oneshot::channel();
        lock_pending(&pending).insert(5, answer.into());
        *control.gates.lock().unwrap() = Some((pending.clone(), Default::default()));
        assert!(!control.cancel_for(Some(1)));
        assert!(!queue.token().is_cancelled());
        assert!(answered.try_recv().is_err());
        assert_eq!(lock_pending(&pending).len(), 1);
        assert!(control.cancel_for(Some(2)));
        assert!(queue.token().is_cancelled());
        assert_eq!(answered.await.unwrap(), Approval::Deny);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn backend_control_cancels_the_turn_and_denies_its_gate_without_rpc_input() {
        let control = TurnControl::default();
        let (handle, queue) = kernel::InterruptQueue::pair();
        let owned = tokio::spawn(std::future::pending::<()>());
        control.own(1, handle, owned.abort_handle());
        let pending: Pending = Arc::default();
        let (answer, answered) = oneshot::channel();
        lock_pending(&pending).insert(5, answer.into());
        *control.gates.lock().unwrap() = Some((pending.clone(), Default::default()));
        assert!(control.cancel());
        assert!(queue.token().is_cancelled());
        assert_eq!(answered.await.unwrap(), Approval::Deny);
        assert!(lock_pending(&pending).is_empty());
        control.finish();
        owned.abort();
        let _ = owned.await;
        assert!(!control.cancel());
    }

    #[tokio::test]
    async fn force_abort_retains_ownership_until_the_owner_is_joined() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let control = TurnControl::default();
        let (handle, queue) = kernel::InterruptQueue::pair();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let (ready, waiting) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = guard;
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        waiting.await.unwrap();
        control.own(1, handle, task.abort_handle());
        assert!(control.abort());
        assert!(queue.token().is_cancelled());
        assert!(control.active.lock().unwrap().is_some());
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(dropped.load(Ordering::SeqCst));
        assert!(control.finish());
        assert!(!control.finish());
        assert!(!control.abort());
    }

    #[test]
    fn child_streams_keep_parent_identity_tool_ids_and_returned_text() {
        use runtime::agents::AgentWatcher;
        let (writer, mut output) = capture_writer(32);
        let watch = crate::chat_agents::Watch { writer };
        let session = ulid::Ulid::new();
        let path = orchestrator::AgentPath::root().child("reviewer").unwrap();
        for step in [
            protocol::AgentStep::Task {
                objective: "review".into(),
                contract: Some("read-only".into()),
            },
            protocol::AgentStep::Reasoning("inspect".into()),
            protocol::AgentStep::Text("found".into()),
            protocol::AgentStep::ToolCall {
                id: Some("call-7".into()),
                tool: "read".into(),
                args: json!({"path": "src"}),
            },
            protocol::AgentStep::ToolResult {
                id: Some("call-7".into()),
                tool: "read".into(),
                ok: false,
                payload: json!({"error": "missing"}),
            },
            protocol::AgentStep::Restarted,
            protocol::AgentStep::SteerQueued("check".into()),
            protocol::AgentStep::Steered("check".into()),
            protocol::AgentStep::SteersReturned(vec!["keep me".into()]),
        ] {
            watch.step(Some(session), &path, step);
        }
        watch.usage(&kernel::Usage {
            prompt_tokens: 11,
            total_tokens: 23,
            cached_prompt_tokens: None,
            ..kernel::Usage::default()
        });
        let frames = captured_values(&mut output);
        assert_eq!(frames.len(), 10);
        for frame in &frames[..9] {
            assert_eq!(frame["method"], "agent.step");
            let event: protocol::AgentEvent =
                serde_json::from_value(frame["params"].clone()).unwrap();
            assert_eq!(event.surface_session, Some(session.to_string()));
            assert_eq!(event.path, path.as_str());
        }
        assert_eq!(frames[4]["params"]["step"]["data"]["id"], "call-7");
        assert_eq!(frames[8]["params"]["step"]["data"], json!(["keep me"]));
        assert_eq!(frames[9]["params"]["total_tokens"], 23);
    }

    #[test]
    fn oversized_result_and_error_replies_are_bounded_and_keep_their_id() {
        let (writer, mut rx) = capture_writer(4);
        let huge = "x".repeat(MAX_OUTBOUND_FRAME);
        assert!(writer.respond(json!(7), json!(huge)));
        assert!(writer.error(json!(8), -32000, huge));
        let values = captured_values(&mut rx);
        assert_eq!(
            values
                .iter()
                .map(|value| value["id"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [7, 8]
        );
        assert!(values.iter().all(|value| {
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("too large")
        }));
        assert!(!writer.cancelled.is_cancelled());
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
            questions: _,
            writer_task: blocked_task,
            control: _,
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
            questions: _,
            writer_task: healthy_task,
            control: _,
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
            questions: _,
            writer_task,
            control: _,
        } = bridge_with_output(BrokenOutput, 2);
        assert!(writer.notify("event", json!({"delta": "x"})));
        tokio::time::timeout(Duration::from_millis(250), writer.cancelled())
            .await
            .expect("broken pipe did not cancel connection");
        writer_task.finish(&writer).await;
    }
}
