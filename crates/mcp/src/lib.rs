//! Supervised Model Context Protocol host for Medha (local stdio servers).
//!
//! Each configured server is a sandboxed child process spoken to over the
//! official `rmcp` stdio transport. Trusted servers connect in parallel at
//! startup (one slow/broken server never stalls the others); approval-gated
//! servers connect only after an explicit human gate. Discovered tools are
//! projected under `mcp__<server>__<tool>` and treated as untrusted/external.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::TokioChildProcess;
use sandbox::{BackendKind, ExecRequest, NetPolicy, SandboxConfig, select_backend};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{process::Command, sync::Mutex, task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;

/// Prefix that marks a tool as MCP-provided and namespaces it by server.
pub const TOOL_PREFIX: &str = "mcp__";

type Client = RunningService<RoleClient, ()>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("MCP is disabled")]
    Disabled,
    #[error("no MCP server named '{0}'")]
    UnknownServer(String),
    #[error("server '{0}' requires approval before it can start")]
    ApprovalRequired(String),
    #[error("'{tool}' is not a tool on MCP server '{server}'")]
    UnknownTool { server: String, tool: String },
    #[error("MCP tool name must be '{TOOL_PREFIX}<server>__<tool>': {0}")]
    BadToolName(String),
    #[error("{server} is unavailable ({command}); {hint}")]
    Unavailable {
        server: String,
        command: String,
        hint: String,
    },
    #[error("MCP command is empty")]
    EmptyCommand,
    #[error("MCP sandbox could not be prepared: {0}")]
    Sandbox(String),
    #[error("failed to start MCP server: {0}")]
    Spawn(String),
    #[error("MCP request failed: {0}")]
    Protocol(String),
    #[error("MCP request timed out after {0:?}")]
    Timeout(Duration),
}

/// One configured server. Built from the lockfile; `requires_approval` gates a
/// project-defined command behind a one-time human preview.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub id: String,
    pub command: Vec<String>,
    /// Explicit environment the server needs (e.g. an API token). Nothing else
    /// from Medha's environment is inherited.
    pub env: Vec<(String, String)>,
    pub requires_approval: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub servers: Vec<ServerConfig>,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub max_text_chars: usize,
    pub allow_network: bool,
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    NeedsApproval,
    Connecting,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    pub server: String,
    pub state: ServerState,
    pub tools: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartPreview {
    pub server: String,
    pub command: Vec<String>,
    pub approval_required: bool,
}

/// A projected MCP tool, ready to expose to the model.
#[derive(Debug, Clone)]
pub struct McpToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// A tool-call result flattened to text, with the server's error flag preserved.
#[derive(Debug, Clone)]
pub struct CallOutput {
    pub server: String,
    pub tool: String,
    pub text: String,
    pub is_error: bool,
    pub truncated: bool,
}

#[derive(Clone)]
struct ToolEntry {
    server: String,
    spec: McpToolSpec,
}

struct Slot {
    config: ServerConfig,
    state: ServerState,
    client: Option<Arc<Client>>,
    detail: Option<String>,
}

