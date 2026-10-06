//! One set of remote MCP connections shared by every chat. A host process owns
//! the connections; each chat attaches over a private local channel and sees
//! the host's servers as its own. Frames are newline-delimited JSON, and each
//! side proves it holds the token without the token crossing the channel.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    CallOutput, Catalog, Error, McpManager, McpToolSpec, Screen, ServerConfig, ServerState,
    ServerStatus, TOOL_PREFIX, UrlSink,
};

const ROLES: wire::Roles = wire::Roles {
    host: "host",
    guest: "chat",
};

/// Where a host listens: a socket path, or a pipe name on Windows.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub address: String,
    pub token: String,
}

#[derive(Serialize, Deserialize)]
struct Request {
    id: u64,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub(crate) struct Snapshot {
    pub(crate) servers: Vec<ServerStatus>,
    pub(crate) catalogs: HashMap<String, Catalog>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireError {
    Disabled,
    UnknownServer {
        server: String,
    },
    Superseded {
        server: String,
    },
    NotReady {
        server: String,
        state: ServerState,
        detail: Option<String>,
    },
    UnknownTool {
        server: String,
        tool: String,
    },
    BadArguments {
        tool: String,
        reason: String,
    },
    NeedsAuth {
        server: String,
    },
    NeedsToken {
        server: String,
    },
    Auth {
        message: String,
    },
    Timeout {
        millis: u64,
    },
    /// The host does not own this server; the chat should run it itself.
    NotShared,
    Other {
        message: String,
    },
}

impl From<Error> for WireError {
    fn from(error: Error) -> Self {
        match error {
            Error::Disabled => Self::Disabled,
            Error::UnknownServer(server) => Self::UnknownServer { server },
            Error::Superseded(server) => Self::Superseded { server },
            Error::ServerNotReady {
                server,
                state,
                detail,
            } => Self::NotReady {
                server,
                state,
                detail,
            },
            Error::UnknownTool { server, tool } => Self::UnknownTool { server, tool },
            Error::BadArguments { tool, reason } => Self::BadArguments { tool, reason },
            Error::NeedsAuth(server) => Self::NeedsAuth { server },
            Error::NeedsToken(server) => Self::NeedsToken { server },
            Error::Auth(message) => Self::Auth { message },
            Error::Timeout(limit) => Self::Timeout {
                millis: limit.as_millis() as u64,
            },
            other => Self::Other {
                message: other.to_string(),
            },
        }
    }
}

impl From<WireError> for Error {
    fn from(error: WireError) -> Self {
        match error {
            WireError::Disabled => Error::Disabled,
            WireError::UnknownServer { server } => Error::UnknownServer(server),
            WireError::Superseded { server } => Error::Superseded(server),
            WireError::NotReady {
                server,
                state,
                detail,
            } => Error::ServerNotReady {
                server,
                state,
                detail,
            },
            WireError::UnknownTool { server, tool } => Error::UnknownTool { server, tool },
            WireError::BadArguments { tool, reason } => Error::BadArguments { tool, reason },
            WireError::NeedsAuth { server } => Error::NeedsAuth(server),
            WireError::NeedsToken { server } => Error::NeedsToken(server),
            WireError::Auth { message } => Error::Auth(message),
            WireError::Timeout { millis } => Error::Timeout(Duration::from_millis(millis)),
            WireError::NotShared => Error::Protocol("server is not shared".into()),
            WireError::Other { message } => Error::Protocol(message),
        }
    }
}

type Writer = Box<dyn AsyncWrite + Unpin + Send>;
type Pending = HashMap<u64, oneshot::Sender<Result<Value, WireError>>>;

async fn send(writer: &Mutex<Option<Writer>>, frame: &Value) -> bool {
    let mut guard = writer.lock().await;
    let Some(out) = guard.as_mut() else {
        return false;
    };
    wire::write_frame(out, frame).await
}

/// A chat's attachment to the host: a mirror of the shared servers, kept
/// current by the host's snapshots, and the requests that act on them.
pub struct Link {
    endpoint: Endpoint,
    writer: Mutex<Option<Writer>>,
    pending: SyncMutex<Pending>,
    mirror: RwLock<Snapshot>,
    sinks: SyncMutex<HashMap<String, UrlSink>>,
    /// Servers handed to the host whose first snapshot may not have arrived.
    claimed: SyncMutex<std::collections::HashSet<String>>,
    next: AtomicU64,
    on_change: Box<dyn Fn() + Send + Sync>,
}

impl Link {
    /// Attach to a running host. Fails fast when none answers, so the caller
    /// can run its servers itself instead.
    pub async fn attach(
        endpoint: Endpoint,
        on_change: impl Fn() + Send + Sync + 'static,
    ) -> Result<Arc<Self>, Error> {
        let link = Arc::new(Self {
            endpoint,
            writer: Mutex::new(None),
            pending: SyncMutex::new(HashMap::new()),
            mirror: RwLock::new(Snapshot::default()),
            sinks: SyncMutex::new(HashMap::new()),
            claimed: SyncMutex::new(std::collections::HashSet::new()),
            next: AtomicU64::new(1),
            on_change: Box::new(on_change),
        });
        Arc::clone(&link).open().await?;
        Ok(link)
    }

    // Boxed: reconnecting reopens from inside the listener that `open` spawns.
    fn open(self: Arc<Self>) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send>> {
        Box::pin(self.open_now())
    }

    async fn open_now(self: Arc<Self>) -> Result<(), Error> {
        let address = &self.endpoint.address;
        let stream = wire::connect(address).await.map_err(|error| {
            Error::Protocol(format!("no connection host at {address}: {error}"))
        })?;
        let (read, mut write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read);
        let reply = wire::greet(&mut reader, &mut write, &self.endpoint.token, ROLES)
            .await
            .map_err(|refusal| {
                Error::Protocol(
                    match refusal {
                        wire::Refusal::Closed => "the connection host closed the channel",
                        wire::Refusal::Silent => "the connection host did not answer",
                        wire::Refusal::Unproven => {
                            "the connection host could not prove it is Medha's"
                        }
                    }
                    .into(),
                )
            })?;
        *self.writer.lock().await = Some(Box::new(write));
        let snapshot: Snapshot = serde_json::from_value(reply["result"].clone())
            .map_err(|_| Error::Protocol("the connection host refused this chat".into()))?;
        *self.mirror.write().expect("hub mirror lock") = snapshot;
        (self.on_change)();
        tokio::spawn(Arc::clone(&self).listen(reader));
        Ok(())
    }

    async fn listen<R: AsyncRead + Unpin + Send + 'static>(
        self: Arc<Self>,
        mut reader: BufReader<R>,
    ) {
        while let Some(frame) = wire::read_frame(&mut reader).await {
            if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                let waiter = self.pending.lock().expect("hub pending lock").remove(&id);
                if let Some(waiter) = waiter {
                    let outcome = match frame.get("error") {
                        Some(error) => Err(serde_json::from_value(error.clone()).unwrap_or(
                            WireError::Other {
                                message: "malformed error from the connection host".into(),
                            },
                        )),
                        None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = waiter.send(outcome);
                }
                continue;
            }
            match frame["method"].as_str() {
                Some("state") => {
                    if let Ok(snapshot) = serde_json::from_value(frame["params"].clone()) {
                        *self.mirror.write().expect("hub mirror lock") = snapshot;
                        (self.on_change)();
                    }
                }
                Some("auth_url") => {
                    let server = frame["params"]["server"].as_str().unwrap_or_default();
                    let url = frame["params"]["url"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    if let Some(sink) = self.sinks.lock().expect("hub sinks lock").get(server) {
                        let _ = sink.send(url);
                    }
                }
                _ => {}
            }
        }
        self.lost().await;
    }

    /// The host went away: fail what was waiting, show the servers as
    /// reconnecting, and keep trying until a host answers again.
    async fn lost(self: Arc<Self>) {
        *self.writer.lock().await = None;
        for (_, waiter) in self.pending.lock().expect("hub pending lock").drain() {
            let _ = waiter.send(Err(WireError::Other {
                message: "Medha's connection host restarted; try again".into(),
            }));
        }
        {
            let mut mirror = self.mirror.write().expect("hub mirror lock");
            for server in &mut mirror.servers {
                server.state = ServerState::Reconnecting;
                server.detail = Some("reconnecting to Medha's connection host".into());
            }
        }
        (self.on_change)();
        let mut delay = Duration::from_millis(250);
        loop {
            tokio::time::sleep(delay).await;
            if Arc::strong_count(&self) == 1 || Arc::clone(&self).open().await.is_ok() {
                return;
            }
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, WireError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("hub pending lock")
            .insert(id, tx);
        if !send(
            &self.writer,
            &json!({"id": id, "method": method, "params": params}),
        )
        .await
        {
            self.pending.lock().expect("hub pending lock").remove(&id);
            return Err(WireError::Other {
                message: "Medha's connection host is restarting; try again".into(),
            });
        }
        rx.await.unwrap_or(Err(WireError::Other {
            message: "Medha's connection host restarted; try again".into(),
        }))
    }

    pub fn owns(&self, server: &str) -> bool {
        self.claimed
            .lock()
            .expect("hub claimed lock")
            .contains(server)
            || self
                .mirror
                .read()
                .expect("hub mirror lock")
                .servers
                .iter()
                .any(|status| status.server == server)
    }

    pub fn statuses(&self) -> Vec<ServerStatus> {
        self.mirror.read().expect("hub mirror lock").servers.clone()
    }

    pub fn tool_specs(&self) -> Vec<McpToolSpec> {
        self.mirror
            .read()
            .expect("hub mirror lock")
            .catalogs
            .values()
            .flat_map(|catalog| catalog.exposed.iter().cloned())
            .collect()
    }

    pub fn server_tools(&self, server: &str) -> Vec<(String, bool)> {
        let mirror = self.mirror.read().expect("hub mirror lock");
        let Some(catalog) = mirror.catalogs.get(server) else {
            return Vec::new();
        };
        let prefix = format!("{TOOL_PREFIX}{server}__");
        let mut tools: Vec<(String, bool)> = catalog
            .exposed
            .iter()
            .filter_map(|spec| spec.name.strip_prefix(&prefix))
            .map(|name| (name.to_string(), true))
            .chain(catalog.hidden.iter().map(|name| (name.clone(), false)))
            .collect();
        tools.sort_by(|a, b| a.0.cmp(&b.0));
        tools
    }

    pub fn state(&self, server: &str) -> Option<ServerState> {
        self.mirror
            .read()
            .expect("hub mirror lock")
            .servers
            .iter()
            .find(|status| status.server == server)
            .map(|status| status.state)
    }

    pub async fn call(
        &self,
        qualified: &str,
        args: &Value,
        screen: bool,
    ) -> Result<CallOutput, Error> {
        let output = self
            .request(
                "call",
                json!({"tool": qualified, "args": args, "screen": screen}),
            )
            .await?;
        serde_json::from_value(output).map_err(|error| Error::Protocol(error.to_string()))
    }

    pub async fn read_screen(&self, server: &str, uri: &str) -> Result<Screen, Error> {
        let page = self
            .request("screen", json!({"server": server, "uri": uri}))
            .await?;
        serde_json::from_value(page).map_err(|error| Error::Protocol(error.to_string()))
    }

    /// `connect` is a person asking, which may retry or enable the server.
    /// `Ok(None)` means the host does not share it and the chat runs it itself.
    pub async fn ensure(&self, server: &str, connect: bool) -> Result<Option<ServerStatus>, Error> {
        let outcome = self
            .request("ensure", json!({"server": server, "connect": connect}))
            .await;
        if !matches!(outcome, Err(WireError::NotShared)) {
            self.claimed
                .lock()
                .expect("hub claimed lock")
                .insert(server.to_string());
        }
        match outcome {
            Ok(status) => serde_json::from_value(status)
                .map(Some)
                .map_err(|error| Error::Protocol(error.to_string())),
            Err(WireError::NotShared) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn remove(&self, server: &str) -> Result<(), Error> {
        self.claimed
            .lock()
            .expect("hub claimed lock")
            .remove(server);
        self.request("remove", json!({"server": server})).await?;
        Ok(())
    }

    pub async fn set_disabled(&self, server: &str, disabled: bool) -> Result<ServerStatus, Error> {
        let status = self
            .request(
                "set_disabled",
                json!({"server": server, "disabled": disabled}),
            )
            .await?;
        serde_json::from_value(status).map_err(|error| Error::Protocol(error.to_string()))
    }

    /// Sign-in runs on the host, which owns the credentials; its browser links
    /// are relayed to `announce` as they arrive.
    pub async fn authorize(&self, server: &str, announce: &UrlSink) -> Result<ServerStatus, Error> {
        self.sinks
            .lock()
            .expect("hub sinks lock")
            .insert(server.to_string(), announce.clone());
        let outcome = self.request("authorize", json!({"server": server})).await;
        self.sinks.lock().expect("hub sinks lock").remove(server);
        serde_json::from_value(outcome?).map_err(|error| Error::Protocol(error.to_string()))
    }
}

/// Which servers the host may run: an id from the user's own config, resolved
/// by the host itself. A chat can name a server, never define one. An error is
/// a definition that could not be read, a saved key among it: the server the
/// host already runs must then stay as it is.
pub type Resolve = Arc<dyn Fn(&str) -> Result<Option<ServerConfig>, String> + Send + Sync>;

/// Serve one attached chat until it disconnects.
pub async fn serve<S>(manager: McpManager, stream: S, token: Arc<str>, resolve: Resolve)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let tasks = TaskTracker::new();
    let stop = CancellationToken::new();
    serve_tracked(
        manager,
        stream,
        token,
        resolve,
        tasks.clone(),
        stop.clone(),
        Arc::new(tokio::sync::Semaphore::new(16)),
    )
    .await;
    stop.cancel();
    tasks.close();
    tasks.wait().await;
}

async fn serve_tracked<S>(
    manager: McpManager,
    stream: S,
    token: Arc<str>,
    resolve: Resolve,
    tasks: TaskTracker,
    stop: CancellationToken,
    admission: Arc<tokio::sync::Semaphore>,
) where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let Some(Some(admitted)) = stop
        .run_until_cancelled(wire::admit(&mut reader, &mut write, &token, ROLES))
        .await
    else {
        return;
    };
    let writer: Arc<Mutex<Option<Writer>>> = Arc::new(Mutex::new(Some(Box::new(write))));
    let mut changes = manager.subscribe();
    if stop
        .run_until_cancelled(send(
            &writer,
            &json!({"id": admitted, "result": manager.snapshot().await}),
        ))
        .await
        != Some(true)
    {
        return;
    }
    let pusher = tasks.spawn({
        let (manager, writer) = (manager.clone(), Arc::clone(&writer));
        let stop = stop.clone();
        async move {
            while matches!(
                stop.run_until_cancelled(changes.changed()).await,
                Some(Ok(()))
            ) {
                let frame = json!({"method": "state", "params": manager.snapshot().await});
                if stop.run_until_cancelled(send(&writer, &frame)).await != Some(true) {
                    return;
                }
            }
        }
    });
    while let Some(Some(frame)) = stop
        .run_until_cancelled(wire::read_frame(&mut reader))
        .await
    {
        let Ok(request) = serde_json::from_value::<Request>(frame) else {
            continue;
        };
        let permit = match Arc::clone(&admission).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let reply = json!({"id": request.id, "error": WireError::Other { message: "The MCP host is busy. Try again in a moment.".into() }});
                if stop.run_until_cancelled(send(&writer, &reply)).await != Some(true) {
                    break;
                }
                continue;
            }
        };
        let (manager, writer, resolve) =
            (manager.clone(), Arc::clone(&writer), Arc::clone(&resolve));
        let stop = stop.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let id = request.id;
            let Some(outcome) = stop
                .run_until_cancelled(handle(&manager, request, &writer, &resolve))
                .await
            else {
                return;
            };
            let frame = match outcome {
                Ok(result) => json!({"id": id, "result": result}),
                Err(error) => json!({"id": id, "error": error}),
            };
            stop.run_until_cancelled(send(&writer, &frame)).await;
        });
    }
    stop.cancel();
    pusher.abort();
}

