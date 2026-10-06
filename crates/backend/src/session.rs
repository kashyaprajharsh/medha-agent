//! One chat as its clients see it: a numbered stream of what it said, the
//! recent part kept for a client that comes back, and answers routed to whoever asked.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::client::Client;
use crate::{Done, Input, Output};

/// What a returning client can be replayed; older events are gone.
pub(crate) const KEPT_FRAMES: usize = 4096;
const KEPT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct Session {
    pub(crate) id: String,
    about: Value,
    input: tokio::sync::Mutex<Option<Input>>,
    stream: Mutex<Stream>,
}

#[derive(Default)]
struct Stream {
    seq: u64,
    kept: VecDeque<(u64, Arc<str>)>,
    kept_bytes: usize,
    viewers: HashMap<u64, Client>,
    /// Requests forwarded to the chat: the id it was given, who asked, and their own id.
    asked: HashMap<String, (Client, Value)>,
    asks: u64,
}

impl Session {
    pub(crate) fn new(id: String, about: Value, input: Input) -> Arc<Self> {
        Arc::new(Self {
            id,
            about,
            input: tokio::sync::Mutex::new(Some(input)),
            stream: Mutex::default(),
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
            "about": self.about,
            "head": stream.seq,
            "clients": stream.viewers.len(),
        })
    }

    /// Replay and subscription happen under one lock, so nothing is missed or repeated.
    pub(crate) fn attach(&self, client: &Client, after: Option<u64>) -> Value {
        let mut stream = self.stream();
        let after = after.unwrap_or(stream.seq);
        let oldest = stream.kept.front().map_or(stream.seq + 1, |(seq, _)| *seq);
        // Events older than what is kept, or a cursor from before a restart.
        let gap = after.saturating_add(1) < oldest || after > stream.seq;
        let mut replayed = 0;
        for (_, line) in stream.kept.iter().filter(|(seq, _)| *seq > after) {
            client.send(line.clone());
            replayed += 1;
        }
        stream.viewers.insert(client.id, client.clone());
        json!({ "session": self.id, "head": stream.seq, "replayed": replayed, "gap": gap })
    }

    pub(crate) fn detach(&self, client: u64) {
        self.stream().viewers.remove(&client);
    }

    /// Asked of this chat itself: an earlier chat of the same id is another chat.
    pub(crate) fn heard_by(&self, client: u64) -> bool {
        self.stream().viewers.contains_key(&client)
    }

    /// Hands a client's request to the chat under an id that cannot collide with another client's.
    pub(crate) async fn forward(&self, client: &Client, mut frame: Value) -> Result<(), String> {
        let mut given = None;
        if let Some(asked) = frame.get("id").cloned() {
            let mut stream = self.stream();
            stream.asks += 1;
            let id = format!("b{}", stream.asks);
            stream.asked.insert(id.clone(), (client.clone(), asked));
            frame["id"] = json!(id);
            given = Some(id);
        }
        let mut line = frame.to_string();
        line.push('\n');
        let mut input = self.input.lock().await;
        let sent = match input.as_mut() {
            Some(pipe) => {
                pipe.write_all(line.as_bytes()).await.is_ok() && pipe.flush().await.is_ok()
            }
            None => false,
        };
        if !sent {
            if let Some(id) = given {
                self.stream().asked.remove(&id);
            }
            return Err("this chat has ended".into());
        }
        Ok(())
    }

    /// Closing its input is how a chat is asked to finish.
    pub(crate) async fn close(&self) {
        self.input.lock().await.take();
    }

    /// Reads what the chat says until it ends, then tells whoever is still attached.
    pub(crate) async fn pump(&self, output: Output, done: Done) {
        let mut reader = BufReader::new(output);
        let mut line = Vec::new();
        loop {
            line.clear();
            let limited = (&mut reader).take(wire::MAX_FRAME as u64 + 1);
            tokio::pin!(limited);
            match limited.read_until(b'\n', &mut line).await {
                Ok(read) if read > 0 && line.len() <= wire::MAX_FRAME => {}
                _ => break,
            }
            // A line that is not a frame is skipped: stopping here would leave the chat blocked.
            if let Ok(frame) = serde_json::from_slice(&line) {
                self.said(frame);
            }
        }
        // Whatever ended the reading, the chat is told to finish before it is waited for.
        self.close().await;
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
                let asked = self.stream().asked.remove(&given);
                if let Some((client, id)) = asked {
                    frame["id"] = id;
                    client.send(line(&frame));
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
            "params": { "session": self.id, "seq": seq, "frame": frame },
        }));
        stream.kept_bytes += event.len();
        stream.kept.push_back((seq, event.clone()));
        while stream.kept.len() > KEPT_FRAMES || stream.kept_bytes > KEPT_BYTES {
            match stream.kept.pop_front() {
                Some((_, dropped)) => stream.kept_bytes -= dropped.len(),
                None => break,
            }
        }
        stream
            .viewers
            .retain(|_, viewer| viewer.send(event.clone()));
    }
}

fn line(frame: &Value) -> Arc<str> {
    let mut text = frame.to_string();
    text.push('\n');
    text.into()
}
