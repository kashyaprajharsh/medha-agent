//! One process that holds many chats and serves any number of clients. A chat
//! is a peer that speaks JSON-RPC frames over a byte stream in each direction;
//! the backend numbers what it says, keeps the recent part, and gives every
//! attached client the same stream. A client that leaves does not stop a chat.

mod client;
mod session;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

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
    /// A current-turn cancellation path independent of a congested input pipe.
    /// `true` means it reached a turn or pending human gate immediately.
    pub cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
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

    /// A nonblocking resource snapshot. Busy resources may report `null`
    /// instead of waiting for the work that the health check is diagnosing.
    fn resources(&self) -> Value {
        Value::Null
    }
}

pub struct Backend<C> {
    chats: C,
    version: String,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Chats being resumed, so two clients cannot start one chat twice.
    resuming: Mutex<HashSet<String>>,
    clients: AtomicU64,
    activity: Mutex<Activity>,
    stopped: CancellationToken,
    max_chats: usize,
}

struct Activity {
    clients: HashMap<u64, Arc<AtomicUsize>>,
    work: usize,
    stopping: bool,
    last_busy: Instant,
    starts: usize,
}

type Failure = (i64, String);

fn refused(message: impl Into<String>) -> Failure {
    (-32001, message.into())
}

impl<C: Chats> Backend<C> {
    pub fn new(chats: C, version: impl Into<String>) -> Arc<Self> {
        Self::with_chat_limit(chats, version, 64)
    }

    pub fn with_chat_limit(chats: C, version: impl Into<String>, max_chats: usize) -> Arc<Self> {
        assert!(max_chats > 0);
        Arc::new(Self {
            chats,
            version: version.into(),
            sessions: Mutex::default(),
            resuming: Mutex::default(),
            clients: AtomicU64::new(1),
            activity: Mutex::new(Activity {
                clients: HashMap::new(),
                work: 0,
                stopping: false,
                last_busy: Instant::now(),
                starts: 0,
            }),
            stopped: CancellationToken::new(),
            max_chats,
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

    pub fn chats(&self) -> &C {
        &self.chats
    }

    pub fn identity(&self) -> Value {
        json!({"backend": self.version, "protocol": PROTOCOL,
            "build": wire::BUILD_ID, "capabilities": wire::CAPABILITIES})
    }

    pub fn status(&self) -> Value {
        let activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        json!({"identity": self.identity(), "stopping": activity.stopping,
            "clients": activity.clients.len(), "in_flight": activity.work,
            "queued_bytes": activity.clients.values().map(|bytes| bytes.load(Ordering::Relaxed)).sum::<usize>(),
            "chats": self.live(), "starting_chats": activity.starts, "max_chats": self.max_chats,
            "resources": self.chats.resources()})
    }

    pub async fn stopping(&self) {
        self.stopped.cancelled().await;
    }

    pub fn begin_stop(&self) {
        self.activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stopping = true;
        self.stopped.cancel();
    }

    pub fn is_drained(&self) -> bool {
        let activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        activity.work == 0 && activity.clients.is_empty() && self.live() == 0
    }

    /// Idle expiry and upgrades use the same atomic admission barrier. No work
    /// can start after the decision to stop, even if its client stays connected.
    pub fn stop_if_idle(&self, idle: Duration) -> bool {
        let mut activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !activity.clients.is_empty() || activity.work > 0 || self.live() > 0 {
            activity.last_busy = Instant::now();
            return false;
        }
        if activity.last_busy.elapsed() < idle {
            return false;
        }
        activity.stopping = true;
        self.stopped.cancel();
        true
    }

    fn upgrade(&self, expected: Option<&str>) -> Result<Value, Failure> {
        let mut activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if expected != Some(wire::BUILD_ID) {
            return Err(refused(
                "backend identity changed; reconnect before upgrading",
            ));
        }
        if self.live() > 0 || activity.work > 0 {
            return Err(refused(
                "This backend has active chats or requests. Finish or close them before upgrading Medha.",
            ));
        }
        activity.stopping = true;
        self.stopped.cancel();
        Ok(json!({"stopping": true}))
    }

    fn admit_work(self: &Arc<Self>) -> Result<Working<C>, Failure> {
        let mut activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if activity.stopping {
            return Err(refused("Medha is stopping. Reconnect in a moment."));
        }
        if activity.work >= 128 {
            return Err(refused(
                "Medha has too many requests waiting. Try again in a moment.",
            ));
        }
        activity.work += 1;
        activity.last_busy = Instant::now();
        Ok(Working(Arc::clone(self)))
    }

    /// Tells every chat to finish, as a backend that is stopping does. Each is
    /// gone from `live` once it has.
    pub fn close_all(&self) {
        let sessions: Vec<Arc<Session>> = self.table().values().cloned().collect();
        for session in sessions {
            session.close();
        }
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
        // Starts are included: a simultaneous burst must not bypass the cap
        // before its chats appear in the live table.
        let _starting = self.starting()?;
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
            let session = Session::new(opened.session, opened.about, opened.input, opened.cancel);
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

    fn starting(self: &Arc<Self>) -> Result<Starting<C>, Failure> {
        let mut activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.live() + activity.starts >= self.max_chats {
            return Err(refused(
                "Medha has reached its active-chat limit. Close or sleep a chat before starting another.",
            ));
        }
        activity.starts += 1;
        Ok(Starting(Arc::clone(self)))
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

struct Starting<C>(Arc<Backend<C>>);
impl<C> Drop for Starting<C> {
    fn drop(&mut self) {
        self.0
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .starts -= 1;
    }
}

struct Working<C>(Arc<Backend<C>>);
impl<C> Drop for Working<C> {
    fn drop(&mut self) {
        let mut activity = self
            .0
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        activity.work -= 1;
        activity.last_busy = Instant::now();
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
