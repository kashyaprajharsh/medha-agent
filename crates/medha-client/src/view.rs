//! A bounded asynchronous viewer. The shared socket reader never waits for
//! rendering; overload detaches this viewer and leaves other viewers alone.

use super::{Chat, Connection, Hear, Said, size};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;

const BEHIND: &str = "This viewer could not keep up. Reconnect to recover the chat.";

#[derive(Clone, Copy)]
pub struct ViewLimits {
    pub frames: usize,
    pub bytes: usize,
}

impl Default for ViewLimits {
    fn default() -> Self {
        Self {
            frames: 512,
            bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Clone)]
struct Ending {
    reason: Option<String>,
    discard: bool,
}

#[derive(Default)]
struct State {
    bytes: usize,
    queued: VecDeque<Queued>,
    ending: Option<Ending>,
    chat: Option<Chat>,
}

struct Inbox {
    connection: Weak<Connection>,
    state: Mutex<State>,
    changed: Notify,
    limits: ViewLimits,
}

struct Queued {
    said: Said,
    bytes: usize,
}

impl Inbox {
    fn end(&self, reason: Option<String>, discard: bool) {
        let (chat, discarded) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state
                .ending
                .as_ref()
                .is_some_and(|ending| ending.discard || !discard)
            {
                return;
            }
            state.ending = Some(Ending { reason, discard });
            let discarded = if discard {
                state.bytes = 0;
                std::mem::take(&mut state.queued)
            } else {
                VecDeque::new()
            };
            (state.chat.clone(), discarded)
        };
        drop(discarded);
        if let (Some(connection), Some(chat)) = (self.connection.upgrade(), chat) {
            connection.leave(&chat);
        }
        self.changed.notify_one();
    }

    fn accept(&self, said: Said) {
        if let Said::Ended(reason) = said {
            self.end(reason, false);
            return;
        }
        let bytes = match &said {
            Said::Frame(frame) | Said::Event { frame, .. } => size(frame),
            Said::Ended(_) => unreachable!(),
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.ending.is_some() {
            return;
        }
        if state.bytes.saturating_add(bytes) > self.limits.bytes
            || state.queued.len() >= self.limits.frames
        {
            drop(state);
            self.end(Some(BEHIND.into()), true);
            return;
        }
        state.bytes += bytes;
        state.queued.push_back(Queued { said, bytes });
        drop(state);
        self.changed.notify_one();
    }
}

/// Owns one attachment and its bounded queue. Replay position advances only
/// when a frame is delivered, never when it merely enters the queue. Callers
/// must resynchronize application state when the attach result reports a gap.
pub struct View {
    connection: Arc<Connection>,
    inbox: Arc<Inbox>,
    cursor: Option<protocol::Cursor>,
    finished: bool,
}

impl View {
    fn mailbox(connection: Arc<Connection>, limits: ViewLimits) -> Result<(Self, Hear), String> {
        if limits.frames == 0 || limits.bytes == 0 {
            return Err("viewer limits must be positive".into());
        }
        let inbox = Arc::new(Inbox {
            connection: Arc::downgrade(&connection),
            state: Mutex::default(),
            changed: Notify::new(),
            limits,
        });
        let incoming = inbox.clone();
        let hear: Hear = Arc::new(move |said| incoming.accept(said));
        Ok((
            Self {
                connection,
                inbox,
                cursor: None,
                finished: false,
            },
            hear,
        ))
    }

    fn bind(&mut self, chat: Chat, attached: &protocol::Attached, after: u64) {
        self.cursor = attached.stream.as_ref().map(|stream| protocol::Cursor {
            stream: stream.clone(),
            after,
        });
        let mut state = self
            .inbox
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ended = state.ending.is_some();
        state.chat = Some(chat.clone());
        drop(state);
        // Replay can fill the queue before the attach reply returns. A viewer
        // already refused must still detach once its handle becomes available.
        if ended {
            self.connection.leave(&chat);
        }
    }

