//! One chat as its clients see it: a numbered stream of what it said, the
//! recent part kept for a client that comes back, and answers routed to whoever asked.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::client::Client;
use crate::{Done, Input, Output};

/// What a returning client can be replayed; older events are gone.
pub(crate) const KEPT_FRAMES: usize = 4096;
const KEPT_BYTES: usize = 8 * 1024 * 1024;
/// The largest frame a chat may say: what a client can read, less what it is wrapped in.
pub(crate) const CHAT_FRAME: usize = wire::MAX_FRAME - 1024;

/// How much may wait to be read by one chat. A chat that is not reading fills
/// this and is refused more; it never holds up the client that asked.
pub(crate) const INPUT_FRAMES: usize = 256;
const INPUT_BYTES: usize = 2 * wire::MAX_FRAME;
const BUSY: &str = "this chat has not read what it was already sent; try again in a moment";
const TOO_LARGE: &str = "this is too large to send to a chat at once";

/// What stops a turn, or answers something the chat itself asked, has a little
/// room of its own, so a chat with everything else waiting can still be told.
pub(crate) const URGENT_FRAMES: usize = 32;
const URGENT_BYTES: usize = 64 * 1024;

pub(crate) struct Session {
    pub(crate) id: String,
    incarnation: String,
    about: Value,
    /// What the chat is asked, in the order it was asked.
    input: mpsc::Sender<String>,
    /// What it is told ahead of that.
    urgent: mpsc::Sender<String>,
    waiting: Arc<AtomicUsize>,
    closing: CancellationToken,
    tied: AtomicBool,
    stream: Mutex<Stream>,
    control: Option<crate::Control>,
}

/// Writes to the chat one request at a time, what is urgent before what is
/// waiting. Dropping its input, when it is told to close or the chat stops
/// taking any, is how a chat is asked to finish.
async fn feed(
    mut input: Input,
    mut asked: mpsc::Receiver<String>,
    mut urgent: mpsc::Receiver<String>,
    waiting: Arc<AtomicUsize>,
    closing: CancellationToken,
) {
    loop {
        let line = tokio::select! {
            biased;
            () = closing.cancelled() => break,
            Some(line) = urgent.recv() => line,
            line = asked.recv() => match line {
                Some(line) => {
                    waiting.fetch_sub(line.len(), Ordering::Relaxed);
                    line
                }
                None => break,
            },
        };
        let written =
            async { input.write_all(line.as_bytes()).await.is_ok() && input.flush().await.is_ok() };
        let written = tokio::select! {
            () = closing.cancelled() => break,
            written = written => written,
        };
        if !written {
            break;
        }
    }
    closing.cancel();
}

#[derive(Default)]
struct Stream {
    seq: u64,
    kept: VecDeque<(u64, Arc<str>)>,
    kept_bytes: usize,
    viewers: HashMap<u64, Client>,
    /// Requests forwarded to the chat: the id it was given, who asked, and their own id.
    asked: HashMap<String, Requested>,
    asks: u64,
}

struct Requested {
    client: Client,
    id: Value,
    presentation: bool,
}

impl Session {
    pub(crate) fn new(
        id: String,
        incarnation: String,
        about: Value,
        input: Input,
        control: Option<crate::Control>,
    ) -> Arc<Self> {
        let (asked, queued) = mpsc::channel(INPUT_FRAMES);
        let (urgent, ahead) = mpsc::channel(URGENT_FRAMES);
        let (waiting, closing) = (Arc::<AtomicUsize>::default(), CancellationToken::new());
        let feeding = feed(input, queued, ahead, Arc::clone(&waiting), closing.clone());
        tokio::spawn(feeding);
        Arc::new(Self {
            id,
            incarnation,
            about,
            input: asked,
            urgent,
            waiting,
            closing,
            tied: AtomicBool::new(false),
            stream: Mutex::default(),
            control,
        })
    }

    fn stream(&self) -> std::sync::MutexGuard<'_, Stream> {
        self.stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn summary(&self) -> Value {
        let stream = self.stream();
        json!({
            "session": self.id,
            "stream": self.incarnation,
            "about": self.about,
            "head": stream.seq,
            "clients": stream.viewers.len(),
        })
    }

