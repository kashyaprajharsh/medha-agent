//! Idle chats sleep: the process goes, the conversation stays, the next request wakes it.

use serde_json::{Value, json};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) const GRACE: Duration = Duration::from_secs(120);
pub(crate) const SWEEP: Duration = Duration::from_secs(20);
/// An idle backend answers at once; this bounds the wait on the window's thread.
const PATIENCE: Duration = Duration::from_secs(3);
const ASK: &str = "medha.sleep";

static FOCUSED: Mutex<Option<String>> = Mutex::new(None);

pub(crate) fn focus(key: Option<String>) {
    if let Ok(mut focused) = FOCUSED.lock() {
        *focused = key;
    }
}

pub(crate) fn is_focused(key: &str) -> bool {
    FOCUSED
        .lock()
        .is_ok_and(|focused| focused.as_deref() == Some(key))
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Wake {
    pub resume: Option<String>,
    pub settings: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Awake,
    Asking,
    Leaving(Wake),
    Asleep(Wake),
}

struct State {
    phase: Phase,
    used: Instant,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Settled {
    Awake,
    Asleep(Wake),
}

pub(crate) struct Rest {
    state: Mutex<State>,
    changed: Condvar,
}

impl Rest {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                phase: Phase::Awake,
                used: Instant::now(),
            }),
            changed: Condvar::new(),
        })
    }

    fn with<T>(&self, change: impl FnOnce(&mut State) -> T) -> Option<T> {
        let mut state = self.state.lock().ok()?;
        let result = change(&mut state);
        self.changed.notify_all();
        Some(result)
    }

    pub fn touch(&self) {
        self.with(|state| state.used = Instant::now());
    }

    /// Whether the chat is busy is the backend's to say; this only asks.
    pub fn ask(&self, now: Instant, grace: Duration) -> Option<Value> {
        self.with(|state| {
            let quiet = now.saturating_duration_since(state.used) >= grace;
            (state.phase == Phase::Awake && quiet).then(|| {
                state.phase = Phase::Asking;
                json!({ "jsonrpc": "2.0", "id": ASK, "method": "session.sleep" })
            })
        })
        .flatten()
    }

    pub fn answer(&self, frame: &Value) -> bool {
        if frame["id"] != ASK {
            return false;
        }
        let result = &frame["result"];
        self.with(|state| {
            state.phase = if result["slept"] == true {
                Phase::Leaving(Wake {
                    resume: result["session"].as_str().map(str::to_owned),
                    settings: Some(result["settings"].clone()).filter(Value::is_object),
                })
            } else {
                state.used = Instant::now();
                Phase::Awake
            };
        });
        true
    }

    pub fn closed(&self) -> bool {
        self.with(|state| match &state.phase {
            Phase::Leaving(wake) => {
                state.phase = Phase::Asleep(wake.clone());
                true
            }
            Phase::Asleep(_) => true,
            Phase::Asking => {
                state.phase = Phase::Awake;
                false
            }
            Phase::Awake => false,
        })
        .unwrap_or(false)
    }

    pub fn current(&self) -> Option<Settled> {
        let state = self.state.lock().ok()?;
        match &state.phase {
            Phase::Asking => None,
            Phase::Leaving(wake) | Phase::Asleep(wake) => Some(Settled::Asleep(wake.clone())),
            Phase::Awake => Some(Settled::Awake),
        }
    }

    /// A backend that said yes reads nothing more, so its successor need not wait for it to exit.
    pub fn settle(&self) -> Result<Settled, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, PATIENCE, |state| state.phase == Phase::Asking)
            .map_err(|error| error.to_string())?;
        if timeout.timed_out() {
            return Err(
                "Medha is still finishing something in this chat. Try again in a moment.".into(),
            );
        }
        Ok(match &state.phase {
            Phase::Leaving(wake) | Phase::Asleep(wake) => Settled::Asleep(wake.clone()),
            _ => Settled::Awake,
        })
    }
}

#[cfg(test)]
#[path = "sleep_tests.rs"]
mod tests;