    pub async fn create(
        connection: Arc<Connection>,
        params: &protocol::CreateSession,
        limits: ViewLimits,
    ) -> Result<(Self, protocol::LiveSession, protocol::Attached), String> {
        let (mut view, hear) = Self::mailbox(connection.clone(), limits)?;
        let (chat, made, attached) = connection.create_chat(params, hear).await?;
        view.bind(chat, &attached, 0);
        Ok((view, made, attached))
    }

    pub async fn attach(
        connection: Arc<Connection>,
        session: String,
        cursor: Option<protocol::Cursor>,
        limits: ViewLimits,
    ) -> Result<(Self, protocol::Attached), String> {
        let (mut view, hear) = Self::mailbox(connection.clone(), limits)?;
        let (chat, attached) = connection
            .attach_chat(session, cursor.clone(), hear)
            .await?;
        let after = if attached.gap {
            attached.head
        } else {
            cursor.map_or(attached.head, |cursor| cursor.after)
        };
        view.bind(chat, &attached, after);
        Ok((view, attached))
    }

    pub fn cursor(&self) -> Option<protocol::Cursor> {
        self.cursor.clone()
    }

    pub fn connection(&self) -> Arc<Connection> {
        self.connection.clone()
    }

    pub fn chat(&self) -> Chat {
        self.inbox
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .chat
            .as_ref()
            .expect("opened viewer")
            .clone()
    }

    pub async fn call<T: protocol::Command>(&self, params: &T) -> Result<T::Output, String> {
        self.connection.call_chat(&self.chat(), params).await
    }

    /// Application snapshots can cover queued replay. Discarding those frames
    /// is explicit and limited to this exact stream incarnation.
    pub fn covered_through(&mut self, cursor: protocol::Cursor) -> Result<(), String> {
        if self
            .cursor
            .as_ref()
            .is_none_or(|current| current.stream != cursor.stream || current.after > cursor.after)
        {
            return Err("the snapshot does not cover this viewer's stream".into());
        }
        self.cursor = Some(cursor);
        Ok(())
    }

