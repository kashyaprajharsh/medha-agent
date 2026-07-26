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

use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// What a surface can ask of a running session.
#[derive(Debug)]
pub enum Interrupt {
    /// Inject a user message at the next turn boundary; the session continues.
    /// The label says where the text came from — an operator typing is `User`,
    /// a sub-agent's report is whatever that agent touched.
    Steer(String, crate::types::TrustLabel),
    /// Finish in-flight work gracefully, then stop with `StopReason::Interrupted`.
    CancelTurn,
}

/// How many interrupts a session has been sent. Monotonic, so a watcher that
/// samples it and later compares can tell that *something* arrived without
/// having to consume it — the queue belongs to the session loop, and a watcher
/// that took from it would be stealing the loop's input.
pub type Activity = watch::Receiver<u64>;

/// Cloneable sender held by the surface (TUI / ACP / gateway later).
#[derive(Clone)]
pub struct InterruptHandle {
    tx: mpsc::UnboundedSender<Interrupt>,
    cancel: CancellationToken,
    activity: Arc<watch::Sender<u64>>,
}

impl InterruptHandle {
    pub fn steer(&self, text: impl Into<String>) {
        let _ = self.steer_labelled(text, crate::types::TrustLabel::User);
    }

    /// Queue text that did not come from the operator, carrying its label so a
    /// consequential action derived from it escalates the same way a direct
    /// observation would.
    pub fn steer_labelled(&self, text: impl Into<String>, trust: crate::types::TrustLabel) -> bool {
        if self.tx.send(Interrupt::Steer(text.into(), trust)).is_err() {
            return false;
        }
        // After the send, never before: a watcher woken by the count must find
        // the text already queued behind it.
        self.activity.send_modify(|count| *count += 1);
        true
    }

    /// Request a graceful stop. Also trips the cancellation token so the
    /// kernel's in-flight waits (stream, tool dispatch) wake immediately.
    pub fn cancel_turn(&self) {
        let _ = self.tx.send(Interrupt::CancelTurn);
        self.cancel.cancel();
        self.activity.send_modify(|count| *count += 1);
    }

    /// Watch for anything queued against this session.
    ///
    /// This is what lets a long wait inside a tool end the moment its own
    /// operator says something, instead of holding the turn for its full
    /// duration against instructions that are already obsolete.
    pub fn activity(&self) -> Activity {
        self.activity.subscribe()
    }
}

/// Kernel-side receiver; one per `run_session` call.
pub struct InterruptQueue {
    rx: mpsc::UnboundedReceiver<Interrupt>,
    cancel: CancellationToken,
    activity: Arc<watch::Sender<u64>>,
}

impl InterruptQueue {
    pub fn pair() -> (InterruptHandle, InterruptQueue) {
        Self::rooted(CancellationToken::new())
    }

    /// A pair whose cancellation *is* `cancel`.
    ///
    /// For a caller that already owns a token for this work — a sub-agent's,
    /// say. Tripping it then routes through the loop's own settle path, so the
    /// run ends by returning rather than by having its future dropped.
    pub fn rooted(cancel: CancellationToken) -> (InterruptHandle, InterruptQueue) {
        let (tx, rx) = mpsc::unbounded_channel();
        let activity = Arc::new(watch::Sender::new(0));
        (
            InterruptHandle {
                tx,
                cancel: cancel.clone(),
                activity: Arc::clone(&activity),
            },
            InterruptQueue {
                rx,
                cancel,
                activity,
            },
        )
    }

    /// Watch for anything queued against this session. Held by the queue as
    /// well as the handle so a watcher outlives every surface that could steer
    /// — otherwise the channel closes the moment the last handle drops and the
    /// watch reads as "gone" rather than "quiet".
    pub fn activity(&self) -> Activity {
        self.activity.subscribe()
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
    pub fn drain_steers(&mut self) -> Vec<(String, crate::types::TrustLabel)> {
        let mut out = Vec::new();
        while let Ok(i) = self.rx.try_recv() {
            if let Interrupt::Steer(text, trust) = i {
                out.push((text, trust));
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
        assert_eq!(
            queue.drain_steers(),
            vec![
                ("first".to_string(), crate::types::TrustLabel::User),
                ("second".to_string(), crate::types::TrustLabel::User)
            ]
        );
        assert!(queue.drain_steers().is_empty(), "drained once, gone");

        handle.steer("late");
        handle.cancel_turn();
        assert!(queue.cancel_requested(), "cancel_turn trips the token");
        assert_eq!(
            queue.drain_steers(),
            vec![("late".to_string(), crate::types::TrustLabel::User)],
            "cancel entries carry no text"
        );
    }

    #[test]
    fn activity_counts_without_consuming_what_the_loop_will_read() {
        let (handle, mut queue) = InterruptQueue::pair();
        let mut activity = handle.activity();
        assert_eq!(*activity.borrow_and_update(), 0);

        handle.steer("narrow it");
        assert!(activity.has_changed().unwrap(), "a steer is activity");
        // Watching must not take the text: the queue belongs to the session
        // loop, and a watcher that consumed from it would steal its input.
        assert_eq!(
            queue.drain_steers(),
            vec![("narrow it".to_string(), crate::types::TrustLabel::User)]
        );
    }

    #[tokio::test]
    async fn a_watcher_outlives_every_surface_that_could_steer() {
        let (handle, queue) = InterruptQueue::pair();
        let mut activity = queue.activity();
        activity.borrow_and_update();
        drop(handle);
        // The queue holds a sender too, so this reads as "quiet", not "gone".
        // Otherwise a wait would end instantly the moment a surface detached.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), activity.changed())
                .await
                .is_err()
        );
    }
}
