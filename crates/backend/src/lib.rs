//! One process that holds many chats and serves any number of clients. A chat
//! is a peer that speaks JSON-RPC frames over a byte stream in each direction;
//! the backend numbers what it says, keeps the recent part, and gives every
//! attached client the same stream. A client that leaves does not stop a chat.

mod client;
mod session;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};

use session::Session;

pub const PROTOCOL: u64 = 1;

pub type Input = Box<dyn AsyncWrite + Send + Unpin>;
pub type Output = Box<dyn AsyncRead + Send + Unpin>;
pub type Done = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// A chat that has started and is ready for its first frame.
pub struct Opened {
    /// The id it is resumed by, so it survives a restart of the backend.
    pub session: String,
    /// What a client listing chats is told about it.
    pub about: Value,
    pub input: Input,
    pub output: Output,
    pub done: Done,
}

/// Starts chats. `params` is what the client sent with `session.create`.
#[async_trait::async_trait]
pub trait Chats: Send + Sync + 'static {
    async fn open(&self, params: &Value) -> Result<Opened, String>;

    /// A request about a folder, not a chat: its history, settings, extensions.
    /// `request` is the client's whole frame, which names the folder.
    async fn about_folder(&self, _request: &Value) -> Result<Value, String> {
        Err("unknown method".into())
    }
}

pub struct Backend<C> {
    chats: C,
    version: String,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Chats being resumed, so two clients cannot start one chat twice.
    resuming: Mutex<HashSet<String>>,
    clients: AtomicU64,
}

type Failure = (i64, String);

fn refused(message: impl Into<String>) -> Failure {
    (-32001, message.into())
}

impl<C: Chats> Backend<C> {
    pub fn new(chats: C, version: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            chats,
            version: version.into(),
            sessions: Mutex::default(),
            resuming: Mutex::default(),
            clients: AtomicU64::new(1),
        })
    }

    /// Serves one client until it disconnects. Its proof of identity comes first, elsewhere.
    pub async fn serve<R, W>(self: &Arc<Self>, reader: R, writer: W)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        client::serve(self, reader, writer).await;
    }

    pub fn live(&self) -> usize {
        self.table().len()
    }

    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn find(&self, id: &str) -> Result<Arc<Session>, Failure> {
        self.table()
            .get(id)
            .cloned()
            .ok_or_else(|| refused("no such session"))
    }

    fn list(&self) -> Value {
        let sessions: Vec<Value> = self.table().values().map(|s| s.summary()).collect();
        json!({ "sessions": sessions })
    }

    async fn create(self: &Arc<Self>, params: &Value) -> Result<Value, Failure> {
        let resume = params.get("resume").and_then(Value::as_str);
        let _claim = match resume {
            Some(id) => Some(self.claim(id)?),
            None => None,
        };
        let opened = self.chats.open(params).await.map_err(refused)?;
        let session = {
            let mut table = self.table();
            // Dropping what was opened closes its input, which ends that chat.
            if table.contains_key(&opened.session) {
                return Err(refused("this chat is already live; attach to it"));
            }
            let session = Session::new(opened.session, opened.about, opened.input);
            table.insert(session.id.clone(), session.clone());
            session
        };
        let reply = session.summary();
        let backend = Arc::clone(self);
        tokio::spawn(async move {
            session.pump(opened.output, opened.done).await;
            backend.table().remove(&session.id);
        });
        Ok(reply)
    }

    /// Refuses a chat that is live, or being resumed by someone else right now.
    fn claim(self: &Arc<Self>, id: &str) -> Result<Claim<C>, Failure> {
        if self.table().contains_key(id) {
            return Err(refused("this chat is already live; attach to it"));
        }
        let mut resuming = self
            .resuming
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !resuming.insert(id.to_string()) {
            return Err(refused("this chat is already being resumed"));
        }
        Ok(Claim {
            backend: Arc::clone(self),
            id: id.to_string(),
        })
    }
}

struct Claim<C> {
    backend: Arc<Backend<C>>,
    id: String,
}

impl<C> Drop for Claim<C> {
    fn drop(&mut self) {
        self.backend
            .resuming
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
