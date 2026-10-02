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
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::{
    CallOutput, Catalog, Error, McpManager, McpToolSpec, Screen, ServerConfig, ServerState,
    ServerStatus, TOOL_PREFIX, UrlSink,
};

const MAX_FRAME: usize = 16 * 1024 * 1024;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

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
    let mut line = frame.to_string();
    line.push('\n');
    let mut guard = writer.lock().await;
    let Some(out) = guard.as_mut() else {
        return false;
    };
    out.write_all(line.as_bytes()).await.is_ok() && out.flush().await.is_ok()
}

/// One frame, or `None` at end of stream or when a peer overruns the cap.
async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Option<Value> {
    let mut line = Vec::new();
    let read = (&mut *reader)
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', &mut line)
        .await
        .ok()?;
    if read == 0 || line.len() > MAX_FRAME {
        return None;
    }
    serde_json::from_slice(&line).ok()
}

fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn nonce() -> Result<String, Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| Error::Protocol(error.to_string()))?;
    Ok(hex(&bytes))
}

/// Each side proves it holds the token without sending it.
fn proof(token: &str, role: &str, nonce: &str) -> String {
    mac(token, &format!("{role}:{nonce}"))
}

fn mac(secret: &str, message: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(message.as_bytes());
    hex(&mac.finalize().into_bytes())
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
        let stream = connect(&self.endpoint.address).await?;
        let (read, write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read);
        *self.writer.lock().await = Some(Box::new(write));
        let closed = || Error::Protocol("the connection host closed the channel".into());
        let silent = || Error::Protocol("the connection host did not answer".into());
        // The host proves itself first: whatever took its address learns nothing.
        let ours = nonce()?;
        let hello = json!({"id": 0, "method": "hello", "params": {"nonce": ours}});
        if !send(&self.writer, &hello).await {
            return Err(closed());
        }
        let challenge = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut reader))
            .await
            .ok()
            .flatten()
            .ok_or_else(silent)?;
        let offered = challenge["result"]["proof"].as_str().unwrap_or_default();
        let theirs = challenge["result"]["nonce"].as_str().unwrap_or_default();
        if theirs.len() != ours.len()
            || !same_secret(offered, &proof(&self.endpoint.token, "host", &ours))
        {
            return Err(Error::Protocol(
                "the connection host could not prove it is Medha's".into(),
            ));
        }
        let answer = json!({"id": 1, "method": "prove",
            "params": {"proof": proof(&self.endpoint.token, "chat", theirs)}});
        if !send(&self.writer, &answer).await {
            return Err(closed());
        }
        let reply = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut reader))
            .await
            .ok()
            .flatten()
            .ok_or_else(silent)?;
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
        while let Some(frame) = read_frame(&mut reader).await {
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
    let (read, write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let writer: Arc<Mutex<Option<Writer>>> = Arc::new(Mutex::new(Some(Box::new(write))));
    let Ok(Some(hello)) = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut reader)).await else {
        return;
    };
    let theirs = hello["params"]["nonce"].as_str().unwrap_or_default();
    let Ok(ours) = nonce() else {
        return;
    };
    if hello["method"] != "hello" || theirs.len() != ours.len() {
        return;
    }
    let challenge = json!({"id": hello["id"],
        "result": {"proof": proof(&token, "host", theirs), "nonce": ours}});
    if !send(&writer, &challenge).await {
        return;
    }
    let Ok(Some(answer)) = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut reader)).await
    else {
        return;
    };
    let offered = answer["params"]["proof"].as_str().unwrap_or_default();
    if answer["method"] != "prove" || !same_secret(offered, &proof(&token, "chat", &ours)) {
        return;
    }
    let mut changes = manager.subscribe();
    if !send(
        &writer,
        &json!({"id": answer["id"], "result": manager.snapshot().await}),
    )
    .await
    {
        return;
    }
    let pusher = tokio::spawn({
        let (manager, writer) = (manager.clone(), Arc::clone(&writer));
        async move {
            while changes.changed().await.is_ok() {
                let frame = json!({"method": "state", "params": manager.snapshot().await});
                if !send(&writer, &frame).await {
                    return;
                }
            }
        }
    });
    while let Some(frame) = read_frame(&mut reader).await {
        let Ok(request) = serde_json::from_value::<Request>(frame) else {
            continue;
        };
        let (manager, writer, resolve) =
            (manager.clone(), Arc::clone(&writer), Arc::clone(&resolve));
        tokio::spawn(async move {
            let id = request.id;
            let frame = match handle(&manager, request, &writer, &resolve).await {
                Ok(result) => json!({"id": id, "result": result}),
                Err(error) => json!({"id": id, "error": error}),
            };
            send(&writer, &frame).await;
        });
    }
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
            let relay = tokio::spawn({
                let (writer, server) = (Arc::clone(writer), server.clone());
                async move {
                    while let Some(url) = links.recv().await {
                        let frame =
                            json!({"method": "auth_url", "params": {"server": server, "url": url}});
                        send(&writer, &frame).await;
                    }
                }
            });
            let outcome = manager.authorize(&server, &sink).await;
            drop(sink);
            let _ = relay.await;
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
    listen(address, move |stream| {
        tokio::spawn(serve(
            manager.clone(),
            stream,
            Arc::clone(&token),
            Arc::clone(&resolve),
        ));
    })
    .await
}

#[cfg(unix)]
async fn connect(address: &str) -> Result<tokio::net::UnixStream, Error> {
    tokio::net::UnixStream::connect(address)
        .await
        .map_err(|error| Error::Protocol(format!("no connection host at {address}: {error}")))
}

#[cfg(unix)]
async fn listen(
    address: &str,
    mut accept: impl FnMut(tokio::net::UnixStream),
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::path::Path::new(address);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let (stream, _) = listener.accept().await?;
        accept(stream);
    }
}

#[cfg(windows)]
async fn connect(address: &str) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, Error> {
    tokio::net::windows::named_pipe::ClientOptions::new()
        .open(address)
        .map_err(|error| Error::Protocol(format!("no connection host at {address}: {error}")))
}

#[cfg(windows)]
async fn listen(
    address: &str,
    mut accept: impl FnMut(tokio::net::windows::named_pipe::NamedPipeServer),
) -> std::io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(address)?;
    loop {
        server.connect().await?;
        let next = ServerOptions::new()
            .reject_remote_clients(true)
            .create(address)?;
        accept(std::mem::replace(&mut server, next));
    }
}

#[cfg(test)]
#[path = "hub_tests.rs"]
mod tests;
