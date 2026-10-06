//! Bounded per-chat dispatch. In-flight admission keeps its reservation, so a
//! retry cannot expand the queue. Control answers overtake ordinary requests.
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

#[derive(Clone)]
pub(super) struct Requests(Arc<Queue>);
pub(super) struct Receiver(Requests);
pub(super) struct Pending {
    pub bytes: usize,
    pub frame: Value,
    urgent: bool,
}
struct Queue {
    state: Mutex<State>,
    changed: Condvar,
}
#[derive(Default)]
struct State {
    normal: VecDeque<Pending>,
    urgent: VecDeque<Pending>,
    frames: [usize; 2],
    bytes: [usize; 2],
    closed: bool,
}

pub(super) fn channel() -> (Requests, Receiver) {
    let send = Requests(Arc::new(Queue {
        state: Mutex::default(),
        changed: Condvar::new(),
    }));
    (send.clone(), Receiver(send))
}
impl Requests {
    pub fn try_send(
        &self,
        (bytes, frame): (usize, Value),
    ) -> Result<(), TrySendError<(usize, Value)>> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return Err(TrySendError::Disconnected((bytes, frame)));
        }
        if frame.is_null() {
            self.0.changed.notify_one();
            return Ok(());
        }
        let urgent = bytes <= wire::CONTROL_FRAME_BYTES
            && frame["method"].as_str().is_some_and(wire::is_control);
        let lane = usize::from(urgent);
        let (frames, budget) = if urgent {
            (32, 1024 * 1024)
        } else {
            (super::OUTBOX_FRAMES, super::OUTBOX_BYTES)
        };
        if state.frames[lane] >= frames || state.bytes[lane].saturating_add(bytes) > budget {
            return Err(TrySendError::Full((bytes, frame)));
        }
        state.frames[lane] += 1;
        state.bytes[lane] += bytes;
        let request = Pending {
            bytes,
            frame,
            urgent,
        };
        if urgent {
            state.urgent.push_back(request);
        } else {
            state.normal.push_back(request);
        }
        self.0.changed.notify_one();
        Ok(())
    }
    pub fn close(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.closed = true;
        state.normal.clear();
        state.urgent.clear();
        self.0.changed.notify_all();
    }
}
impl Receiver {
    pub fn next(&self) -> Option<Pending> {
        let mut state = self
            .0
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while !state.closed && state.normal.is_empty() && state.urgent.is_empty() {
            state = self
                .0
                .0
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.closed {
            None
        } else {
            state
                .urgent
                .pop_front()
                .or_else(|| state.normal.pop_front())
        }
    }
    pub fn complete(&self, request: Pending) {
        let mut state = self
            .0
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let lane = usize::from(request.urgent);
        state.frames[lane] -= 1;
        state.bytes[lane] -= request.bytes;
    }
    pub fn retry(&self, request: Pending) {
        let mut state = self
            .0
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !state.closed {
            let urgent = request.urgent;
            if request.urgent {
                state.urgent.push_front(request);
            } else {
                state.normal.push_front(request);
            }
            if !urgent && !state.urgent.is_empty() {
                return;
            }
            // Sleep with the mutex released; a new control frame wakes us at once.
            let _ = self
                .0
                .0
                .changed
                .wait_timeout(state, Duration::from_millis(20));
        }
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.0.close();
    }
}