struct ManagerInner {
    workspace: PathBuf,
    config: Config,
    servers: Mutex<HashMap<String, Slot>>,
    tools: RwLock<Vec<ToolEntry>>,
    approved: Mutex<HashSet<String>>,
    cancel: CancellationToken,
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        self.cancel.cancel();
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
            .map(|server| {
                let state = if server.requires_approval {
                    ServerState::NeedsApproval
                } else {
                    ServerState::Connecting
                };
                (
                    server.id.clone(),
                    Slot {
                        config: server.clone(),
                        state,
                        client: None,
                        detail: None,
                    },
                )
            })
            .collect();
        Self {
            inner: Arc::new(ManagerInner {
                workspace: normalize(&workspace),
                config,
                servers: Mutex::new(servers),
                tools: RwLock::new(Vec::new()),
                approved: Mutex::new(HashSet::new()),
                cancel: CancellationToken::new(),
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.config.enabled
    }

    /// Connect every trusted (non-approval) server in parallel, bounded by the
    /// startup timeout. Failures are recorded and never block the others.
    pub async fn connect_startup(&self) {
        if !self.inner.config.enabled {
            return;
        }
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
        let _ = timeout(self.inner.config.startup_timeout, async {
            while set.join_next().await.is_some() {}
        })
        .await;
    }

    pub async fn start_preview(&self, server_id: &str) -> Result<StartPreview, Error> {
        let server = self.server_config(server_id).await?;
        Ok(StartPreview {
            server: server.id,
            command: server.command,
            approval_required: server.requires_approval,
        })
    }

    /// Add a server at runtime (from `/mcp add`) and connect it immediately.
    pub async fn add_server(&self, server: ServerConfig) -> Result<ServerStatus, Error> {
        if !self.inner.config.enabled {
            return Err(Error::Disabled);
        }
        self.inner.servers.lock().await.insert(
            server.id.clone(),
            Slot {
                config: server.clone(),
                state: ServerState::Connecting,
                client: None,
                detail: None,
            },
        );
        self.inner.approved.lock().await.insert(server.id.clone());
        self.connect_one(&server).await?;
        Ok(self
            .status()
            .await
            .into_iter()
            .find(|status| status.server == server.id)
            .unwrap_or(ServerStatus {
                server: server.id,
                state: ServerState::Ready,
                tools: 0,
                detail: None,
            }))
    }

    /// Remove a server at runtime (from `/mcp remove`); drops its client (which
    /// tears down the child) and purges its tools.
    pub async fn remove_server(&self, server_id: &str) -> Result<(), Error> {
        let existed = self.inner.servers.lock().await.remove(server_id).is_some();
        if !existed {
            return Err(Error::UnknownServer(server_id.to_string()));
        }
        self.inner
            .tools
            .write()
            .expect("mcp tools lock poisoned")
            .retain(|entry| entry.server != server_id);
        Ok(())
    }

    /// Approve (persist for the session) and connect an approval-gated server.
    pub async fn approve_and_connect(&self, server_id: &str) -> Result<ServerStatus, Error> {
        let server = self.server_config(server_id).await?;
        self.inner.approved.lock().await.insert(server.id.clone());
        self.connect_one(&server).await?;
        Ok(self
            .status()
            .await
            .into_iter()
            .find(|status| status.server == server_id)
            .unwrap_or(ServerStatus {
                server: server_id.to_string(),
                state: ServerState::Ready,
                tools: 0,
                detail: None,
            }))
    }

    /// Currently-connected tools, namespaced. Read each turn to build the
    /// capability sheath, so a server that connects mid-session appears next turn.
    pub fn tool_specs(&self) -> Vec<McpToolSpec> {
        let mut specs: Vec<McpToolSpec> = self
            .inner
            .tools
            .read()
            .expect("mcp tools lock poisoned")
            .iter()
            .map(|entry| entry.spec.clone())
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    pub fn is_mcp_tool(name: &str) -> bool {
        name.starts_with(TOOL_PREFIX)
    }

    /// Invoke `mcp__<server>__<tool>` with the given arguments.
    pub async fn call(&self, qualified: &str, args: &Value) -> Result<CallOutput, Error> {
        if !self.inner.config.enabled {
            return Err(Error::Disabled);
        }
        let (server, tool) = parse_qualified(qualified)?;
        let client = {
            let servers = self.inner.servers.lock().await;
            servers
                .get(&server)
                .and_then(|slot| slot.client.clone())
                .ok_or_else(|| Error::UnknownServer(server.clone()))?
        };
        let mut params = CallToolRequestParams::new(tool.clone());
        if let Some(object) = args.as_object() {
            params = params.with_arguments(object.clone());
        }
        let call = client.call_tool(params);
        let result = match timeout(self.inner.config.request_timeout, call).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Err(Error::Protocol(error.to_string())),
            Err(_) => return Err(Error::Timeout(self.inner.config.request_timeout)),
        };
        Ok(self.flatten_result(&server, &tool, result))
    }

    pub async fn status(&self) -> Vec<ServerStatus> {
        let servers = self.inner.servers.lock().await;
        let tools = self.inner.tools.read().expect("mcp tools lock poisoned");
        let mut out: Vec<ServerStatus> = servers
            .iter()
            .map(|(id, slot)| ServerStatus {
                server: id.clone(),
                state: slot.state,
                tools: tools.iter().filter(|entry| &entry.server == id).count(),
                detail: slot.detail.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.server.cmp(&b.server));
        out
    }

    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        let mut servers = self.inner.servers.lock().await;
        for slot in servers.values_mut() {
            // Dropping the running service aborts its task and (via kill_on_drop)
            // the child process; an explicit cancel needs owned self, so drop is
            // the reliable teardown path.
            slot.client = None;
            slot.state = ServerState::Failed;
        }
        self.inner
            .tools
            .write()
            .expect("mcp tools lock poisoned")
            .clear();
    }

    async fn connect_one(&self, server: &ServerConfig) -> Result<(), Error> {
        self.set_state(&server.id, ServerState::Connecting, None).await;
        match self.spawn_client(server).await {
            Ok((client, tools)) => {
                {
                    let mut slots = self.inner.servers.lock().await;
                    if let Some(slot) = slots.get_mut(&server.id) {
                        slot.client = Some(client);
                        slot.state = ServerState::Ready;
                        slot.detail = None;
                    }
                }
                let mut cache = self.inner.tools.write().expect("mcp tools lock poisoned");
                cache.retain(|entry| entry.server != server.id);
                cache.extend(tools);
                Ok(())
            }
            Err(error) => {
                self.set_state(&server.id, ServerState::Failed, Some(error.to_string()))
                    .await;
                tracing::debug!(target: "medha_mcp", server = %server.id, %error, "MCP server failed to start");
                Err(error)
            }
        }
    }

    async fn spawn_client(&self, server: &ServerConfig) -> Result<(Arc<Client>, Vec<ToolEntry>), Error> {
        let (program, args) = server.command.split_first().ok_or(Error::EmptyCommand)?;
        if !sandbox::program_on_path(program) {
            return Err(Error::Unavailable {
                server: server.id.clone(),
                command: program.clone(),
                hint: "install the configured MCP server executable".into(),
            });
        }
        let command = self.build_command(program, args, &server.env)?;
        let transport = TokioChildProcess::new(command).map_err(|e| Error::Spawn(e.to_string()))?;
        let client = ()
            .serve(transport)
            .await
            .map_err(|e| Error::Spawn(e.to_string()))?;
        let client = Arc::new(client);
        let listed = client
            .list_tools(Default::default())
            .await
            .map_err(|e| Error::Protocol(e.to_string()))?;
        let tools = listed
            .tools
            .into_iter()
            .filter_map(|tool| tool_entry(&server.id, &tool))
            .collect();
        Ok((client, tools))
    }

    fn build_command(
        &self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Command, Error> {
        let net = if self.inner.config.allow_network {
            NetPolicy::Allow
        } else {
            NetPolicy::Deny
        };
        let sandbox_config = SandboxConfig {
            backend: BackendKind::Native,
            net,
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
        if !self.inner.config.allow_network && backend.label() == "host" {
            return Err(Error::Sandbox(
                "network-denied native isolation is unavailable; set mcp.allow_network = true".into(),
            ));
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

    fn flatten_result(
        &self,
        server: &str,
        tool: &str,
        result: rmcp::model::CallToolResult,
    ) -> CallOutput {
        let is_error = result.is_error.unwrap_or(false);
        let text = serde_json::to_value(&result.content)
            .ok()
            .and_then(|value| value.as_array().map(|items| collect_text(items)))
            .unwrap_or_default();
        let cap = self.inner.config.max_text_chars;
        let truncated = text.chars().count() > cap;
        let text = if truncated {
            let cut = text
                .char_indices()
                .nth(cap)
                .map_or(text.len(), |(index, _)| index);
            format!("{}\n… [truncated]", &text[..cut])
        } else {
            text
        };
        CallOutput {
            server: server.to_string(),
            tool: tool.to_string(),
            text,
            is_error,
            truncated,
        }
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

    async fn set_state(&self, id: &str, state: ServerState, detail: Option<String>) {
        let mut servers = self.inner.servers.lock().await;
        if let Some(slot) = servers.get_mut(id) {
            slot.state = state;
            slot.detail = detail;
        }
    }
}

fn tool_entry(server: &str, tool: &rmcp::model::Tool) -> Option<ToolEntry> {
    let value = serde_json::to_value(tool).ok()?;
    let name = value.get("name")?.as_str()?.to_string();
    if name.contains("__") {
        // A raw tool name containing the separator would make the qualified name
        // ambiguous to parse; skip it rather than mis-route a later call.
        return None;
    }
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let schema = value
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object" }));
    Some(ToolEntry {
        server: server.to_string(),
        spec: McpToolSpec {
            name: qualify(server, &name),
            description,
            schema,
        },
    })
}

fn collect_text(items: &[Value]) -> String {
    items
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
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
        "PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "TMP", "TEMP", "LANG", "LC_ALL",
        "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "SystemRoot", "PATHEXT",
    ];
    ALLOWED
        .iter()
        .filter_map(|name| {
            std::env::var_os(name).map(|value| ((*name).to_string(), value.to_string_lossy().into_owned()))
        })
        .collect()
}

fn normalize(path: &std::path::Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
