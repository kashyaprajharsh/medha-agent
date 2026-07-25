//! Supervised Model Context Protocol host for Medha (local stdio servers).
//!
//! Each configured server is a sandboxed child process spoken to over the
//! official `rmcp` stdio transport. Trusted servers connect in parallel at
//! startup (one slow/broken server never stalls the others); approval-gated
//! servers connect only after an explicit human gate. A supervisor sweep probes
//! liveness, reconnects with exponential backoff, parks flapping servers, and
//! re-lists catalogues on `tools/list_changed`. Discovered tools are filtered
//! per server, projected under `mcp__<server>__<tool>`, and treated as untrusted.

use std::{
    collections::HashMap,
    fmt,
    future::Future,
    path::PathBuf,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientInfo, ContentBlock, Implementation,
};
use rmcp::service::{NotificationContext, Peer, RoleClient, RunningService, ServiceExt};
use rmcp::transport::{
    IntoTransport, TokioChildProcess,
    streamable_http_client::{StreamableHttpClientTransport, StreamableHttpClientTransportConfig},
};
use rmcp::{ClientHandler, model::Tool};
use sandbox::{BackendKind, ExecRequest, NetPolicy, SandboxConfig, select_backend};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::{
    process::Command,
    sync::{Mutex, Semaphore, mpsc},
    task::JoinSet,
    time::{Instant, MissedTickBehavior, timeout},
};
use tokio_util::sync::CancellationToken;

mod oauth;

/// Prefix that marks a tool as MCP-provided and namespaces it by server.
pub const TOOL_PREFIX: &str = "mcp__";

/// Grace period for the protocol shutdown before the process group is killed.
const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// Tool descriptions are model context and an injection surface; bound them.
const MAX_DESCRIPTION: usize = 1_024;
/// Concurrent in-flight calls allowed against a server that declares parallel-safe.
const PARALLEL_CALLS: usize = 8;

type Client = RunningService<RoleClient, Handler>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("MCP is disabled")]
    Disabled,
    #[error("no MCP server named '{0}'")]
    UnknownServer(String),
    /// The server exists but is not usable right now. Carries the state so a
    /// caller can tell "approve it" from "it crashed" without parsing text.
    #[error("MCP server '{server}' is {state}{}", detail.as_deref().map(|d| format!(": {d}")).unwrap_or_default())]
    ServerNotReady {
        server: String,
        state: ServerState,
        detail: Option<String>,
    },
    #[error("'{tool}' is not an exposed tool on MCP server '{server}'")]
    UnknownTool { server: String, tool: String },
    #[error("MCP tool name must be '{TOOL_PREFIX}<server>__<tool>': {0}")]
    BadToolName(String),
    #[error("invalid arguments for '{tool}': {reason}")]
    BadArguments { tool: String, reason: String },
    #[error("{server} is unavailable ({command}); {hint}")]
    Unavailable {
        server: String,
        command: String,
        hint: String,
    },
    #[error("MCP command is empty")]
    EmptyCommand,
    #[error(
        "MCP server '{0}' needs authorization — connect it from /mcp or run `medha mcp auth {0}`"
    )]
    NeedsAuth(String),
    #[error(
        "MCP server '{0}' needs an API token — add one with `medha mcp add {0} --url <url> --bearer <token>`"
    )]
    NeedsToken(String),
    #[error("MCP authorization failed: {0}")]
    Auth(String),
    #[error("invalid MCP server URL: {0}")]
    BadUrl(String),
    #[error("MCP sandbox could not be prepared: {0}")]
    Sandbox(String),
    #[error("failed to start MCP server: {0}")]
    Spawn(String),
    #[error("MCP request failed: {0}")]
    Protocol(String),
    #[error("MCP request timed out after {0:?}")]
    Timeout(Duration),
}

impl Error {
    /// Configuration faults no retry can fix; everything else is worth another attempt.
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Error::EmptyCommand | Error::Unavailable { .. } | Error::Sandbox(_) | Error::BadUrl(_)
        )
    }

    /// Server state behind the failure, when the error is about one.
    pub fn server_state(&self) -> Option<ServerState> {
        match self {
            Error::ServerNotReady { state, .. } => Some(*state),
            _ => None,
        }
    }
}