async fn handle(
    manager: &McpManager,
    request: Request,
    writer: &Arc<Mutex<Option<Writer>>>,
    resolve: &Resolve,
) -> Result<Value, WireError> {
    let text = |key: &str| request.params[key].as_str().unwrap_or_default().to_string();
    let server = text("server");
    let status = |status: ServerStatus| serde_json::to_value(status).unwrap_or(Value::Null);
    match request.method.as_str() {
        "call" => {
            // An older chat sends no flag, and is the model calling.
            let screen = request.params["screen"].as_bool().unwrap_or(false);
            let output = manager
                .call_as(&text("tool"), &request.params["args"], screen)
                .await?;
            Ok(serde_json::to_value(output).unwrap_or(Value::Null))
        }
        "screen" => {
            let page = manager.read_screen(&server, &text("uri")).await?;
            Ok(serde_json::to_value(page).unwrap_or(Value::Null))
        }
        "ensure" => {
            let config = resolve(&server)
                .map_err(|message| WireError::Other { message })?
                .ok_or(WireError::NotShared)?;
            let connect = request.params["connect"].as_bool().unwrap_or(false);
            Ok(status(manager.ensure(config, connect).await?))
        }
        "remove" => {
            manager.remove_server(&server).await?;
            Ok(Value::Null)
        }
        "set_disabled" => {
            let disabled = request.params["disabled"].as_bool().unwrap_or(false);
            Ok(status(manager.set_disabled(&server, disabled).await?))
        }
        "authorize" => {
            let (sink, mut links) = mpsc::unbounded_channel::<String>();
            let outcome = {
                let authorizing = manager.authorize(&server, &sink);
                tokio::pin!(authorizing);
                loop {
                    tokio::select! {
                        outcome = &mut authorizing => break outcome,
                        Some(url) = links.recv() => {
                            let frame = json!({"method": "auth_url", "params": {"server": server, "url": url}});
                            send(writer, &frame).await;
                        }
                    }
                }
            };
            drop(sink);
            while let Ok(url) = links.try_recv() {
                send(
                    writer,
                    &json!({"method": "auth_url", "params": {"server": server, "url": url}}),
                )
                .await;
            }
            Ok(status(outcome?))
        }
        _ => Err(WireError::Other {
            message: format!("unknown request '{}'", request.method),
        }),
    }
}