    pub(crate) fn cancel_turn(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|control| control(crate::TurnAction::Cancel))
    }

    pub(crate) fn abort_turn(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|control| control(crate::TurnAction::Abort))
    }

    /// Replay and subscription happen under one lock, so nothing is missed or repeated.
    pub(crate) fn attach(
        &self,
        client: &Client,
        after: Option<u64>,
        incarnation: Option<&str>,
    ) -> Result<Value, String> {
        let mut stream = self.stream();
        if self.closing.is_cancelled() {
            return Err("this chat is ending".into());
        }
        let replaced = incarnation.is_some_and(|expected| expected != self.incarnation);
        let after = if replaced {
            stream.seq
        } else {
            after.unwrap_or(stream.seq)
        };
        let oldest = stream
            .kept
            .front()
            .map_or(stream.seq.saturating_add(1), |(seq, _)| *seq);
        // Events older than what is kept, or a cursor from before a restart.
        let gap = replaced || after.saturating_add(1) < oldest || after > stream.seq;
        let mut replayed = 0;
        for (_, line) in stream.kept.iter().filter(|(seq, _)| *seq > after) {
            client.send(line.clone());
            replayed += 1;
        }
        stream.viewers.insert(client.id, client.clone());
        Ok(
            json!({ "session": self.id, "stream": self.incarnation, "head": stream.seq, "replayed": replayed, "gap": gap }),
        )
    }

    pub(crate) fn detach(&self, client: u64) {
        let mut stream = self.stream();
        let unwatched = stream.viewers.remove(&client).is_some() && stream.viewers.is_empty();
        if unwatched && self.tied.load(Ordering::Relaxed) {
            self.close();
        }
    }

    /// Makes this a chat that ends once nobody is left watching it: with its
    /// last client, not with whichever client happened to start it.
    pub(crate) fn tie(&self) {
        self.tied.store(true, Ordering::Relaxed);
    }

    /// For a tied chat whose starter has gone: one nobody ever watched ends now.
    pub(crate) fn close_if_unwatched(&self) -> bool {
        let stream = self.stream();
        if stream.viewers.is_empty() {
            self.close();
            true
        } else {
            false
        }
    }

    /// Asked of this chat itself: an earlier chat of the same id is another chat.
    pub(crate) fn heard_by(&self, client: u64) -> bool {
        self.stream().viewers.contains_key(&client)
    }

    /// Hands a client's request to the chat under an id that cannot collide with another client's.
    ///
    /// Never waits on the chat: the request joins what the chat has yet to read,
    /// or is refused when that is full, so the client's other requests go on.
    ///
    /// What stops a turn or answers the chat's own question is read before
    /// requests still waiting, in the order those were sent. A chat reads it
    /// when it next reads anything: only closing reaches one that has stopped reading.
    pub(crate) fn forward(&self, client: &Client, mut frame: Value) -> Result<(), String> {
        if self.closing.is_cancelled() {
            return Err("this chat has ended".into());
        }
        let ahead = frame["method"].as_str().is_some_and(wire::is_control);
        let mut given = None;
        if let Some(asked) = frame.get("id").cloned() {
            let mut stream = self.stream();
            stream.asks += 1;
            let id = format!("b{}", stream.asks);
            stream.asked.insert(
                id.clone(),
                Requested {
                    client: client.clone(),
                    id: asked,
                    presentation: frame["method"] == "session.presentation",
                },
            );
            frame["id"] = json!(id);
            given = Some(id);
        }
        let mut line = frame.to_string();
        line.push('\n');
        let bytes = line.len();
        let sent = if bytes > wire::MAX_FRAME {
            Err(TOO_LARGE)
        } else if ahead && bytes <= URGENT_BYTES {
            self.urgent.try_send(line).map_err(|refused| match refused {
                mpsc::error::TrySendError::Closed(_) => "this chat has ended",
                mpsc::error::TrySendError::Full(_) => BUSY,
            })
        } else {
            let room = self.waiting.fetch_add(bytes, Ordering::Relaxed) + bytes <= INPUT_BYTES;
            let sent = match room.then(|| self.input.try_send(line)) {
                Some(Ok(())) => Ok(()),
                Some(Err(mpsc::error::TrySendError::Closed(_))) => Err("this chat has ended"),
                Some(Err(mpsc::error::TrySendError::Full(_))) | None => Err(BUSY),
            };
            if sent.is_err() {
                self.waiting.fetch_sub(bytes, Ordering::Relaxed);
            }
            sent
        };
        let Err(refused) = sent else { return Ok(()) };
        if let Some(id) = given {
            self.stream().asked.remove(&id);
        }
        Err(refused.into())
    }

    /// Reaches a chat even when it has stopped reading what it is asked.
    pub(crate) fn close(&self) {
        self.cancel_turn();
        self.closing.cancel();
    }

    /// Reads what the chat says until it ends, then tells whoever is still attached.
    pub(crate) async fn pump(&self, output: Output, done: Done) {
        let mut reader = BufReader::new(output);
        let mut line = Vec::new();
        'chat: loop {
            line.clear();
            // Read the whole valid ACP frame before looking for its id: JSON
            // fields may appear in any order, including error before id. The
            // smaller CHAT_FRAME limit still leaves room for the event envelope.
            let limited = (&mut reader).take(wire::MAX_FRAME as u64 + 1);
            tokio::pin!(limited);
            match limited.read_until(b'\n', &mut line).await {
                Ok(read) if read > 0 => {}
                _ => break,
            }
            // No client could read a frame this large. Whoever it answers is told so, the
            // rest of it is passed over, and the chat goes on.
            if line.len() > CHAT_FRAME {
                let asked = answered(&line).and_then(|given| self.stream().asked.remove(&given));
                if let Some(Requested { client, id, .. }) = asked {
                    client.reply(Some(id), Err(crate::refused(crate::client::TOO_LARGE)));
                }
                while !line.ends_with(b"\n") {
                    line.clear();
                    let rest = (&mut reader).take(64 * 1024);
                    tokio::pin!(rest);
                    if !matches!(rest.read_until(b'\n', &mut line).await, Ok(read) if read > 0) {
                        break 'chat;
                    }
                }
                continue;
            }
            // A line that is not a frame is skipped: stopping here would leave the chat blocked.
            if let Ok(frame) = serde_json::from_slice(&line) {
                self.said(frame);
            }
        }
        // Whatever ended the reading, the chat is told to finish before it is waited for.
        self.close();
        let error = match done.await {
            Ok(()) => Value::Null,
            Err(error) => json!(error),
        };
        self.publish(json!({ "method": "session.ended", "params": { "error": error } }));
    }

    fn said(&self, mut frame: Value) {
        let answers = frame.get("method").is_none();
        let given = frame.get("id").and_then(Value::as_str).map(str::to_owned);
        match (answers, given) {
            (true, Some(given)) => {
                let mut stream = self.stream();
                if let Some(asked) = stream.asked.remove(&given) {
                    if asked.presentation && frame["result"].is_object() {
                        // The chat enqueues this snapshot under the same lock
                        // as its projection updates. Its preceding output has
                        // now been published; subsequent events are not covered.
                        frame["result"]["cursor"] =
                            json!({ "stream": self.incarnation, "after": stream.seq });
                    }
                    frame["id"] = asked.id;
                    asked.client.send(line(&frame));
                }
            }
            _ => self.publish(frame),
        }
    }

    fn publish(&self, frame: Value) {
        let mut stream = self.stream();
        stream.seq += 1;
        let seq = stream.seq;
        let event = line(&json!({
            "jsonrpc": "2.0",
            "method": "session.event",
            "params": { "session": self.id, "stream": self.incarnation, "seq": seq, "frame": frame },
        }));
        stream.kept_bytes += event.len();
        stream.kept.push_back((seq, event.clone()));
        while stream.kept.len() > KEPT_FRAMES || stream.kept_bytes > KEPT_BYTES {
            match stream.kept.pop_front() {
                Some((_, dropped)) => stream.kept_bytes -= dropped.len(),
                None => break,
            }
        }
        let watched = stream.viewers.len();
        stream
            .viewers
            .retain(|_, viewer| viewer.send(event.clone()));
        // A client dropped for falling behind has left like any other.
        let unwatched = watched > 0 && stream.viewers.is_empty();
        drop(stream);
        if unwatched && self.tied.load(Ordering::Relaxed) {
            self.close();
        }
    }
}

/// The id a frame carries, read from the start of one too large to read whole.
fn answered(start: &[u8]) -> Option<String> {
    struct Id<'a>(&'a mut Option<String>);
    impl<'de> serde::de::Visitor<'de> for Id<'_> {
        type Value = ();

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a frame")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut frame: A) -> Result<(), A::Error> {
            while let Some(key) = frame.next_key::<String>()? {
                if key == "id" {
                    *self.0 = frame.next_value::<Value>()?.as_str().map(str::to_owned);
                    break;
                }
                frame.next_value::<serde::de::IgnoredAny>()?;
            }
            Ok(())
        }
    }
    let mut id = None;
    // It ends mid-frame, so reading it fails; the id is kept from before it does.
    let _ = serde::Deserializer::deserialize_map(
        &mut serde_json::Deserializer::from_slice(start),
        Id(&mut id),
    );
    id
}

fn line(frame: &Value) -> Arc<str> {
    let mut text = frame.to_string();
    text.push('\n');
    text.into()
}