/// Per-server tool exposure filter. `allow` (when non-empty) whitelists, then
/// `deny` subtracts. Entries are exact names or a single trailing `*` glob.
#[derive(Debug, Clone, Default)]
pub struct ToolFilter {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl ToolFilter {
    fn admits(&self, tool: &str) -> bool {
        if !self.allow.is_empty() && !self.allow.iter().any(|p| glob(p, tool)) {
            return false;
        }
        !self.deny.iter().any(|p| glob(p, tool))
    }
}

/// Exact match, or `prefix*` when the pattern ends in `*`.
fn glob(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// How Medha reaches a server.
#[derive(Debug, Clone)]
pub enum Transport {
    /// Local child process spoken to over stdio, sandboxed with a sanitized
    /// environment.
    Stdio {
        command: Vec<String>,
        /// Explicit environment the server needs (e.g. an API token). Nothing
        /// else from Medha's environment is inherited.
        env: Vec<(String, String)>,
    },
    /// Hosted server reached over Streamable HTTP.
    Remote { url: String, auth: RemoteAuth },
}

impl Default for Transport {
    fn default() -> Self {
        Transport::Stdio {
            command: Vec::new(),
            env: Vec::new(),
        }
    }
}

impl Transport {
    /// One-line description for approval previews and status.
    pub fn target(&self) -> String {
        match self {
            Transport::Stdio { command, .. } => command.join(" "),
            Transport::Remote { url, .. } => url.clone(),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Transport::Stdio { .. } => "stdio",
            Transport::Remote { .. } => "http",
        }
    }
}

/// Credential scheme for a remote server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RemoteAuth {
    /// Ask the server what it needs on first connect. The default, so pasting a
    /// URL is enough — no flags to remember.
    #[default]
    Auto,
    /// Explicitly unauthenticated.
    None,
    /// Static token sent as `Authorization: Bearer …`.
    Bearer(String),
    /// Authorization-code + PKCE. Tokens persist through the host's
    /// [`TokenStore`], so the browser is needed once, not every launch.
    OAuth,
}

/// Where remote OAuth credentials persist between sessions. Medha wires this to
/// the OS keychain; without one an OAuth server re-authorizes every launch.
pub trait TokenStore: Send + Sync + fmt::Debug {
    fn load(&self, server: &str) -> Option<String>;
    fn save(&self, server: &str, blob: &str);
    fn clear(&self, server: &str);
}

/// Sink for an authorization URL, so the caller shows it in whatever UI it owns
/// while the browser opens.
pub type UrlSink = tokio::sync::mpsc::UnboundedSender<String>;

/// One configured server. Built from the user config; `requires_approval` gates
/// a project-defined command behind a one-time human preview.
#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    pub id: String,
    pub transport: Transport,
    pub requires_approval: bool,
    /// Per-server network override; `None` falls back to the host default.
    pub allow_network: Option<bool>,
    /// Which of the server's tools reach the model. Unfiltered by default.
    pub tools: ToolFilter,
    /// Opt-in concurrent calls. Off by default: most servers hold per-session
    /// state and a server annotation is a hint, never a guarantee.
    pub parallel_calls: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub servers: Vec<ServerConfig>,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub max_text_chars: usize,
    /// Default network policy; a server may override it.
    pub allow_network: bool,
    /// Supervisor sweep period: liveness probe and reconnect scheduling.
    pub health_interval: Duration,
    /// Consecutive connect failures tolerated before the server is parked.
    pub max_reconnects: u32,
    /// How long a parked server waits before one slow self-probe.
    pub park_probe: Duration,
    /// How long the interactive OAuth flow waits for the browser redirect.
    pub auth_timeout: Duration,
    /// Persistence for remote OAuth credentials.
    pub tokens: Option<Arc<dyn TokenStore>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            servers: Vec::new(),
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            max_text_chars: 16_000,
            // MCP servers commonly need network; keep the fs jail, allow net.
            allow_network: true,
            health_interval: Duration::from_secs(5),
            max_reconnects: 5,
            park_probe: Duration::from_secs(300),
            auth_timeout: Duration::from_secs(300),
            tokens: None,
        }
    }
}

/// Connection lifecycle. `Parked` and `Failed` are both quiescent, but only
/// `Failed` is permanent — a parked server still self-probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    NeedsApproval,
    /// Remote server with no usable credentials; waiting on an interactive sign-in.
    NeedsAuth,
    /// Remote server wanting credentials Medha cannot obtain on its own — the
    /// user has to supply a token.
    NeedsToken,
    Connecting,
    Ready,
    /// Was ready, lost its transport; a reconnect is scheduled.
    Degraded,
    Reconnecting,
    /// Reconnect budget exhausted; revived only by a slow self-probe or refresh.
    Parked,
    /// Terminal: a configuration fault retrying cannot fix.
    Failed,
    /// Shut down deliberately.
    Stopped,
}

impl ServerState {
    fn is_live(self) -> bool {
        matches!(self, ServerState::Ready)
    }
}

impl fmt::Display for ServerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NeedsApproval => "awaiting approval",
            Self::NeedsAuth => "needs sign-in",
            Self::NeedsToken => "needs an API token",
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Reconnecting => "reconnecting",
            Self::Parked => "parked after repeated failures",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    pub server: String,
    pub state: ServerState,
    /// Tools exposed to the model.
    pub tools: usize,
    /// Tools the server offers that the filter (or a malformed name) withheld.
    #[serde(skip_serializing_if = "is_zero")]
    pub hidden: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

#[derive(Debug, Clone, Serialize)]
pub struct StartPreview {
    pub server: String,
    pub transport: String,
    /// Command line for stdio, URL for remote.
    pub target: String,
    pub approval_required: bool,
}

/// A projected MCP tool, ready to expose to the model.
#[derive(Debug, Clone)]
pub struct McpToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// A tool-call result flattened to text. The text is complete — capping and
/// artifact spill happen at the tool layer, which owns the artifact store.
#[derive(Debug, Clone)]
pub struct CallOutput {
    pub server: String,
    pub tool: String,
    pub text: String,
    pub is_error: bool,
}

/// A server's exposed tools plus how many were withheld.
#[derive(Default)]
struct Catalog {
    exposed: Vec<McpToolSpec>,
    hidden: usize,
}

/// A live connection taken out of its slot, awaiting shutdown and process reap.
struct Retiree {
    client: Option<Client>,
    pid: Option<u32>,
}

impl Retiree {
    fn is_empty(&self) -> bool {
        self.client.is_none() && self.pid.is_none()
    }

    /// Protocol shutdown first (rmcp closes the transport and waits for the
    /// child), then a forced process-group kill so `uvx`/`npx` grandchildren
    /// cannot survive as orphans.
    async fn retire(mut self) {
        if let Some(client) = &mut self.client {
            let _ = client.close_with_timeout(CLOSE_GRACE).await;
        }
        drop(self.client.take());
        kill_process_group(self.pid);
    }
}

struct Slot {
    config: ServerConfig,
    state: ServerState,
    client: Option<Client>,
    /// Cheap clone used to issue requests without holding the server map lock.
    peer: Option<Peer<RoleClient>>,
    /// Bounds in-flight calls: 1 permit unless the server opts into parallel.
    gate: Arc<Semaphore>,
    pid: Option<u32>,
    detail: Option<String>,
    /// Consecutive connect failures. Reset only once a live request proves the
    /// connection — a handshake alone can flap moments later.
    failures: u32,
    proven: bool,
    retry_at: Option<Instant>,
}

impl Slot {
    fn new(config: ServerConfig) -> Self {
        let state = if config.requires_approval {
            ServerState::NeedsApproval
        } else {
            ServerState::Connecting
        };
        let permits = if config.parallel_calls {
            PARALLEL_CALLS
        } else {
            1
        };
        Self {
            config,
            state,
            client: None,
            peer: None,
            gate: Arc::new(Semaphore::new(permits)),
            pid: None,
            detail: None,
            failures: 0,
            proven: false,
            retry_at: None,
        }
    }

