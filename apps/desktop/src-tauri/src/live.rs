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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use transcript_view::Steps;

const REQUESTS: [&str; 21] = [
    "mcp.signin",
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
                Some("model.text" | "model.reasoning")
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
}

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

pub struct LiveSessions {
    workspace: PathBuf,
    open: Mutex<HashMap<String, Live>>,
}

impl LiveSessions {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            open: Mutex::new(HashMap::new()),
        }
    }

    pub fn open(&self, app: &AppHandle, key: &str, resume: Option<&str>) -> Result<(), String> {
        if !is_token(key) {
            return Err("invalid live session key".into());
        }
        if let Some(id) = resume
            && !is_ulid(id)
        {
            return Err("invalid session id".into());
        }
        let mut open = self.open.lock().map_err(|error| error.to_string())?;
        if let Some(live) = open.get_mut(key)
            && matches!(live.child.try_wait(), Ok(None))
        {
            return Ok(());
        }
        let mut command = Command::new(crate::service::backend_executable()?);
        command.arg("--acp");
        if let Some(id) = resume {
            command.arg("--resume").arg(id);
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

        let (app, key_owned) = (app.clone(), key.to_owned());
        // A bounded pipe decouples blocking stdout reads from timed UI updates.
        // It cannot build an unbounded backlog when the renderer is slower.
        let (sender, frames) = sync_channel(64);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if let Ok(frame) = serde_json::from_str::<Value>(&line)
                    && sender.send(frame).is_err()
                {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            let mut segment = String::new();
            let mut steps = Steps::default();
            pump_stream(frames, |frame| {
                forward(
                    &app,
                    &key_owned,
                    render(sanitize(frame, &mut steps), &mut segment),
                );
            });
            let stderr = tail
                .lock()
                .map(|tail| tail.iter().cloned().collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            forward(
                &app,
                &key_owned,
                json!({ "method": "exit", "params": { "stderr": stderr } }),
            );
        });

        open.insert(
            key.to_owned(),
            Live {
                child,
                input,
                next_id: 1,
            },
        );
        Ok(())
    }

    pub fn request(&self, key: &str, method: &str, params: Value) -> Result<u64, String> {
        if !REQUESTS.contains(&method) {
            return Err(format!("{method} is not a desktop request"));
        }
        let mut open = self.open.lock().map_err(|error| error.to_string())?;
        let live = open.get_mut(key).ok_or("this session is not live")?;
        let id = live.next_id;
        live.next_id += 1;
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(live.input, "{frame}")
            .and_then(|()| live.input.flush())
            .map_err(|error| format!("Medha stopped: {error}"))?;
        Ok(id)
    }

    pub fn close(&self, key: &str) -> Result<(), String> {
        let removed = self
            .open
            .lock()
            .map_err(|error| error.to_string())?
            .remove(key);
        drop(removed);
        Ok(())
    }
}

fn forward(app: &AppHandle, key: &str, frame: Value) {
    let _ = app.emit("medha-live", json!({ "key": key, "frame": frame }));
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
