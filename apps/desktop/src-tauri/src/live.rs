//! Live sessions: one `medha --acp` bridge per session, driven by the same
//! kernel, config and credentials as the TUI. Frames are forwarded to the
//! window as `medha-live` events, with raw tool arguments and output reshaped by
//! `transcript-view` exactly as the read-only history reshapes them.

use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, Once, Weak};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use transcript_view::Steps;

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
const STDERR_TAIL: usize = 12;
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
    child: Child,
    input: ChildStdin,
    next_id: u64,
    emit: Emit,
    rest: Arc<Rest>,
}

type Emit = Arc<dyn Fn(Value) + Send + Sync>;

impl Drop for Live {
    fn drop(&mut self) {
        let _ = writeln!(
            self.input,
            "{}",
            json!({ "jsonrpc": "2.0", "id": 0, "method": "shutdown" })
        );
        let _ = self.input.flush();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Open = HashMap<String, Live>;

static EVERY: Mutex<Vec<Weak<Inner>>> = Mutex::new(Vec::new());
static SWEEPER: Once = Once::new();

pub struct LiveSessions {
    inner: Arc<Inner>,
}

struct Inner {
    workspace: PathBuf,
    open: Mutex<Open>,
    backend: Option<PathBuf>,
    env: Vec<(String, String)>,
}

impl LiveSessions {
    pub fn new(workspace: PathBuf) -> Self {
        Self::launching(workspace, None, Vec::new())
    }

    fn launching(workspace: PathBuf, backend: Option<PathBuf>, env: Vec<(String, String)>) -> Self {
        let inner = Arc::new(Inner {
            workspace,
            open: Mutex::new(HashMap::new()),
            backend,
            env,
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
        if let Some(live) = open.get_mut(key)
            && matches!(live.child.try_wait(), Ok(None))
        {
            return Ok(());
        }
        let live = self.inner.spawn(&emit, resume, None)?;
        open.insert(key.to_owned(), live);
        Ok(())
    }

    pub fn request(&self, key: &str, method: &str, params: Value) -> Result<u64, String> {
        if !REQUESTS.contains(&method) {
            return Err(format!("{method} is not a desktop request"));
        }
        let mut open = self.inner.awake(key)?;
        let live = open.get_mut(key).ok_or("this session is not live")?;
        let id = live.next_id;
        live.next_id += 1;
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(live.input, "{frame}")
            .and_then(|()| live.input.flush())
            .map_err(|_| {
                "This session has stopped. Start a new session to continue.".to_string()
            })?;
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
            if sleep::is_focused(key) || !matches!(live.child.try_wait(), Ok(None)) {
                continue;
            }
            if let Some(ask) = live.rest.ask(now, sleep::GRACE)
                && writeln!(live.input, "{ask}")
                    .and_then(|()| live.input.flush())
                    .is_err()
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
            let mut woken = self.spawn(&emit, wake.resume.as_deref(), Some(&wake))?;
            // Ids keep counting, so no answer is matched to an earlier request.
            woken.next_id = next_id;
            if let Some(gone) = open.insert(key.to_owned(), woken) {
                std::thread::spawn(move || drop(gone));
            }
            return Ok(open);
        }
    }

    fn spawn(
        &self,
        emit: &Emit,
        resume: Option<&str>,
        wake: Option<&Wake>,
    ) -> Result<Live, String> {
        let backend = match &self.backend {
            Some(backend) => backend.clone(),
            None => crate::service::backend_executable()?,
        };
        let mut command = Command::new(backend);
        command.arg("--acp").envs(self.env.iter().cloned());
        if let Some((address, token)) = crate::mcp_host::env() {
            command
                .env("MEDHA_MCP_HOST", address)
                .env("MEDHA_MCP_HOST_TOKEN", token);
        }
        if let Some(id) = resume {
            command.arg("--resume").arg(id);
        }
        if let Some(wake) = wake {
            let settings = wake.settings.clone().unwrap_or_else(|| json!({}));
            command.env("MEDHA_ACP_SETTINGS", settings.to_string());
        }
        let mut child = command
            .current_dir(&self.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("Could not start Medha: {error}"))?;
        let input = child.stdin.take().ok_or("Medha stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("Medha stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("Medha stderr unavailable")?;

        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
        let sink = Arc::clone(&tail);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Ok(mut tail) = sink.lock() {
                    if tail.len() == STDERR_TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            }
        });

        let rest = Rest::new();
        let emitter = Arc::clone(emit);
        // A bounded pipe decouples blocking stdout reads from timed UI updates.
        // It cannot build an unbounded backlog when the renderer is slower.
        let (sender, frames) = sync_channel(64);
        let reading = Arc::clone(&rest);
        // A resumed chat is one the window knows; a fresh one brings a new session id.
        let mut greeted = wake.is_none_or(|wake| wake.resume.is_none());
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if reading.answer(&frame) {
                    continue;
                }
                if !greeted && frame["method"] == "ready" {
                    greeted = true;
                    continue;
                }
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });
        let pumping = Arc::clone(&rest);
        std::thread::spawn(move || {
            let mut segment = String::new();
            let mut steps = Steps::default();
            pump_stream(frames, |frame| {
                emitter(render(sanitize(frame, &mut steps), &mut segment));
            });
            if pumping.closed() {
                return;
            }
            let stderr = tail
                .lock()
                .map(|tail| tail.iter().cloned().collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            emitter(json!({ "method": "exit", "params": { "stderr": stderr } }));
        });

        Ok(Live {
            child,
            input,
            next_id: 1,
            emit: Arc::clone(emit),
            rest,
        })
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