    pub async fn recv(&mut self) -> Option<Said> {
        loop {
            if self.finished {
                return None;
            }
            let notified = self.inbox.changed.notified();
            // Inspect the queue and its end marker together. An end arriving
            // just after the final frame cannot cause that frame to be lost.
            let (queued, ending) = {
                let mut state = self
                    .inbox
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let queued = state.queued.pop_front();
                if let Some(queued) = &queued {
                    state.bytes -= queued.bytes;
                }
                (queued, state.ending.clone())
            };
            if let Some(queued) = queued {
                if let Said::Event {
                    stream: Some(stream),
                    seq,
                    ..
                } = &queued.said
                {
                    if let Some(cursor) = &self.cursor {
                        if cursor.stream != *stream {
                            self.inbox.end(
                                Some("The chat stream changed. Reconnect to recover it.".into()),
                                true,
                            );
                            continue;
                        }
                        if *seq <= cursor.after {
                            continue;
                        }
                    }
                    self.cursor = Some(protocol::Cursor {
                        stream: stream.clone(),
                        after: *seq,
                    });
                }
                return Some(queued.said);
            }
            if let Some(ending) = ending {
                self.finished = true;
                return Some(Said::Ended(ending.reason));
            }
            notified.await;
        }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        let (chat, queued) = {
            let mut state = self
                .inbox
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.ending = Some(Ending {
                reason: None,
                discard: true,
            });
            state.bytes = 0;
            (state.chat.take(), std::mem::take(&mut state.queued))
        };
        if let Some(chat) = chat {
            self.connection.leave(&chat);
        }
        drop(queued);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::{mpsc, watch};

    fn viewer(limits: ViewLimits) -> (View, Hear, mpsc::UnboundedReceiver<String>) {
        let (lines, _queued) = mpsc::unbounded_channel();
        let (controls, urgent) = mpsc::unbounded_channel();
        let connection = Arc::new(Connection {
            lines,
            controls,
            unsent: Arc::default(),
            routes: Mutex::default(),
            stop: watch::channel(false).0,
        });
        let (mut view, hear) = View::mailbox(connection.clone(), limits).unwrap();
        let chat = connection.listen("chat".into(), hear.clone()).unwrap();
        let attached = serde_json::from_value(
            json!({"session": "chat", "stream": "first", "head": 0, "replayed": 0, "gap": false}),
        )
        .unwrap();
        view.bind(chat, &attached, 0);
        (view, hear, urgent)
    }

    fn event(seq: u64) -> Said {
        Said::Event {
            stream: Some("first".into()),
            seq,
            frame: json!({"method": "note", "params": {"n": seq}}),
        }
    }

    #[tokio::test]
    async fn a_normal_end_delivers_every_final_frame_before_the_end() {
        let (mut view, hear, _) = viewer(ViewLimits::default());
        hear(event(1));
        hear(event(2));
        hear(Said::Ended(None));
        assert_eq!(view.cursor().unwrap().after, 0);
        assert!(matches!(
            view.recv().await,
            Some(Said::Event { seq: 1, .. })
        ));
        assert!(matches!(
            view.recv().await,
            Some(Said::Event { seq: 2, .. })
        ));
        assert_eq!(view.cursor().unwrap().after, 2);
        assert!(matches!(view.recv().await, Some(Said::Ended(None))));
        hear(event(3));
        assert!(view.recv().await.is_none());
        assert_eq!(view.inbox.state.lock().unwrap().bytes, 0);
    }

    #[tokio::test]
    async fn a_slow_viewer_is_bounded_and_detaches_without_closing_the_chat() {
        for limits in [
            ViewLimits {
                frames: 1,
                bytes: 1024,
            },
            ViewLimits {
                frames: 20,
                bytes: 1,
            },
        ] {
            let (mut view, hear, mut urgent) = viewer(limits);
            hear(event(1));
            hear(event(2));
            {
                let state = view.inbox.state.lock().unwrap();
                assert_eq!(state.bytes, 0);
                assert!(state.queued.is_empty());
            }
            assert!(
                matches!(view.recv().await, Some(Said::Ended(Some(reason))) if reason == BEHIND)
            );
            let detach: serde_json::Value =
                serde_json::from_str(&urgent.try_recv().unwrap()).unwrap();
            assert_eq!(detach["method"], "session.detach");
            assert!(urgent.try_recv().is_err());
            assert_eq!(view.cursor().unwrap().after, 0);
        }
    }

    #[tokio::test]
    async fn only_delivery_or_an_explicit_snapshot_advances_the_replay_cursor() {
        let (mut view, hear, _) = viewer(ViewLimits::default());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), view.recv())
                .await
                .is_err()
        );
        assert_eq!(view.cursor().unwrap().after, 0);
        hear(event(1));
        hear(event(2));
        hear(event(3));
        assert_eq!(view.cursor().unwrap().after, 0);
        assert!(
            view.covered_through(protocol::Cursor {
                stream: "other".into(),
                after: 2
            })
            .is_err()
        );
        view.covered_through(protocol::Cursor {
            stream: "first".into(),
            after: 2,
        })
        .unwrap();
        assert!(matches!(
            view.recv().await,
            Some(Said::Event { seq: 3, .. })
        ));
        assert!(
            view.covered_through(protocol::Cursor {
                stream: "first".into(),
                after: 2
            })
            .is_err()
        );
        assert_eq!(view.inbox.state.lock().unwrap().bytes, 0);
    }

    #[tokio::test]
    async fn an_event_from_a_different_incarnation_requires_resynchronizing() {
        let (mut view, hear, _) = viewer(ViewLimits::default());
        hear(Said::Event {
            stream: Some("second".into()),
            seq: 1,
            frame: json!({"method": "note"}),
        });
        hear(Said::Ended(None));
        assert!(
            matches!(view.recv().await, Some(Said::Ended(Some(reason))) if reason.contains("stream changed"))
        );
        assert_eq!(view.cursor().unwrap().after, 0);
    }
}
