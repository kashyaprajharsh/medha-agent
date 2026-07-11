//! Kernel-owned interrupts (§4.1, Vol 3 §7): graceful mid-tool cancellation
//! and a steer queue. The contract with surfaces:
//!
//! - **Steer**: text queued mid-turn is injected as a user message at the next
//!   turn boundary; the session continues. Logged as a normal `user.message`,
//!   so projection/resume need no changes.
//! - **CancelTurn**: the session stops *gracefully* — in-flight tools get a
//!   bounded settle window, every admitted intent still receives an
//!   observation (real or synthesized `[interrupted]`), and the loop returns
//!   `StopReason::Interrupted` with the settled history. Never a mid-tool kill.
//!
//! Steers that never reached a boundary when a cancel lands are handed back to
//! the surface via `StreamSink::steers_returned` — typed text must not vanish.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// What a surface can ask of a running session.
#[derive(Debug)]
pub enum Interrupt {
    /// Inject a user message at the next turn boundary; the session continues.
    Steer(String),
    /// Finish in-flight work gracefully, then stop with `StopReason::Interrupted`.
    CancelTurn,
}

/// Cloneable sender held by the surface (TUI / ACP / gateway later).
#[derive(Clone)]
pub struct InterruptHandle {
    tx: mpsc::UnboundedSender<Interrupt>,
    cancel: CancellationToken,
}

impl InterruptHandle {
    pub fn steer(&self, text: impl Into<String>) {
        let _ = self.tx.send(Interrupt::Steer(text.into()));
    }

    /// Request a graceful stop. Also trips the cancellation token so the
    /// kernel's in-flight waits (stream, tool dispatch) wake immediately.
    pub fn cancel_turn(&self) {
        let _ = self.tx.send(Interrupt::CancelTurn);
        self.cancel.cancel();
    }
}

/// Kernel-side receiver; one per `run_session` call.
pub struct InterruptQueue {
    rx: mpsc::UnboundedReceiver<Interrupt>,
    cancel: CancellationToken,
}

impl InterruptQueue {
    pub fn pair() -> (InterruptHandle, InterruptQueue) {
        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        (InterruptHandle { tx, cancel: cancel.clone() }, InterruptQueue { rx, cancel })
    }

    /// Token the loop selects on; clone freely into tool waits.
    pub fn token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Take every queued steer message, in send order. `CancelTurn` entries
    /// carry no payload (the token is the signal) and are simply consumed.
    pub fn drain_steers(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(i) = self.rx.try_recv() {
            if let Interrupt::Steer(s) = i {
                out.push(s);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steers_drain_in_order_and_cancel_trips_the_token() {
        let (handle, mut queue) = InterruptQueue::pair();
        handle.steer("first");
        handle.steer("second");
        assert!(!queue.cancel_requested());
        assert_eq!(queue.drain_steers(), vec!["first".to_string(), "second".to_string()]);
        assert!(queue.drain_steers().is_empty(), "drained once, gone");

        handle.steer("late");
        handle.cancel_turn();
        assert!(queue.cancel_requested(), "cancel_turn trips the token");
        assert_eq!(queue.drain_steers(), vec!["late".to_string()], "cancel entries carry no text");
    }
}
