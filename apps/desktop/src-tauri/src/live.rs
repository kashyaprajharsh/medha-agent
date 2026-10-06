//! Live sessions: each is a chat in the Medha backend, driven by the same
//! kernel, config and credentials as the TUI. Frames are forwarded to the
//! window as `medha-live` events, with raw tool arguments and output reshaped by
//! `transcript-view` exactly as the read-only history reshapes them.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, Once, Weak};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use transcript_view::Steps;

use crate::backend::{Backend, Said};
use crate::sleep::{self, Rest, Settled, Wake};
#[path = "request_queue.rs"]
mod request_queue;

const REQUESTS: [&str; 25] = [
    "mcp.signin",
    "connectors.connect",
    "tasks.list",
    "memory.list",
    "memory.pin",
    "memory.forget",
    "memory.provenance",
    "message.send",
    "approval.respond",
    "cancel",
    "session.settings",
    "session.configure",
    "agent.control",
    "patch.list",
    "patch.apply",
    "question.respond",
    "extensions.reload",
    "extensions.catalog",
    "mcp.screens",
    "mcp.screen",
    "mcp.screen.call",
    "mcp.connect",
    "mcp.disconnect",
    "session.rewind",
    "session.rewind.points",
];
const STOPPED: &str = "This session has stopped. Start a new session to continue.";
const STREAM_INTERVAL: Duration = Duration::from_millis(32);
const STREAM_BYTES: usize = 64 * 1024;
/// What may wait to be drawn for one chat, and to be sent to one. Past this the
/// window stops following the chat, or refuses the request, and says so.
const INBOX_FRAMES: usize = 8192;
const INBOX_BYTES: usize = 32 * 1024 * 1024;
const OUTBOX_FRAMES: usize = 256;
const OUTBOX_BYTES: usize = 2 * wire::MAX_FRAME;
const FELL_BEHIND: &str = "This window could not keep up with the chat and stopped following it. Open the chat again to continue.";
const WAITING: &str = "This chat has too much waiting to be sent. Try again in a moment.";

#[derive(Default)]
struct StreamFrames {
    pending: Option<Value>,
    deadline: Option<Instant>,
}

impl StreamFrames {
    fn flush(&mut self) -> Option<Value> {
        self.deadline = None;
        self.pending.take()
    }

    fn push(&mut self, frame: Value, now: Instant) -> Vec<Value> {
        let mut ready = Vec::new();
        if self.deadline.is_some_and(|deadline| now >= deadline)
            && let Some(pending) = self.flush()
        {
            ready.push(pending);
        }
        let streaming = frame["method"] == "event"
            && matches!(
                frame["params"]["kind"].as_str(),
                // A tool call's arguments arrive in the same rapid pieces as text does.
                Some("model.text" | "model.reasoning" | "tool.input")
            )
            && frame["params"]["delta"].is_string();
        if !streaming {
            if let Some(pending) = self.flush() {
                ready.push(pending);
            }
            ready.push(frame);
            return ready;
        }
        // Merge only equivalent notifications. Keep metadata and all control
        // boundaries intact, including retry, tool calls and turn completion.
        let compatible = self.pending.as_ref().is_some_and(|pending| {
            let left = pending.as_object().unwrap();
            let right = frame.as_object().unwrap();
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    if key != "params" {
                        return right.get(key) == Some(value);
                    }
                    let left = value.as_object().unwrap();
                    let right = frame["params"].as_object().unwrap();
                    left.len() == right.len()
                        && left
                            .iter()
                            .all(|(key, value)| key == "delta" || right.get(key) == Some(value))
                })
        });
        if compatible {
            let pending = self.pending.as_mut().unwrap();
            if let Value::String(delta) = &mut pending["params"]["delta"] {
                delta.push_str(frame["params"]["delta"].as_str().unwrap());
            }
        } else {
            if let Some(pending) = self.flush() {
                ready.push(pending);
            }
            self.pending = Some(frame);
            self.deadline = Some(now + STREAM_INTERVAL);
        }
        if self.pending.as_ref().is_some_and(|pending| {
            pending["params"]["delta"].as_str().unwrap().len() >= STREAM_BYTES
        }) && let Some(pending) = self.flush()
        {
            ready.push(pending);
        }
        ready
    }
}

