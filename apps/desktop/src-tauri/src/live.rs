//! Live sessions: each is a chat in the Medha backend, driven by the same
//! kernel, config and credentials as the TUI. Frames are forwarded to the
//! window as `medha-live` events, with raw tool arguments and output reshaped by
//! `transcript-view` exactly as the read-only history reshapes them.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, Once, Weak};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use transcript_view::Steps;

use crate::backend::{Backend, Said};
use crate::sleep::{self, Rest, Settled, Wake};

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

fn pump_stream(frames: Receiver<Value>, mut emit: impl FnMut(Value)) {
    let mut stream = StreamFrames::default();
    loop {
        let received = match stream.deadline {
            Some(deadline) => {
                frames.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            }
            None => frames.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
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
    /// What the chat is asked, in the order it was asked. Dropping it closes the chat.
    requests: Sender<Value>,
    ended: Arc<AtomicBool>,
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
        live.requests.send(frame).map_err(|_| STOPPED.to_string())?;
        Ok(id)
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
                && live.requests.send(ask).is_err()
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
        let why = Arc::new(Mutex::new(String::new()));
        let (sender, frames) = channel();
        let hear: crate::backend::Hear = Arc::new({
            let (rest, ended, why) = (Arc::clone(&rest), Arc::clone(&ended), Arc::clone(&why));
            let sender = Mutex::new(Some(sender));
            // A resumed chat is one the window knows; a fresh one brings a new session id.
            let greeted = AtomicBool::new(wake.is_none_or(|wake| wake.resume.is_none()));
            move |said| match said {
                Said::Frame(frame) => {
                    if rest.answer(&frame) {
                        return;
                    }
                    if frame["method"] == "ready" && !greeted.swap(true, Ordering::Relaxed) {
                        return;
                    }
                    if let Ok(sender) = sender.lock()
                        && let Some(sender) = sender.as_ref()
                    {
                        let _ = sender.send(frame);
                    }
                }
                Said::Ended(reason) => {
                    if let Ok(mut why) = why.lock() {
                        *why = reason.unwrap_or_default();
                    }
                    ended.store(true, Ordering::Relaxed);
                    if let Ok(mut sender) = sender.lock() {
                        sender.take();
                    }
                }
            }
        });

        let (emitter, pumping, over) = (Arc::clone(emit), Arc::clone(&rest), Arc::clone(&ended));
        std::thread::spawn(move || {
            let mut segment = String::new();
            let mut steps = Steps::default();
            pump_stream(frames, |frame| {
                emitter(render(sanitize(frame, &mut steps), &mut segment));
            });
            over.store(true, Ordering::Relaxed);
            if pumping.closed() {
                return;
            }
            let stderr = why.lock().map(|why| why.clone()).unwrap_or_default();
            emitter(json!({ "method": "exit", "params": { "stderr": stderr } }));
        });

        let (requests, asked) = channel::<Value>();
        let (backend, folder) = (Arc::clone(&self.backend), self.workspace.clone());
        let resume = resume.map(str::to_owned);
        let settings = wake.map(|wake| wake.settings.clone().unwrap_or_else(|| json!({})));
        let (resting, over) = (Arc::clone(&rest), Arc::clone(&ended));
        std::thread::spawn(move || {
            let opened = backend.connection().and_then(|connection| {
                let chat = connection.open_chat(
                    &folder,
                    resume.as_deref(),
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
            for frame in asked {
                if connection.tell(&chat, frame).is_err() {
                    return;
                }
            }
            // A chat that is over, or has said it is going to sleep, is not told to stop.
            let leaving = matches!(resting.current(), Some(Settled::Asleep(_)));
            if over.load(Ordering::Relaxed) || leaving {
                return;
            }
            let shutdown = json!({ "jsonrpc": "2.0", "id": 0, "method": "shutdown" });
            let _ = connection.tell(&chat, shutdown);
            let _ = connection.tell(&chat, json!({ "method": "session.close" }));
        });

        Live {
            requests,
            ended,
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

#[cfg(all(test, unix))]
#[path = "live_sleep_tests.rs"]
mod sleep_tests;