    /// Detach the live connection so it can be retired outside the map lock.
    fn detach(&mut self) -> Retiree {
        self.peer = None;
        self.proven = false;
        Retiree {
            client: self.client.take(),
            pid: self.pid.take(),
        }
    }
}

/// Client-side protocol handler. The only server→client traffic Medha acts on is
/// `tools/list_changed`; sampling and elicitation stay refused (Stage 2-D).
struct Handler {
    server: String,
    changed: mpsc::UnboundedSender<String>,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.client_info = Implementation::new("medha", env!("CARGO_PKG_VERSION"));
        info
    }

    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        let _ = self.changed.send(self.server.clone());
        std::future::ready(())
    }
}

/// A freshly established connection, before it is installed into its slot.
struct Connected {
    client: Client,
    peer: Peer<RoleClient>,
    pid: Option<u32>,
    catalog: Catalog,
}

struct ManagerInner {
    workspace: PathBuf,
    config: Config,
    servers: Mutex<HashMap<String, Slot>>,
    tools: RwLock<HashMap<String, Catalog>>,
    changed_tx: mpsc::UnboundedSender<String>,
    changed_rx: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    supervising: AtomicBool,
    cancel: CancellationToken,
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Last-resort reap: the supervisor is gone, so nothing else will run the
        // graceful path for connections still attached.
        for slot in self.servers.get_mut().values_mut() {
            kill_process_group(slot.pid.take());
        }
    }
}

#[derive(Clone)]
pub struct McpManager {
    inner: Arc<ManagerInner>,
}