fn pump_stream(frames: impl Incoming, mut emit: impl FnMut(Value)) {
    let mut stream = StreamFrames::default();
    loop {
        let within = stream
            .deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let received = frames.next(within);
        match received {
            Ok(frame) => {
                for frame in stream.push(frame, Instant::now()) {
                    emit(frame);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(frame) = stream.flush() {
                    emit(frame);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some(frame) = stream.flush() {
        emit(frame);
    }
}

struct Live {
    /// What the chat is asked, in the order it was asked, each with its size as
    /// it will leave, and no more than a chat may have waiting.
    requests: request_queue::Requests,
    ended: Arc<AtomicBool>,
    /// Set when this is dropped, which closes the chat.
    closed: Arc<AtomicBool>,
    next_id: u64,
    emit: Emit,
    rest: Arc<Rest>,
}

type Emit = Arc<dyn Fn(Value) + Send + Sync>;

type Open = HashMap<String, Live>;

static EVERY: Mutex<Vec<Weak<Inner>>> = Mutex::new(Vec::new());
static SWEEPER: Once = Once::new();

pub struct LiveSessions {
    inner: Arc<Inner>,
}

struct Inner {
    workspace: PathBuf,
    open: Mutex<Open>,
    backend: Arc<Backend>,
}

impl LiveSessions {
    pub fn new(workspace: PathBuf) -> Self {
        Self::on(workspace, Backend::shared())
    }

    fn on(workspace: PathBuf, backend: Arc<Backend>) -> Self {
        let inner = Arc::new(Inner {
            workspace,
            open: Mutex::new(HashMap::new()),
            backend,
        });
        if let Ok(mut every) = EVERY.lock() {
            every.push(Arc::downgrade(&inner));
        }
        SWEEPER.call_once(|| {
            std::thread::spawn(sweep_forever);
        });
        Self { inner }
    }

    pub fn open(&self, app: &AppHandle, key: &str, resume: Option<&str>) -> Result<(), String> {
        self.open_to(forward(app, key), key, resume)
    }

    fn open_to(&self, emit: Emit, key: &str, resume: Option<&str>) -> Result<(), String> {
        if !is_token(key) {
            return Err("invalid live session key".into());
        }
        if let Some(id) = resume
            && !is_ulid(id)
        {
            return Err("invalid session id".into());
        }
        let mut open = self.inner.awake(key)?;
        if open.get(key).is_some_and(|live| !live.has_ended()) {
            return Ok(());
        }
        let live = self.inner.spawn(&emit, resume, None);
        open.insert(key.to_owned(), live);
        Ok(())
    }

    pub fn request(&self, key: &str, method: &str, params: Value) -> Result<u64, String> {
        if !REQUESTS.contains(&method) {
            return Err(format!("{method} is not a desktop request"));
        }
        let mut open = self.inner.awake(key)?;
        let live = open.get_mut(key).ok_or("this session is not live")?;
        if live.has_ended() {
            return Err(STOPPED.into());
        }
        let id = live.next_id;
        live.next_id += 1;
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        // Refused here, where whoever asked is told, and not after it has been queued.
        let bytes = crate::backend::size(&frame) + crate::backend::WRAPPER;
        if bytes > wire::MAX_FRAME {
            return Err(crate::backend::TOO_LARGE.into());
        }
        let refused = match live.requests.try_send((bytes, frame)) {
            Ok(()) => return Ok(id),
            Err(TrySendError::Disconnected(_)) => STOPPED,
            Err(TrySendError::Full(_)) => WAITING,
        };
        Err(refused.into())
    }

    pub fn close(&self, key: &str) -> Result<(), String> {
        let removed = self
            .inner
            .open
            .lock()
            .map_err(|error| error.to_string())?
            .remove(key);
        drop(removed);
        Ok(())
    }
}

fn every() -> Vec<Arc<Inner>> {
    EVERY
        .lock()
        .map(|mut every| {
            every.retain(|inner| inner.strong_count() > 0);
            every.iter().filter_map(Weak::upgrade).collect()
        })
        .unwrap_or_default()
}

/// Only a chat that was opened is looked for, so focusing one never creates anything.
pub fn wake(key: &str) {
    let key = key.to_owned();
    std::thread::spawn(move || {
        for inner in every() {
            let opened = inner.open.lock().is_ok_and(|open| open.contains_key(&key));
            if opened {
                drop(inner.awake(&key));
            }
        }
    });
}

fn sweep_forever() {
    loop {
        std::thread::sleep(sleep::SWEEP);
        for inner in every() {
            inner.sweep(Instant::now());
        }
    }
}

impl Inner {
    fn sweep(&self, now: Instant) {
        let Ok(mut open) = self.open.lock() else {
            return;
        };
        for (key, live) in open.iter_mut() {
            if sleep::is_focused(key) || live.has_ended() {
                continue;
            }
            if let Some(ask) = live.rest.ask(now, sleep::GRACE)
                && live.requests.try_send((0, ask)).is_err()
            {
                live.rest.closed();
            }
        }
    }

    /// Decides under the same lock the sweep asks under, so a sleep question
    /// is either waited out here or written after the caller's frame.
    fn awake(&self, key: &str) -> Result<MutexGuard<'_, Open>, String> {
        loop {
            let mut open = self.open.lock().map_err(|error| error.to_string())?;
            let Some(live) = open.get(key) else {
                return Ok(open);
            };
            let Some(now) = live.rest.current() else {
                let rest = Arc::clone(&live.rest);
                drop(open);
                rest.settle()?;
                continue;
            };
            let Settled::Asleep(wake) = now else {
                live.rest.touch();
                return Ok(open);
            };
            let (emit, next_id) = (Arc::clone(&live.emit), live.next_id);
            let mut woken = self.spawn(&emit, wake.resume.as_deref(), Some(&wake));
            // Ids keep counting, so no answer is matched to an earlier request.
            woken.next_id = next_id;
            open.insert(key.to_owned(), woken);
            return Ok(open);
        }
    }

    /// Returns at once: the chat opens on a thread of its own, and what it is
    /// asked meanwhile waits in order. A chat that cannot open says so as its exit.
    fn spawn(&self, emit: &Emit, resume: Option<&str>, wake: Option<&Wake>) -> Live {
        let rest = Rest::new();
        let ended = Arc::new(AtomicBool::new(false));
        let (behind, closed) = (Arc::<AtomicBool>::default(), Arc::<AtomicBool>::default());
        let why = Arc::new(Mutex::new(String::new()));
        let (sender, frames) = sync_channel(INBOX_FRAMES);
        let waiting = Arc::<AtomicUsize>::default();
        let (requests, asked) = request_queue::channel();
        let hear: crate::backend::Hear = Arc::new({
            let (rest, ended, why) = (Arc::clone(&rest), Arc::clone(&ended), Arc::clone(&why));
            let (behind, waiting, nudge) =
                (Arc::clone(&behind), Arc::clone(&waiting), requests.clone());
            let sender = Mutex::new(Some(sender));
            // A resumed chat is one the window knows; a fresh one brings a new session id.
            let greeted = AtomicBool::new(wake.is_none_or(|wake| wake.resume.is_none()));
            // The chat's stream is over for this window; whoever sends its requests is woken to see so.
            let over = move |reason: String, sender: &Mutex<Option<SyncSender<_>>>| {
                // Only the first reason is the one the window is told.
                if ended.swap(true, Ordering::Relaxed) {
                    return;
                }
                if let Ok(mut why) = why.lock() {
                    *why = reason;
                }
                if let Ok(mut sender) = sender.lock() {
                    sender.take();
                }
                nudge.close();
            };
            move |said| match said {
                Said::Frame(frame) | Said::Event { frame, .. } => {
                    if rest.answer(&frame) {
                        return;
                    }
                    if frame["method"] == "ready" && !greeted.swap(true, Ordering::Relaxed) {
                        return;
                    }
                    let bytes = weight(&frame);
                    let room = waiting.fetch_add(bytes, Ordering::Relaxed) + bytes <= INBOX_BYTES;
                    let taken = room
                        && sender.lock().is_ok_and(|sender| {
                            sender
                                .as_ref()
                                .is_none_or(|sender| sender.try_send((bytes, frame)).is_ok())
                        });
                    // A window that cannot keep up stops following; the reader of every chat never waits on one.
                    if !taken {
                        behind.store(true, Ordering::Relaxed);
                        over(FELL_BEHIND.into(), &sender);
                    }
                }
                Said::Ended(reason) => over(reason.unwrap_or_default(), &sender),
            }
        });

        let (emitter, pumping, over) = (Arc::clone(emit), Arc::clone(&rest), Arc::clone(&ended));
        std::thread::spawn(move || {
            let mut segment = String::new();
            let mut steps = Steps::default();
            pump_stream(Weighed { frames, waiting }, |frame| {
                emitter(render(sanitize(frame, &mut steps), &mut segment));
            });
            over.store(true, Ordering::Relaxed);
            if pumping.closed() {
                return;
            }
            let stderr = why.lock().map(|why| why.clone()).unwrap_or_default();
            emitter(json!({ "method": "exit", "params": { "stderr": stderr } }));
        });

        let (backend, folder) = (Arc::clone(&self.backend), self.workspace.clone());
        let resume = resume.map(str::to_owned);
        let settings = wake.map(|wake| wake.settings.clone().unwrap_or_else(|| json!({})));
        // A chat woken from sleep resumes the one this window just let go to sleep.
        let waking = wake.is_some();
        let (resting, over) = (Arc::clone(&rest), Arc::clone(&ended));
        let (behind, gone) = (Arc::clone(&behind), Arc::clone(&closed));
        std::thread::spawn(move || {
            let opened = backend.connection().and_then(|connection| {
                let chat = connection.open_chat(
                    &folder,
                    resume.as_deref(),
                    waking,
                    settings.as_ref(),
                    Arc::clone(&hear),
                )?;
                Ok((connection, chat))
            });
            let (connection, chat) = match opened {
                Ok(opened) => opened,
                Err(error) => return hear(Said::Ended(Some(error))),
            };
            // Kept only by the connection from here, so the stream ends when the connection lets go.
            drop(hear);
            while let Some(request) = asked.next() {
                if gone.load(Ordering::Relaxed) || over.load(Ordering::Relaxed) {
                    break;
                }
                // Failed ordinary admission keeps its reservation; the next
                // dispatch can select a control instead of waiting behind it.
                let method = request.frame["method"].as_str().unwrap_or_default();
                if connection.has_room(request.bytes, method) {
                    match connection.tell(&chat, request.frame.clone()) {
                        Ok(()) => {
                            asked.complete(request);
                            continue;
                        }
                        Err(refused) if refused == crate::backend::NOT_TAKING => {}
                        Err(refused) if refused == crate::backend::TOO_LARGE => {
                            asked.complete(request);
                            continue;
                        }
                        Err(_) => return,
                    }
                }
                asked.retry(request);
            }
            // A window that stopped following a chat lets go of it and no more: the
            // chat is not this window's to stop for whoever else is watching it.
            if behind.load(Ordering::Relaxed) {
                return connection.leave(&chat);
            }
            // A chat that ended, or has said it is going to sleep, is not told to stop.
            if over.load(Ordering::Relaxed) || matches!(resting.current(), Some(Settled::Asleep(_)))
            {
                return;
            }
            let shutdown = json!({ "jsonrpc": "2.0", "id": 0, "method": "shutdown" });
            let _ = connection.tell(&chat, shutdown);
            let _ = connection.tell(&chat, json!({ "method": "session.close" }));
        });

        Live {
            requests,
            ended,
            closed,
            next_id: 1,
            emit: Arc::clone(emit),
            rest,
        }
    }
}

impl Live {
    fn has_ended(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        self.requests.close();
    }
}

/// Roughly what a frame holds: the text in it.
fn weight(frame: &Value) -> usize {
    match frame {
        Value::String(text) => text.len(),
        Value::Array(items) => items.iter().map(weight).sum(),
        Value::Object(fields) => fields.values().map(weight).sum::<usize>() + 16 * fields.len(),
        _ => 8,
    }
}

/// What the window has yet to draw of one chat, with its size given back as it is taken.
struct Weighed {
    frames: Receiver<(usize, Value)>,
    waiting: Arc<AtomicUsize>,
}

/// Where the stream is read from: a chat's bounded inbox, or a plain channel.
trait Incoming {
    fn next(&self, within: Option<Duration>) -> Result<Value, RecvTimeoutError>;
}

impl Incoming for Receiver<Value> {
    fn next(&self, within: Option<Duration>) -> Result<Value, RecvTimeoutError> {
        match within {
            Some(within) => self.recv_timeout(within),
            None => self.recv().map_err(|_| RecvTimeoutError::Disconnected),
        }
    }
}

impl Incoming for Weighed {
    fn next(&self, within: Option<Duration>) -> Result<Value, RecvTimeoutError> {
        let (bytes, frame) = match within {
            Some(within) => self.frames.recv_timeout(within)?,
            None => self
                .frames
                .recv()
                .map_err(|_| RecvTimeoutError::Disconnected)?,
        };
        self.waiting.fetch_sub(bytes, Ordering::Relaxed);
        Ok(frame)
    }
}

fn forward(app: &AppHandle, key: &str) -> Emit {
    let (app, key) = (app.clone(), key.to_owned());
    Arc::new(move |frame| {
        let _ = app.emit("medha-live", json!({ "key": key, "frame": frame }));
    })
}

fn sanitize(mut frame: Value, steps: &mut Steps) -> Value {
    if frame["method"] != "event" {
        return frame;
    }
    let kind = frame["params"]["kind"].as_str().map(str::to_owned);
    let Some(params) = frame["params"].as_object_mut() else {
        return frame;
    };
    let field = |params: &serde_json::Map<String, Value>, key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let (id, tool) = (field(params, "id"), field(params, "tool"));
    match kind.as_deref() {
        Some("tool.call") => {
            let args = params.remove("args").unwrap_or(Value::Null);
            let call = steps.call(&id, &tool, &args);
            put(params, "verb", Some(Value::String(call.verb)));
            put(params, "target", call.target.map(Value::String));
            put(params, "file_path", call.file_path.map(Value::String));
            put(params, "input_label", call.input_label.map(Value::from));
            put(params, "input", call.input.map(Value::String));
            put(params, "plan", call.plan);
        }
        Some("tool.observation") => {
            let output = params.remove("payload").unwrap_or(Value::Null);
            let ok = params.get("ok") == Some(&Value::Bool(true));
            let outcome = steps.result(&id, &tool, ok, &output);
            put(
                params,
                "error_code",
                outcome.error_code.map(|code| Value::String(code.into())),
            );
            put(params, "summary", outcome.summary.map(Value::String));
            put(params, "output", outcome.output.map(Value::String));
            put(params, "detail", outcome.detail.map(Value::String));
        }
        _ => {}
    }
    frame
}

fn put(params: &mut serde_json::Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        params.insert(key.into(), value);
    }
}

/// Streamed text is Markdown only as a whole, so each text segment is rendered
/// from its start on each coalesced update. A tool call, reasoning or the end of the turn
/// closes the segment, matching where the window starts a new one.
fn render(mut frame: Value, segment: &mut String) -> Value {
    if frame["method"] != "event" {
        return frame;
    }
    match frame["params"]["kind"].as_str() {
        Some("model.text") => {
            segment.push_str(frame["params"]["delta"].as_str().unwrap_or_default());
            frame["params"]["html"] = Value::String(transcript_view::to_html(segment));
        }
        Some(
            "tool.call" | "model.reasoning" | "message.steered" | "model.restarted" | "turn.done"
            | "turn.error" | "turn.cancelled",
        ) => segment.clear(),
        _ => {}
    }
    frame
}

/// History arrives as text. What the assistant said is Markdown, rendered here
/// as live text is, so the backend sends each reply once and not twice.
pub(crate) fn render_history(mut page: Value) -> Value {
    for event in page["events"].as_array_mut().into_iter().flatten() {
        if event["kind"] == "assistant"
            && let Some(text) = event["text"].as_str()
        {
            event["html"] = Value::String(transcript_view::to_html(text));
        }
    }
    page
}

pub(crate) fn is_token(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(crate) fn is_ulid(id: &str) -> bool {
    id.len() == 26 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "live_sleep_tests.rs"]
mod sleep_tests;