/// Accept chats on `address` until the process exits.
pub async fn run_host(
    manager: McpManager,
    address: &str,
    token: Arc<str>,
    resolve: Resolve,
) -> std::io::Result<()> {
    run_host_until(manager, address, token, resolve, std::future::pending()).await
}

/// Stops admission and joins every guest/request before returning. Blocking
/// credential resolution remains tracked until it really finishes.
pub async fn run_host_until(
    manager: McpManager,
    address: &str,
    token: Arc<str>,
    resolve: Resolve,
    stopped: impl Future<Output = ()>,
) -> std::io::Result<()> {
    let tasks = TaskTracker::new();
    let stop = CancellationToken::new();
    let clients = tasks.clone();
    let ending = stop.clone();
    let admission = Arc::new(tokio::sync::Semaphore::new(16));
    let guests = Arc::new(tokio::sync::Semaphore::new(128));
    let serving = wire::listen(address, move |stream| {
        let Ok(guest) = Arc::clone(&guests).try_acquire_owned() else {
            return;
        };
        let child = ending.child_token();
        let serving = serve_tracked(
            manager.clone(),
            stream,
            Arc::clone(&token),
            Arc::clone(&resolve),
            clients.clone(),
            child,
            Arc::clone(&admission),
        );
        clients.spawn(async move {
            let _guest = guest;
            serving.await;
        });
    });
    let result = tokio::select! {
        result = serving => result,
        () = stopped => Ok(()),
    };
    stop.cancel();
    tasks.close();
    tasks.wait().await;
    result
}

#[cfg(test)]
#[path = "hub_tests.rs"]
mod tests;