impl McpManager {
    pub fn new(workspace: PathBuf, config: Config) -> Self {
        let servers = config
            .servers
            .iter()
            .map(|server| (server.id.clone(), Slot::new(server.clone())))
            .collect();
        let (changed_tx, changed_rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(ManagerInner {
                workspace: normalize(&workspace),
                config,
                servers: Mutex::new(servers),
                tools: RwLock::new(HashMap::new()),
                changed_tx,
                changed_rx: Mutex::new(Some(changed_rx)),
                supervising: AtomicBool::new(false),
                cancel: CancellationToken::new(),
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.config.enabled
    }

    /// Cap the tool layer applies before spilling the remainder to an artifact.
    pub fn max_text_chars(&self) -> usize {
        self.inner.config.max_text_chars
    }

    /// Connect every trusted (non-approval) server in parallel. Each attempt is
    /// individually bounded by the startup timeout, so every server reaches a
    /// definite state and one slow server never stalls the others.
    pub async fn connect_startup(&self) {
        if !self.inner.config.enabled {
            return;
        }
        self.ensure_supervisor();
        let mut set = JoinSet::new();
        for server in &self.inner.config.servers {
            if server.requires_approval {
                continue;
            }
            let this = self.clone();
            let server = server.clone();
            set.spawn(async move {
                let _ = this.connect_one(&server).await;
            });
        }
        while set.join_next().await.is_some() {}
    }

    pub async fn start_preview(&self, server_id: &str) -> Result<StartPreview, Error> {
        let server = self.server_config(server_id).await?;
        Ok(StartPreview {
            server: server.id,
            transport: server.transport.label().to_string(),
            target: server.transport.target(),
            approval_required: server.requires_approval,
        })
    }

    /// Add a server at runtime (from `/mcp add`) and connect it immediately.
    pub async fn add_server(&self, server: ServerConfig) -> Result<ServerStatus, Error> {
        if !self.inner.config.enabled {
            return Err(Error::Disabled);
        }
        self.ensure_supervisor();
        let replaced = {
            let mut servers = self.inner.servers.lock().await;
            let mut slot = Slot::new(server.clone());
            // A runtime add is its own approval; connect it without a second gate.
            slot.state = ServerState::Connecting;
            servers
                .insert(server.id.clone(), slot)
                .map(|mut old| old.detach())
        };
        if let Some(retiree) = replaced {
            retiree.retire().await;
        }
        self.connect_one(&server).await?;
        self.server_status(&server.id).await
    }

    /// Remove a server at runtime (from `/mcp remove`): protocol shutdown, reap
    /// its process tree, and purge its tools.
    pub async fn remove_server(&self, server_id: &str) -> Result<(), Error> {
        let retiree = self
            .inner
            .servers
            .lock()
            .await
            .remove(server_id)
            .map(|mut slot| slot.detach())
            .ok_or_else(|| Error::UnknownServer(server_id.to_string()))?;
        self.forget_tools(server_id);
        retiree.retire().await;
        Ok(())
    }

    /// Approve and connect an approval-gated server. Also the manual refresh
    /// path: it clears the failure budget so a parked server retries at once.
    ///
    /// `announce` opts the caller into the interactive sign-in for a remote
    /// OAuth server that has no usable credentials — it receives the
    /// authorization URL while a browser opens. Passing `None` keeps the call
    /// non-interactive, which is what a model-invoked tool must do.
    pub async fn approve_and_connect(
        &self,
        server_id: &str,
        announce: Option<&UrlSink>,
    ) -> Result<ServerStatus, Error> {
        let server = self.server_config(server_id).await?;
        self.ensure_supervisor();
        {
            let mut servers = self.inner.servers.lock().await;
            if let Some(slot) = servers.get_mut(server_id) {
                slot.failures = 0;
                slot.retry_at = None;
            }
        }
        match (self.connect_one(&server).await, announce) {
            (Err(Error::NeedsAuth(_)), Some(announce)) => self.authorize(server_id, announce).await,
            (result, _) => {
                result?;
                self.server_status(server_id).await
            }
        }
    }

    /// Run the interactive OAuth flow for a remote server, persist the tokens,
    /// and connect. The only path that may open a browser.
    pub async fn authorize(
        &self,
        server_id: &str,
        announce: &UrlSink,
    ) -> Result<ServerStatus, Error> {
        let server = self.server_config(server_id).await?;
        // `Auto` reaches here once the probe found an OAuth challenge, so both
        // it and an explicit `OAuth` are valid sign-in targets.
        let Transport::Remote {
            url,
            auth: RemoteAuth::OAuth | RemoteAuth::Auto,
        } = &server.transport
        else {
            return Err(Error::Auth(format!(
                "'{server_id}' is not a remote server; nothing to sign in to"
            )));
        };
        let store = self
            .inner
            .config
            .tokens
            .as_ref()
            .ok_or_else(|| Error::Auth("no credential store is configured".into()))?;
        self.set_state(
            server_id,
            ServerState::Connecting,
            Some("signing in…".into()),
        )
        .await;
        let credentials =
            match oauth::authorize(url, self.inner.config.auth_timeout, announce).await {
                Ok(credentials) => credentials,
                Err(error) => {
                    self.set_state(server_id, ServerState::NeedsAuth, Some(error.to_string()))
                        .await;
                    return Err(error);
                }
            };
        store.save(server_id, &credentials);
        self.ensure_supervisor();
        self.connect_one(&server).await?;
        self.server_status(server_id).await
    }

    /// Forget a remote server's stored credentials, so the next connect asks
    /// for a fresh sign-in.
    pub fn sign_out(&self, server_id: &str) {
        if let Some(store) = &self.inner.config.tokens {
            store.clear(server_id);
        }
    }

    /// Currently-exposed tools, namespaced. Read each turn to build the
    /// capability sheath, so a server that connects mid-session appears next turn.
    pub fn tool_specs(&self) -> Vec<McpToolSpec> {
        let mut specs: Vec<McpToolSpec> = self
            .catalogs()
            .values()
            .flat_map(|catalog| catalog.exposed.iter().cloned())
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    pub fn is_mcp_tool(name: &str) -> bool {
        name.starts_with(TOOL_PREFIX)
    }

    /// Whether this server's credentials must be obtained interactively before
    /// it can connect.
    pub async fn needs_sign_in(&self, server_id: &str) -> bool {
        self.inner
            .servers
            .lock()
            .await
            .get(server_id)
            .is_some_and(|slot| slot.state == ServerState::NeedsAuth)
    }

    /// Invoke `mcp__<server>__<tool>` with the given arguments.
    pub async fn call(&self, qualified: &str, args: &Value) -> Result<CallOutput, Error> {
        if !self.inner.config.enabled {
            return Err(Error::Disabled);
        }
        let (server, tool) = parse_qualified(qualified)?;
        let (peer, gate) = {
            let servers = self.inner.servers.lock().await;
            let slot = servers
                .get(&server)
                .ok_or_else(|| Error::UnknownServer(server.clone()))?;
            match (&slot.peer, slot.state.is_live()) {
                (Some(peer), true) => (peer.clone(), Arc::clone(&slot.gate)),
                _ => {
                    return Err(Error::ServerNotReady {
                        server,
                        state: slot.state,
                        detail: slot.detail.clone(),
                    });
                }
            }
        };
        // Only exposed tools are callable: a filtered or malformed tool is not
        // addressable even if the model guesses its name.
        let schema = self
            .catalogs()
            .get(&server)
            .and_then(|catalog| catalog.exposed.iter().find(|spec| spec.name == qualified))
            .map(|spec| spec.schema.clone())
            .ok_or_else(|| Error::UnknownTool {
                server: server.clone(),
                tool: tool.clone(),
            })?;
        validate_arguments(&schema, args).map_err(|reason| Error::BadArguments {
            tool: qualified.to_string(),
            reason,
        })?;

        let mut params = CallToolRequestParams::new(tool.clone());
        if let Some(object) = args.as_object() {
            params = params.with_arguments(object.clone());
        }
        let _permit = gate.acquire().await.map_err(|_| Error::ServerNotReady {
            server: server.clone(),
            state: ServerState::Stopped,
            detail: None,
        })?;
        let result = match timeout(self.inner.config.request_timeout, peer.call_tool(params)).await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Err(Error::Protocol(error.to_string())),
            Err(_) => return Err(Error::Timeout(self.inner.config.request_timeout)),
        };
        // A completed round trip is the proof that resets the reconnect budget.
        self.mark_proven(&server).await;
        Ok(CallOutput {
            server,
            tool,
            is_error: result.is_error.unwrap_or(false),
            text: flatten(&result),
        })
    }

    pub async fn status(&self) -> Vec<ServerStatus> {
        let servers = self.inner.servers.lock().await;
        let catalogs = self.catalogs();
        let mut out: Vec<ServerStatus> = servers
            .iter()
            .map(|(id, slot)| status_of(id, slot, catalogs.get(id)))
            .collect();
        out.sort_by(|a, b| a.server.cmp(&b.server));
        out
    }

    /// Ordered teardown: stop accepting work, protocol-shutdown every server in
    /// parallel, then force-reap the process groups.
    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        let retirees: Vec<Retiree> = {
            let mut servers = self.inner.servers.lock().await;
            servers
                .values_mut()
                .map(|slot| {
                    let retiree = slot.detach();
                    slot.state = ServerState::Stopped;
                    slot.detail = None;
                    slot.retry_at = None;
                    retiree
                })
                .collect()
        };
        let mut set = JoinSet::new();
        for retiree in retirees.into_iter().filter(|r| !r.is_empty()) {
            set.spawn(retiree.retire());
        }
        while set.join_next().await.is_some() {}
        self.tools_mut().clear();
    }

    /// Start the supervisor sweep and the `tools/list_changed` listener once.
    /// Lazy so `McpManager::new` stays usable outside a Tokio runtime.
    fn ensure_supervisor(&self) {
        if !self.inner.config.enabled
            || self.inner.supervising.swap(true, Ordering::AcqRel)
            || self.inner.cancel.is_cancelled()
        {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(supervise(weak));
        if let Ok(mut guard) = self.inner.changed_rx.try_lock()
            && let Some(rx) = guard.take()
        {
            tokio::spawn(watch_tool_changes(Arc::downgrade(&self.inner), rx));
        }
    }

    /// One supervisor pass: retire dead transports, probe unproven connections,
    /// and run any reconnect whose backoff has elapsed.
    async fn health_sweep(&self) {
        let now = Instant::now();
        let mut retirees = Vec::new();
        let mut due = Vec::new();
        let mut probe = Vec::new();
        {
            let mut servers = self.inner.servers.lock().await;
            for slot in servers.values_mut() {
                match slot.state {
                    ServerState::Ready => {
                        if slot.client.as_ref().is_none_or(|c| c.is_transport_closed()) {
                            retirees.push(slot.detach());
                            slot.state = ServerState::Degraded;
                            slot.detail = Some("connection lost".into());
                            slot.retry_at = Some(now);
                        } else if !slot.proven {
                            probe.push(slot.config.id.clone());
                        }
                    }
                    ServerState::Degraded | ServerState::Parked => {
                        if slot.retry_at.is_some_and(|at| at <= now) {
                            slot.retry_at = None;
                            due.push(slot.config.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        // Reap the old process tree before spawning any replacement.
        for retiree in retirees.into_iter().filter(|r| !r.is_empty()) {
            retiree.retire().await;
        }
        for id in probe {
            self.refresh_tools(&id).await;
        }
        for server in due {
            self.forget_tools(&server.id);
            let _ = self.connect_one(&server).await;
        }
    }

    /// Re-list a server's catalogue. Doubles as the liveness probe: a successful
    /// round trip is what proves a fresh connection and clears its failure count.
    async fn refresh_tools(&self, server_id: &str) {
        let Some((peer, filter)) = ({
            let servers = self.inner.servers.lock().await;
            servers.get(server_id).and_then(|slot| {
                slot.peer
                    .clone()
                    .map(|peer| (peer, slot.config.tools.clone()))
            })
        }) else {
            return;
        };
        match timeout(self.inner.config.request_timeout, peer.list_all_tools()).await {
            Ok(Ok(tools)) => {
                let catalog = build_catalog(server_id, &filter, tools);
                self.tools_mut().insert(server_id.to_string(), catalog);
                self.mark_proven(server_id).await;
            }
            outcome => {
                let detail = match outcome {
                    Ok(Err(error)) => error.to_string(),
                    _ => "tools/list timed out".to_string(),
                };
                tracing::debug!(target: "medha_mcp", server = %server_id, %detail, "MCP catalogue refresh failed");
            }
        }
    }

    /// Reconnect (or first-connect) a server: reap any predecessor, spawn, and
    /// install or record the failure with a backoff.
    async fn connect_one(&self, server: &ServerConfig) -> Result<(), Error> {
        let previous = {
            let mut servers = self.inner.servers.lock().await;
            let Some(slot) = servers.get_mut(&server.id) else {
                return Err(Error::UnknownServer(server.id.clone()));
            };
            slot.state = if slot.failures > 0 || slot.client.is_some() {
                ServerState::Reconnecting
            } else {
                ServerState::Connecting
            };
            slot.detach()
        };
        if !previous.is_empty() {
            previous.retire().await;
        }

        match self.spawn_client(server).await {
            Ok(connected) => {
                let orphan = {
                    let mut servers = self.inner.servers.lock().await;
                    match servers.get_mut(&server.id) {
                        Some(slot) => {
                            slot.client = Some(connected.client);
                            slot.peer = Some(connected.peer);
                            slot.pid = connected.pid;
                            slot.state = ServerState::Ready;
                            slot.detail = None;
                            slot.retry_at = None;
                            None
                        }
                        // Removed mid-connect: retire the new client, don't leak it.
                        None => Some(Retiree {
                            client: Some(connected.client),
                            pid: connected.pid,
                        }),
                    }
                };
                if let Some(orphan) = orphan {
                    orphan.retire().await;
                    return Err(Error::UnknownServer(server.id.clone()));
                }
                self.tools_mut()
                    .insert(server.id.clone(), connected.catalog);
                Ok(())
            }
            Err(error) => {
                self.record_failure(&server.id, &error).await;
                tracing::debug!(target: "medha_mcp", server = %server.id, %error, "MCP server failed to start");
                Err(error)
            }
        }
    }

    async fn record_failure(&self, server_id: &str, error: &Error) {
        let mut servers = self.inner.servers.lock().await;
        let Some(slot) = servers.get_mut(server_id) else {
            return;
        };
        slot.detail = Some(error.to_string());
        // Missing credentials are not a fault to retry — only a human can clear
        // it, so park the slot in a state the UI can act on.
        match error {
            Error::NeedsAuth(_) => {
                slot.state = ServerState::NeedsAuth;
                slot.retry_at = None;
                return;
            }
            Error::NeedsToken(_) => {
                slot.state = ServerState::NeedsToken;
                slot.retry_at = None;
                return;
            }
            _ => {}
        }
        slot.failures = slot.failures.saturating_add(1);
        if error.is_terminal() {
            slot.state = ServerState::Failed;
            slot.retry_at = None;
        } else if slot.failures >= self.inner.config.max_reconnects {
            // Park instead of hot-looping; a slow self-probe still revives it.
            slot.state = ServerState::Parked;
            slot.retry_at = Some(Instant::now() + self.inner.config.park_probe);
        } else {
            slot.state = ServerState::Degraded;
            slot.retry_at = Some(Instant::now() + backoff(slot.failures));
        }
    }

    async fn set_state(&self, server_id: &str, state: ServerState, detail: Option<String>) {
        let mut servers = self.inner.servers.lock().await;
        if let Some(slot) = servers.get_mut(server_id) {
            slot.state = state;
            slot.detail = detail;
        }
    }

    async fn mark_proven(&self, server_id: &str) {
        let mut servers = self.inner.servers.lock().await;
        if let Some(slot) = servers.get_mut(server_id) {
            slot.proven = true;
            slot.failures = 0;
        }
    }

    async fn spawn_client(&self, server: &ServerConfig) -> Result<Connected, Error> {
        match &server.transport {
            Transport::Stdio { command, env } => self.spawn_stdio(server, command, env).await,
            Transport::Remote { url, auth } => self.connect_remote(server, url, auth).await,
        }
    }

    async fn spawn_stdio(
        &self,
        server: &ServerConfig,
        command: &[String],
        env: &[(String, String)],
    ) -> Result<Connected, Error> {
        let (program, args) = command.split_first().ok_or(Error::EmptyCommand)?;
        if !sandbox::program_on_path(program) {
            return Err(Error::Unavailable {
                server: server.id.clone(),
                command: program.clone(),
                hint: "install the configured MCP server executable".into(),
            });
        }
        let command = self.build_command(program, args, env, server)?;
        let transport = TokioChildProcess::new(command).map_err(|e| Error::Spawn(e.to_string()))?;
        let pid = transport.id();
        match self.handshake(server, transport).await {
            Ok(mut connected) => {
                connected.pid = pid;
                Ok(connected)
            }
            // The transport is already dropped; reap the tree it may have spawned.
            Err(error) => {
                kill_process_group(pid);
                Err(error)
            }
        }
    }

    /// Connect a hosted server over Streamable HTTP. OAuth reconnects from
    /// persisted credentials only — the interactive flow needs a human, so it
    /// lives in [`McpManager::authorize`] and never runs from a model call.
    async fn connect_remote(
        &self,
        server: &ServerConfig,
        url: &str,
        auth: &RemoteAuth,
    ) -> Result<Connected, Error> {
        oauth::require_secure(url)?;
        let config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
        match auth {
            RemoteAuth::None => {
                self.handshake(
                    server,
                    StreamableHttpClientTransport::with_client(reqwest::Client::new(), config),
                )
                .await
            }
            RemoteAuth::Bearer(token) => {
                self.handshake(
                    server,
                    StreamableHttpClientTransport::with_client(
                        reqwest::Client::new(),
                        config.auth_header(token.clone()),
                    ),
                )
                .await
            }
            RemoteAuth::OAuth => {
                let stored = self
                    .stored_token(&server.id)
                    .ok_or_else(|| Error::NeedsAuth(server.id.clone()))?;
                let client = oauth::client_from_stored(url, &stored).await?;
                self.handshake(
                    server,
                    StreamableHttpClientTransport::with_client(client, config),
                )
                .await
            }
            // Let the server decide, so configuring one is just a pasted URL.
            RemoteAuth::Auto => {
                if self.stored_token(&server.id).is_some() {
                    return Box::pin(self.connect_remote(server, url, &RemoteAuth::OAuth)).await;
                }
                match oauth::probe(url).await {
                    oauth::Challenge::Open => {
                        Box::pin(self.connect_remote(server, url, &RemoteAuth::None)).await
                    }
                    oauth::Challenge::OAuth => Err(Error::NeedsAuth(server.id.clone())),
                    oauth::Challenge::Token => Err(Error::NeedsToken(server.id.clone())),
                }
            }
        }
    }

    fn stored_token(&self, server_id: &str) -> Option<String> {
        self.inner
            .config
            .tokens
            .as_ref()
            .and_then(|store| store.load(server_id))
    }

    /// Protocol handshake plus the first catalogue listing, under one deadline.
    async fn handshake<T, E, A>(
        &self,
        server: &ServerConfig,
        transport: T,
    ) -> Result<Connected, Error>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let handler = Handler {
            server: server.id.clone(),
            changed: self.inner.changed_tx.clone(),
        };
        let deadline = self.inner.config.startup_timeout;
        let connect = async {
            let client = handler
                .serve(transport)
                .await
                .map_err(|e| Error::Spawn(e.to_string()))?;
            // Paginated: a large server would otherwise silently lose its tail.
            let tools = client
                .list_all_tools()
                .await
                .map_err(|e| Error::Protocol(e.to_string()))?;
            Ok::<_, Error>((client, tools))
        };
        match timeout(deadline, connect).await {
            Ok(Ok((client, tools))) => Ok(Connected {
                peer: client.peer().clone(),
                client,
                pid: None,
                catalog: build_catalog(&server.id, &server.tools, tools),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Timeout(deadline)),
        }
    }

    fn build_command(
        &self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
        server: &ServerConfig,
    ) -> Result<Command, Error> {
        let allow_network = server
            .allow_network
            .unwrap_or(self.inner.config.allow_network);
        let sandbox_config = SandboxConfig {
            backend: BackendKind::Native,
            net: if allow_network {
                NetPolicy::Allow
            } else {
                NetPolicy::Deny
            },
            ..SandboxConfig::default()
        };
        // Package-manager servers (uvx/npx) must write a cache. Rather than widen
        // the write jail to the real ~/.cache (which sits next to credentials and
        // dotfiles), redirect their caches into ONE Medha-owned directory and make
        // only that directory writable. The jail stays tight everywhere else.
        let cache = mcp_cache_dir();
        if let Some(dir) = &cache {
            let _ = std::fs::create_dir_all(dir);
        }
        let backend = select_backend(&sandbox_config, cache.clone().into_iter().collect());
        if !allow_network && backend.label() == "host" {
            return Err(Error::Sandbox(format!(
                "network-denied isolation is unavailable for '{}'; allow network for it or the host",
                server.id
            )));
        }
        let mut environment = sanitized_environment();
        if let Some(dir) = &cache {
            environment.retain(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "XDG_CACHE_HOME" | "XDG_DATA_HOME" | "UV_CACHE_DIR" | "npm_config_cache"
                )
            });
            let path = |sub: &str| dir.join(sub).to_string_lossy().into_owned();
            // Caches, downloaded package data (uvx installs tools here), and npm
            // cache all land under the one writable Medha dir.
            environment.push(("XDG_CACHE_HOME".into(), path("cache")));
            environment.push(("XDG_DATA_HOME".into(), path("data")));
            environment.push(("UV_CACHE_DIR".into(), path("cache/uv")));
            environment.push(("npm_config_cache".into(), path("npm")));
        }
        // Explicit per-server env (e.g. an API token) wins over the defaults.
        environment.extend(env.iter().cloned());
        let request = ExecRequest {
            program: program.to_string(),
            args: args.to_vec(),
            cwd: self.inner.workspace.clone(),
            env: environment,
            clear_env: true,
        };
        let mut command = backend
            .build_command(&request)
            .map_err(|e| Error::Sandbox(e.to_string()))?;
        command.kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        Ok(command)
    }

    async fn server_config(&self, server_id: &str) -> Result<ServerConfig, Error> {
        self.inner
            .servers
            .lock()
            .await
            .get(server_id)
            .map(|slot| slot.config.clone())
            .ok_or_else(|| Error::UnknownServer(server_id.to_string()))
    }

    async fn server_status(&self, server_id: &str) -> Result<ServerStatus, Error> {
        let servers = self.inner.servers.lock().await;
        let slot = servers
            .get(server_id)
            .ok_or_else(|| Error::UnknownServer(server_id.to_string()))?;
        Ok(status_of(server_id, slot, self.catalogs().get(server_id)))
    }

    fn catalogs(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Catalog>> {
        self.inner.tools.read().expect("mcp tools lock poisoned")
    }

    fn tools_mut(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Catalog>> {
        self.inner.tools.write().expect("mcp tools lock poisoned")
    }

    fn forget_tools(&self, server_id: &str) {
        self.tools_mut().remove(server_id);
    }
}

/// Supervisor sweep: ticks until the manager is cancelled or dropped.
async fn supervise(weak: Weak<ManagerInner>) {
    let Some((interval, cancel)) = weak
        .upgrade()
        .map(|inner| (inner.config.health_interval, inner.cancel.clone()))
    else {
        return;
    };
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await; // the first tick is immediate; skip it
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let Some(inner) = weak.upgrade() else { return };
        McpManager { inner }.health_sweep().await;
    }
}

/// Refresh a server's catalogue whenever it announces `tools/list_changed`.
async fn watch_tool_changes(weak: Weak<ManagerInner>, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(server) = rx.recv().await {
        let Some(inner) = weak.upgrade() else { return };
        if inner.cancel.is_cancelled() {
            return;
        }
        McpManager { inner }.refresh_tools(&server).await;
    }
}

fn status_of(id: &str, slot: &Slot, catalog: Option<&Catalog>) -> ServerStatus {
    ServerStatus {
        server: id.to_string(),
        state: slot.state,
        tools: catalog.map_or(0, |c| c.exposed.len()),
        hidden: catalog.map_or(0, |c| c.hidden),
        detail: slot.detail.clone(),
    }
}

/// Exponential reconnect backoff, capped so a flapping server never hot-loops.
fn backoff(failures: u32) -> Duration {
    const BASE_MS: u64 = 500;
    const CAP: Duration = Duration::from_secs(30);
    CAP.min(Duration::from_millis(BASE_MS << failures.min(6)))
}

/// Project a server's tools, dropping the ones the filter or name rules withhold.
fn build_catalog(server: &str, filter: &ToolFilter, tools: Vec<Tool>) -> Catalog {
    let mut exposed = Vec::with_capacity(tools.len());
    let mut hidden = 0;
    for tool in tools {
        // A malformed name would mis-route a later call; a filtered one is
        // deliberate. Neither reaches the model's context.
        if !valid_tool_name(&tool.name) || !filter.admits(&tool.name) {
            hidden += 1;
            continue;
        }
        exposed.push(McpToolSpec {
            name: qualify(server, &tool.name),
            description: truncate(tool.description.as_deref().unwrap_or(""), MAX_DESCRIPTION),
            schema: Value::Object(tool.input_schema.as_ref().clone()),
        });
    }
    exposed.sort_by(|a, b| a.name.cmp(&b.name));
    Catalog { exposed, hidden }
}

/// A tool name must be a plain bounded identifier: no collision with the `__`
/// namespace separator, no control characters, no unbounded length.
fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.contains("__")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Structural check against the server's schema: arguments must be an object
/// carrying every declared `required` property. Full JSON-Schema validation is
/// the server's job; this stops the malformed calls before they cost a round trip.
fn validate_arguments(schema: &Value, args: &Value) -> Result<(), String> {
    let object = match args {
        Value::Null => None,
        Value::Object(map) => Some(map),
        _ => return Err("arguments must be a JSON object".into()),
    };
    let missing: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|required| {
            required
                .iter()
                .filter_map(Value::as_str)
                .filter(|name| object.is_none_or(|map| !map.contains_key(*name)))
                .collect()
        })
        .unwrap_or_default();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "missing required argument(s): {}",
            missing.join(", ")
        ))
    }
}

/// Flatten a tool result to text: content blocks first, falling back to the
/// structured payload so a structured-only server is not reported as empty.
fn flatten(result: &CallToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            ContentBlock::Resource(resource) => serde_json::to_value(&resource.resource)
                .ok()
                .and_then(|value| {
                    value
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }),
            ContentBlock::ResourceLink(link) => Some(format!("[resource] {}", link.uri)),
            // Binary payloads are not model context; note their presence only.
            ContentBlock::Image(_) => Some("[image]".to_string()),
            ContentBlock::Audio(_) => Some("[audio]".to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        return text;
    }
    result
        .structured_content
        .as_ref()
        .and_then(|value| serde_json::to_string_pretty(value).ok())
        .unwrap_or_default()
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

fn qualify(server: &str, tool: &str) -> String {
    format!("{TOOL_PREFIX}{server}__{tool}")
}

fn parse_qualified(name: &str) -> Result<(String, String), Error> {
    let rest = name
        .strip_prefix(TOOL_PREFIX)
        .ok_or_else(|| Error::BadToolName(name.to_string()))?;
    let (server, tool) = rest
        .split_once("__")
        .ok_or_else(|| Error::BadToolName(name.to_string()))?;
    if server.is_empty() || tool.is_empty() {
        return Err(Error::BadToolName(name.to_string()));
    }
    Ok((server.to_string(), tool.to_string()))
}

/// Kill the child's whole process group: `uvx`/`npx` fork the real server, so
/// killing only the direct child leaves an orphan holding the transport.
fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid.filter(|pid| *pid > 0) else {
        return;
    };
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// One Medha-owned directory that MCP servers may write to (under `~/.medha`, so
/// everything Medha writes stays in one place). Package managers (uv/npm) are
/// pointed here via env, so the write jail never opens up the user's real
/// `~/.cache` / `~/.local` and the secrets living beside them.
fn mcp_cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("MEDHA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".medha")))?;
    Some(base.join("mcp-cache"))
}

/// MCP servers get local toolchain discovery plus their explicitly configured
/// environment — never Medha's own API keys or session secrets.
fn sanitized_environment() -> Vec<(String, String)> {
    const ALLOWED: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "SystemRoot",
        "PATHEXT",
    ];
    ALLOWED
        .iter()
        .filter_map(|name| {
            std::env::var_os(name)
                .map(|value| ((*name).to_string(), value.to_string_lossy().into_owned()))
        })
        .collect()
}

/// Reject a remote server URL that would carry credentials in the clear.
/// Exposed so configuration is validated when it is written, not at connect time.
pub fn validate_remote_url(url: &str) -> Result<(), Error> {
    oauth::require_secure(url)
}

pub fn is_url(token: &str) -> bool {
    token.starts_with("https://") || token.starts_with("http://")
}

/// A short server id from a URL, so pasting one is enough to configure it:
/// `https://mcp.linear.app/mcp` → `linear`. Common service prefixes are dropped
/// and the public suffix ignored, which is what a user would have typed anyway.
pub fn id_from_url(url: &str) -> Option<String> {
    let host = url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
    let labels: Vec<&str> = host
        .split('.')
        .filter(|label| !matches!(*label, "mcp" | "api" | "www" | "server"))
        .collect();
    // Take the registrable name, not the TLD: `linear.app` → `linear`.
    let name = labels.first()?;
    (!name.is_empty()).then(|| name.to_string())
}

fn normalize(path: &std::path::Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stdio(command: &[&str]) -> Transport {
        Transport::Stdio {
            command: command.iter().map(|part| part.to_string()).collect(),
            env: Vec::new(),
        }
    }

    fn tool(name: &str) -> Tool {
        Tool::new(name.to_string(), "a tool", Arc::new(serde_json::Map::new()))
    }

    #[test]
    fn qualified_names_round_trip() {
        let name = qualify("github", "search_code");
        assert_eq!(name, "mcp__github__search_code");
        assert_eq!(
            parse_qualified(&name).unwrap(),
            ("github".to_string(), "search_code".to_string())
        );
        assert!(McpManager::is_mcp_tool(&name));
        assert!(!McpManager::is_mcp_tool("fs.read"));
    }

    #[test]
    fn bad_tool_names_are_rejected() {
        assert!(parse_qualified("fs.read").is_err());
        assert!(parse_qualified("mcp__only").is_err());
        assert!(parse_qualified("mcp____tool").is_err());
        assert!(parse_qualified("mcp__server__").is_err());
    }

    #[test]
    fn suspicious_tool_names_never_reach_the_model() {
        assert!(valid_tool_name("search_code"));
        assert!(!valid_tool_name(""));
        assert!(!valid_tool_name("evil__name")); // would break qualified parsing
        assert!(!valid_tool_name("drop\ntable")); // control characters
        assert!(!valid_tool_name(&"x".repeat(129)));
    }

    #[test]
    fn filter_allows_then_denies() {
        let filter = ToolFilter {
            allow: vec!["get_*".into(), "quote".into()],
            deny: vec!["get_secret".into()],
        };
        assert!(filter.admits("get_price"));
        assert!(filter.admits("quote"));
        assert!(!filter.admits("get_secret"));
        assert!(!filter.admits("delete_all"));
        assert!(ToolFilter::default().admits("anything"));
    }

    #[test]
    fn catalog_hides_filtered_and_malformed_tools() {
        let filter = ToolFilter {
            allow: Vec::new(),
            deny: vec!["hidden".into()],
        };
        let catalog = build_catalog(
            "srv",
            &filter,
            vec![tool("visible"), tool("hidden"), tool("bad__name")],
        );
        assert_eq!(catalog.exposed.len(), 1);
        assert_eq!(catalog.exposed[0].name, "mcp__srv__visible");
        assert_eq!(catalog.hidden, 2);
    }

    #[test]
    fn required_arguments_are_checked() {
        let schema = json!({ "type": "object", "required": ["symbol"] });
        assert!(validate_arguments(&schema, &json!({ "symbol": "AAPL" })).is_ok());
        assert!(validate_arguments(&schema, &json!({})).is_err());
        assert!(validate_arguments(&schema, &Value::Null).is_err());
        assert!(validate_arguments(&schema, &json!("oops")).is_err());
        assert!(validate_arguments(&json!({}), &json!({})).is_ok());
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(64), Duration::from_secs(30));
    }

    #[test]
    fn sanitized_environment_excludes_secrets() {
        // SAFETY: single-threaded test; no other thread reads the environment.
        unsafe {
            std::env::set_var("MEDHA_SECRET_TOKEN", "shh");
            std::env::set_var("PATH", "/usr/bin");
        }
        let env = sanitized_environment();
        assert!(env.iter().any(|(k, _)| k == "PATH"));
        assert!(env.iter().all(|(k, _)| k != "MEDHA_SECRET_TOKEN"));
    }

    #[tokio::test]
    async fn disabled_manager_rejects_calls() {
        let manager = McpManager::new(PathBuf::from("."), Config::default());
        assert!(!manager.enabled());
        assert!(matches!(
            manager.call("mcp__x__y", &json!({})).await,
            Err(Error::Disabled)
        ));
    }

    #[tokio::test]
    async fn unready_server_reports_its_state_not_a_missing_server() {
        let manager = McpManager::new(
            PathBuf::from("."),
            Config {
                enabled: true,
                servers: vec![ServerConfig {
                    id: "gated".into(),
                    transport: stdio(&["true"]),
                    requires_approval: true,
                    ..Default::default()
                }],
                ..Config::default()
            },
        );
        let error = manager
            .call("mcp__gated__thing", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(error.server_state(), Some(ServerState::NeedsApproval));
        assert!(matches!(
            manager.call("mcp__absent__thing", &json!({})).await,
            Err(Error::UnknownServer(_))
        ));
    }

    #[tokio::test]
    async fn terminal_failures_do_not_schedule_retries() {
        let manager = McpManager::new(
            PathBuf::from("."),
            Config {
                enabled: true,
                servers: vec![ServerConfig {
                    id: "missing".into(),
                    transport: stdio(&["medha-no-such-binary"]),
                    ..Default::default()
                }],
                ..Config::default()
            },
        );
        manager.connect_startup().await;
        let status = &manager.status().await[0];
        assert_eq!(status.state, ServerState::Failed);
        assert!(
            status
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("unavailable")
        );
        manager.shutdown().await;
    }
}
