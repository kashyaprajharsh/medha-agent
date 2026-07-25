//! Supervised Language Server Protocol support for Medha.
//!
//! Medha lazily supervises one client per `(server, project root)`, fans
//! read-only queries across matching servers, synchronizes edited documents,
//! and preserves fresh diagnostic deltas for the coding loop.

use futures::future::join_all;
use sandbox::{BackendKind, ExecRequest, NetPolicy, SandboxConfig, select_backend};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::{DiffTag, TextDiff};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader, BufWriter,
    },
    process::Child,
    sync::{Mutex, Notify, OnceCell, oneshot},
    task::JoinHandle,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;
use url::Url;

type BoxReader = Box<dyn AsyncRead + Send + Unpin>;
type BoxWriter = Box<dyn AsyncWrite + Send + Unpin>;
type Pending = Arc<StdMutex<HashMap<i64, oneshot::Sender<Value>>>>;
type ClientCell = Arc<OnceCell<Result<Arc<LspClient>, Arc<str>>>>;

struct ClientTransport {
    reader: BoxReader,
    writer: BoxWriter,
    child: Option<Child>,
}

#[derive(Clone, Copy)]
struct ClientSettings {
    startup_timeout: Duration,
    request_timeout: Duration,
    diagnostics_timeout: Duration,
    diagnostic_settle: Duration,
    max_results: usize,
    max_text_chars: usize,
    max_open_documents: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    server: String,
    root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Language {
    pub extension: String,
    pub language_id: String,
}

#[derive(Debug, Clone)]
pub struct ServerAdapter {
    pub id: String,
    pub command: Vec<String>,
    pub languages: Vec<Language>,
    pub root_markers: Vec<String>,
    /// Project-provided commands are inert until `lsp.start` passes the human
    /// gate for the resolved `(server, root)` pair.
    pub requires_approval: bool,
    /// Server settings answered to `workspace/configuration` and sent as
    /// `initializationOptions`. `Null` means the server runs on its own
    /// defaults. Sections are resolved by dotted path (e.g. `rust-analyzer.check`).
    pub settings: Value,
}

impl ServerAdapter {
    pub fn rust_analyzer() -> Self {
        Self {
            id: "rust-analyzer".into(),
            command: vec!["rust-analyzer".into()],
            languages: vec![Language {
                extension: "rs".into(),
                language_id: "rust".into(),
            }],
            root_markers: vec![
                "Cargo.toml".into(),
                "rust-project.json".into(),
                ".git".into(),
            ],
            requires_approval: false,
            settings: Value::Null,
        }
    }

    pub fn typescript() -> Self {
        Self {
            id: "typescript-language-server".into(),
            command: vec!["typescript-language-server".into(), "--stdio".into()],
            languages: [
                ("ts", "typescript"),
                ("tsx", "typescriptreact"),
                ("js", "javascript"),
                ("jsx", "javascriptreact"),
                ("mjs", "javascript"),
                ("cjs", "javascript"),
            ]
            .into_iter()
            .map(|(extension, language_id)| Language {
                extension: extension.into(),
                language_id: language_id.into(),
            })
            .collect(),
            root_markers: vec![
                "tsconfig.json".into(),
                "jsconfig.json".into(),
                "package.json".into(),
                ".git".into(),
            ],
            requires_approval: false,
            settings: Value::Null,
        }
    }

    pub fn python() -> Self {
        Self {
            id: "pyright".into(),
            command: vec!["pyright-langserver".into(), "--stdio".into()],
            languages: vec![Language {
                extension: "py".into(),
                language_id: "python".into(),
            }],
            root_markers: vec![
                "pyproject.toml".into(),
                "setup.py".into(),
                "requirements.txt".into(),
                ".git".into(),
            ],
            requires_approval: false,
            settings: Value::Null,
        }
    }

    pub fn go() -> Self {
        Self {
            id: "gopls".into(),
            command: vec!["gopls".into()],
            languages: vec![Language {
                extension: "go".into(),
                language_id: "go".into(),
            }],
            root_markers: vec!["go.work".into(), "go.mod".into(), ".git".into()],
            requires_approval: false,
            settings: Value::Null,
        }
    }

    pub fn clangd() -> Self {
        Self {
            id: "clangd".into(),
            command: vec!["clangd".into()],
            languages: [
                ("c", "c"),
                ("h", "c"),
                ("cc", "cpp"),
                ("cpp", "cpp"),
                ("cxx", "cpp"),
                ("hpp", "cpp"),
                ("hh", "cpp"),
            ]
            .into_iter()
            .map(|(extension, language_id)| Language {
                extension: extension.into(),
                language_id: language_id.into(),
            })
            .collect(),
            root_markers: vec![
                "compile_commands.json".into(),
                "compile_flags.txt".into(),
                "CMakeLists.txt".into(),
                ".git".into(),
            ],
            requires_approval: false,
            settings: Value::Null,
        }
    }

    /// Declarative adapter for a server that needs no special handling: an id, a
    /// command, its extensions and the markers that identify a project root.
    /// Nothing spawns unless the binary is actually on PATH, so listing a server
    /// costs nothing on a machine that does not have it.
    fn simple(
        id: &str,
        command: &[&str],
        languages: &[(&str, &str)],
        root_markers: &[&str],
    ) -> Self {
        Self {
            id: id.into(),
            command: command.iter().map(|part| (*part).to_string()).collect(),
            languages: languages
                .iter()
                .map(|(extension, language_id)| Language {
                    extension: (*extension).into(),
                    language_id: (*language_id).into(),
                })
                .collect(),
            root_markers: root_markers
                .iter()
                .map(|marker| (*marker).to_string())
                .chain(std::iter::once(".git".to_string()))
                .collect(),
            requires_approval: false,
            settings: Value::Null,
        }
    }

    pub fn ruby() -> Self {
        Self::simple(
            "ruby-lsp",
            &["ruby-lsp"],
            &[("rb", "ruby"), ("rake", "ruby"), ("gemspec", "ruby")],
            &["Gemfile", ".ruby-version"],
        )
    }

    pub fn java() -> Self {
        Self::simple(
            "jdtls",
            &["jdtls"],
            &[("java", "java")],
            &["pom.xml", "build.gradle", "build.gradle.kts", ".classpath"],
        )
    }

    pub fn csharp() -> Self {
        Self::simple(
            "omnisharp",
            &["omnisharp", "-lsp"],
            &[("cs", "csharp")],
            // Markers are matched as exact filenames, not globs, so a solution
            // file cannot be named here — `.git` carries these projects.
            &["omnisharp.json", "global.json", "NuGet.config"],
        )
    }

    pub fn php() -> Self {
        Self::simple(
            "intelephense",
            &["intelephense", "--stdio"],
            &[("php", "php")],
            &["composer.json"],
        )
    }

    pub fn lua() -> Self {
        Self::simple(
            "lua-language-server",
            &["lua-language-server"],
            &[("lua", "lua")],
            &[".luarc.json", "stylua.toml"],
        )
    }

    pub fn bash() -> Self {
        Self::simple(
            "bash-language-server",
            &["bash-language-server", "start"],
            &[("sh", "shellscript"), ("bash", "shellscript")],
            &[],
        )
    }

    pub fn zig() -> Self {
        Self::simple(
            "zls",
            &["zls"],
            &[("zig", "zig"), ("zon", "zig")],
            &["build.zig"],
        )
    }

    pub fn swift() -> Self {
        Self::simple(
            "sourcekit-lsp",
            &["sourcekit-lsp"],
            &[("swift", "swift")],
            &["Package.swift"],
        )
    }

    pub fn yaml() -> Self {
        Self::simple(
            "yaml-language-server",
            &["yaml-language-server", "--stdio"],
            &[("yaml", "yaml"), ("yml", "yaml")],
            &[],
        )
    }
}

pub fn language_mappings(names: &[String]) -> Vec<Language> {
    names
        .iter()
        .flat_map(|name| {
            let normalized = name.trim().to_ascii_lowercase();
            let pairs = match normalized.as_str() {
                "rust" => vec![("rs", "rust")],
                "typescript" => vec![("ts", "typescript"), ("tsx", "typescriptreact")],
                "javascript" => vec![
                    ("js", "javascript"),
                    ("jsx", "javascriptreact"),
                    ("mjs", "javascript"),
                    ("cjs", "javascript"),
                ],
                "python" => vec![("py", "python")],
                "go" => vec![("go", "go")],
                "c" => vec![("c", "c"), ("h", "c")],
                "cpp" | "c++" => vec![
                    ("cc", "cpp"),
                    ("cpp", "cpp"),
                    ("cxx", "cpp"),
                    ("hpp", "cpp"),
                    ("hh", "cpp"),
                ],
                _ => Vec::new(),
            };
            if pairs.is_empty() && !normalized.is_empty() {
                return vec![Language {
                    extension: normalized.clone(),
                    language_id: normalized,
                }];
            }
            pairs
                .into_iter()
                .map(|(extension, language_id)| Language {
                    extension: extension.into(),
                    language_id: language_id.into(),
                })
                .collect()
        })
        .collect()
}

struct ReaderState {
    writer: Arc<Mutex<BufWriter<BoxWriter>>>,
    root: PathBuf,
    pending: Pending,
    diagnostics: Arc<StdMutex<HashMap<PathBuf, DiagnosticSnapshot>>>,
    sequence: Arc<AtomicU64>,
    diagnostic_notify: Arc<Notify>,
    alive: Arc<AtomicBool>,
    process_group: Arc<AtomicU64>,
    sync_kind: Arc<AtomicU64>,
    settings: Arc<Value>,
    cancel: CancellationToken,
}

struct ClientEntry {
    cell: ClientCell,
    created_at: Instant,
    last_used: Instant,
    /// Consecutive failed (re)starts; caps the retry rate and parks past the limit.
    failures: u32,
}

/// Ceiling for the exponential restart backoff.
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(60);

/// Runtime configuration. Built-in commands are trusted application defaults;
/// project-defined commands remain approval-gated by the manager.
#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub servers: Vec<ServerAdapter>,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub diagnostics_timeout: Duration,
    pub diagnostic_settle: Duration,
    pub idle_timeout: Duration,
    pub restart_backoff: Duration,
    /// Consecutive failed (re)starts before a server is parked instead of retried.
    pub max_restart_attempts: u32,
    pub max_servers: usize,
    pub max_results: usize,
    pub max_text_chars: usize,
    /// Max documents kept open per server; the least-recently-used is closed
    /// past this to bound language-server memory in long sessions.
    pub max_open_documents: usize,
    pub allow_network: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            // Listing an adapter costs nothing when its binary is absent —
            // nothing spawns and the file simply falls through to the text
            // tools. Coverage is therefore a matter of naming servers, and a
            // narrow list is the only reason a language would silently get no
            // code intelligence at all.
            servers: vec![
                ServerAdapter::rust_analyzer(),
                ServerAdapter::typescript(),
                ServerAdapter::python(),
                ServerAdapter::go(),
                ServerAdapter::clangd(),
                ServerAdapter::ruby(),
                ServerAdapter::java(),
                ServerAdapter::csharp(),
                ServerAdapter::php(),
                ServerAdapter::lua(),
                ServerAdapter::bash(),
                ServerAdapter::zig(),
                ServerAdapter::swift(),
                ServerAdapter::yaml(),
            ],
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(8),
            diagnostics_timeout: Duration::from_secs(4),
            diagnostic_settle: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(10 * 60),
            restart_backoff: Duration::from_secs(5),
            max_restart_attempts: 5,
            // Above the number of built-in adapters, so a polyglot repo does not
            // thrash the LRU just by touching each of its languages once.
            max_servers: 16,
            max_results: 200,
            max_text_chars: 16_000,
            max_open_documents: 64,
            allow_network: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("LSP is disabled")]
    Disabled,
    #[error("path is outside the workspace: {0}")]
    OutsideWorkspace(PathBuf),
    #[error("LSP command is empty")]
    EmptyCommand,
    #[error("no configured language server for: {0}")]
    UnsupportedFile(PathBuf),
    #[error("multiple project language servers match; specify one of: {0}")]
    AmbiguousServers(String),
    #[error("language-server capacity reached ({0})")]
    Capacity(usize),
    #[error("language server '{server}' requires approval before starting in {root}")]
    ApprovalRequired {
        server: String,
        root: PathBuf,
        command: Vec<String>,
    },
    #[error("failed to start language server: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("{server} is unavailable ({command}); {hint}")]
    ServerUnavailable {
        server: String,
        command: String,
        hint: String,
    },
    #[error("language server I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("language server request timed out: {0}")]
    Timeout(&'static str),
    #[error("language server disconnected")]
    Disconnected,
    #[error("language server rejected request: {0}")]
    Protocol(String),
    #[error("language-server sandbox could not be prepared: {0}")]
    Sandbox(String),
    #[error("invalid file URI: {0}")]
    InvalidFileUri(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub range: Range,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
    #[serde(
        default,
        rename = "codeDescription",
        skip_serializing_if = "Option::is_none"
    )]
    pub code_description: Option<CodeDescription>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub message: String,
    /// LSP diagnostic tags (1 = unnecessary, 2 = deprecated).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<u8>,
    /// Secondary spans the server attached to explain the error ("expected
    /// because…"). High-signal context for the agent; ignored by identity.
    #[serde(
        default,
        rename = "relatedInformation",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub related_information: Vec<RelatedInformation>,
    /// Opaque server payload preserved for later code-action resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Diagnostic identity ignores enrichment (related spans, tags, `data`,
/// documentation link). Two diagnostics with the same span, severity, code,
/// source, and message are the same finding, so dedup and introduced/resolved
/// deltas key only on what the compiler reports as the error itself. Attaching
/// this to equality also keeps line-shifted baselines matching across edits.
impl PartialEq for Diagnostic {
    fn eq(&self, other: &Self) -> bool {
        self.range == other.range
            && self.severity == other.severity
            && self.code == other.code
            && self.source == other.source
            && self.message == other.message
    }
}

impl Eq for Diagnostic {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeDescription {
    pub href: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RelatedInformation {
    pub location: Location,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct DiagnosticBaseline {
    entries: Vec<DiagnosticBaselineEntry>,
}

#[derive(Debug, Clone)]
struct DiagnosticBaselineEntry {
    server: String,
    text: String,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Hover {
    pub contents: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
    pub location: Location,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum QueryReport<T> {
    Ready {
        server: String,
        root: PathBuf,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sources: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
        items: Vec<T>,
        #[serde(skip)]
        overflow: Vec<T>,
        total: usize,
        truncated: bool,
    },
    Unavailable {
        reason: String,
    },
    Unsupported {
        path: PathBuf,
    },
}

/// `no_fresh_data` is deliberately different from `fresh` with an empty list.
/// The former must never be interpreted as a clean file.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DiagnosticReport {
    Fresh {
        server: String,
        root: PathBuf,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sources: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
        path: PathBuf,
        version: i64,
        diagnostics: Vec<Diagnostic>,
        #[serde(skip)]
        overflow: Vec<Diagnostic>,
        total: usize,
        truncated: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        introduced: Option<Vec<Diagnostic>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        resolved: Option<Vec<Diagnostic>>,
    },
    NoFreshData {
        server: String,
        root: PathBuf,
        path: PathBuf,
        version: i64,
        waited_ms: u64,
    },
    Unavailable {
        path: PathBuf,
        reason: String,
    },
    Unsupported {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    pub server: String,
    pub root: PathBuf,
    pub state: ServerState,
    pub idle_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerStartPreview {
    pub server: String,
    pub root: PathBuf,
    pub command: Vec<String>,
    pub approval_required: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    Starting,
    Ready,
    Broken,
    Crashed,
}

#[derive(Clone)]
pub struct LspManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    workspace: PathBuf,
    config: Config,
    clients: Mutex<HashMap<ClientKey, ClientEntry>>,
    approved_servers: Mutex<HashSet<ClientKey>>,
    reaper_started: AtomicBool,
    cancel: CancellationToken,
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl LspManager {
    pub fn new(workspace: PathBuf, config: Config) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                workspace: normalize_path(&workspace),
                config,
                clients: Mutex::new(HashMap::new()),
                approved_servers: Mutex::new(HashSet::new()),
                reaper_started: AtomicBool::new(false),
                cancel: CancellationToken::new(),
            }),
        }
    }

    pub fn supports(&self, path: impl AsRef<Path>) -> bool {
        self.supports_path(path.as_ref())
    }

    pub fn start_preview(&self, path: impl AsRef<Path>) -> Result<ServerStartPreview, Error> {
        self.start_preview_for(path, None)
    }

    pub fn start_preview_for(
        &self,
        path: impl AsRef<Path>,
        server_id: Option<&str>,
    ) -> Result<ServerStartPreview, Error> {
        let path = normalize_path_from(&self.inner.workspace, path.as_ref());
        let (adapter, root, _) = self.resolve_start_server(&path, server_id)?;
        Ok(ServerStartPreview {
            server: adapter.id,
            root,
            command: adapter.command,
            approval_required: adapter.requires_approval,
        })
    }

    pub async fn approve_and_start(&self, path: impl AsRef<Path>) -> Result<ServerStatus, Error> {
        self.approve_and_start_for(path, None).await
    }

    pub async fn approve_and_start_for(
        &self,
        path: impl AsRef<Path>,
        server_id: Option<&str>,
    ) -> Result<ServerStatus, Error> {
        let path = normalize_path_from(&self.inner.workspace, path.as_ref());
        let (adapter, root, key) = self.resolve_start_server(&path, server_id)?;
        self.inner.approved_servers.lock().await.insert(key.clone());
        // An explicit start is also the manual refresh: drop a parked entry so it
        // starts clean. Without this, a server that exhausted its restart budget
        // — a few transient crashes during a heavy build is enough — keeps
        // returning its cached error for the rest of the session, with no way
        // back short of restarting Medha.
        let parked = {
            let mut clients = self.inner.clients.lock().await;
            match clients.get(&key) {
                Some(entry) if entry.failures > 0 => clients.remove(&key),
                _ => None,
            }
        };
        if let Some(entry) = parked
            && let Some(Ok(client)) = entry.cell.get()
        {
            client.shutdown().await;
        }
        let client = self
            .client_for_resolved(
                adapter.clone(),
                root.clone(),
                ClientKey {
                    server: adapter.id.clone(),
                    root: root.clone(),
                },
            )
            .await?;
        Ok(ServerStatus {
            server: client.server.clone(),
            root,
            state: ServerState::Ready,
            idle_ms: 0,
            detail: adapter
                .requires_approval
                .then(|| "approved for this session".to_string()),
        })
    }

    /// Return diagnostics produced after synchronizing the current file text.
    pub async fn diagnostics(&self, path: impl AsRef<Path>) -> DiagnosticReport {
        let path = normalize_path_from(&self.inner.workspace, path.as_ref());
        if !self.inner.config.enabled {
            return DiagnosticReport::Unavailable {
                path,
                reason: Error::Disabled.to_string(),
            };
        }
        if !self.supports_path(&path) {
            return DiagnosticReport::Unsupported { path };
        }
        let clients = self.clients_for(&path).await;
        let mut reports = Vec::new();
        let mut calls = Vec::new();
        for result in clients {
            match result {
                Ok(client) => {
                    let path = path.clone();
                    calls.push(async move { client.fresh_diagnostics(&path).await });
                }
                Err(error) => reports.push(DiagnosticReport::Unavailable {
                    path: path.clone(),
                    reason: error.to_string(),
                }),
            }
        }
        reports.extend(join_all(calls).await.into_iter().map(|result| {
            result.unwrap_or_else(|error| DiagnosticReport::Unavailable {
                path: path.clone(),
                reason: error.to_string(),
            })
        }));
        merge_diagnostic_reports(
            reports,
            &path,
            &self.inner.workspace,
            self.inner.config.max_results,
        )
    }

    /// Capture diagnostics for the exact pre-edit text. The opaque baseline is
    /// shifted through the subsequent text diff before introduced/resolved
    /// diagnostics are calculated.
    pub async fn diagnostic_baseline(
        &self,
        path: impl AsRef<Path>,
        text: String,
    ) -> Option<DiagnosticBaseline> {
        let path = normalize_path_from(&self.inner.workspace, path.as_ref());
        if !self.inner.config.enabled || !self.supports_path(&path) {
            return None;
        }
        let clients = self.clients_for(&path).await;
        let calls = clients.into_iter().filter_map(Result::ok).map(|client| {
            let path = path.clone();
            let text = text.clone();
            async move { client.diagnostic_baseline(&path, text).await }
        });
        let mut entries = Vec::new();
        for baseline in join_all(calls)
            .await
            .into_iter()
            .filter_map(Result::ok)
            .flatten()
        {
            entries.extend(baseline.entries);
        }
        (!entries.is_empty()).then_some(DiagnosticBaseline { entries })
    }

    pub async fn diagnostics_after_edit(
        &self,
        path: impl AsRef<Path>,
        new_text: String,
        baseline: Option<DiagnosticBaseline>,
    ) -> DiagnosticReport {
        let path = normalize_path_from(&self.inner.workspace, path.as_ref());
        if !self.inner.config.enabled {
            return DiagnosticReport::Unavailable {
                path,
                reason: Error::Disabled.to_string(),
            };
        }
        if !self.supports_path(&path) {
            return DiagnosticReport::Unsupported { path };
        }
        let clients = self.clients_for(&path).await;
        let mut reports = Vec::new();
        let mut calls = Vec::new();
        for result in clients {
            match result {
                Ok(client) => {
                    let path = path.clone();
                    let text = new_text.clone();
                    let baseline = baseline.clone();
                    calls.push(async move {
                        // Cold server: forward the edit so it warms up, but never
                        // stall the edit waiting for indexing to finish.
                        if !client.is_published() {
                            let version = client.sync_only(&path, text).await?;
                            return Ok::<_, Error>(DiagnosticReport::NoFreshData {
                                server: client.server.clone(),
                                root: client.root.clone(),
                                path: path.clone(),
                                version,
                                waited_ms: 0,
                            });
                        }
                        let report = client
                            .fresh_diagnostics_for_text(&path, text.clone())
                            .await?;
                        Ok::<_, Error>(apply_diagnostic_delta(report, baseline.as_ref(), &text))
                    });
                }
                Err(error) => reports.push(DiagnosticReport::Unavailable {
                    path: path.clone(),
                    reason: error.to_string(),
                }),
            }
        }
        reports.extend(join_all(calls).await.into_iter().map(|result| {
            result.unwrap_or_else(|error| DiagnosticReport::Unavailable {
                path: path.clone(),
                reason: error.to_string(),
            })
        }));
        merge_diagnostic_reports(
            reports,
            &path,
            &self.inner.workspace,
            self.inner.config.max_results,
        )
    }

    pub async fn definition(
        &self,
        path: impl AsRef<Path>,
        position: Position,
    ) -> QueryReport<Location> {
        let path = normalize_path_from(&self.inner.workspace, path.as_ref());
        if let Some(report) = self.unsupported_or_disabled(&path) {
            return report;
        }
        let results = self
            .query_clients(&path, |client| {
                let path = path.clone();
                let position = position.clone();
                async move { client.definition(&path, position).await }
            })
            .await;
        merge_location_reports(
            results,
            &self.inner.workspace,
            self.inner.config.max_results,
        )
    }

    pub async fn references(
        &self,
        path: impl AsRef<Path>,
        position: Position,
        include_declaration: bool,
    ) -> QueryReport<Location> {
        let path = normalize_path_from(&self.inner.workspace, path.as_ref());
        if let Some(report) = self.unsupported_or_disabled(&path) {
            return report;
        }
        let results = self
            .query_clients(&path, |client| {
                let path = path.clone();
                let position = position.clone();
                async move {
                    client
                        .references(&path, position, include_declaration)
                        .await
                }
            })
            .await;
        merge_location_reports(
            results,
            &self.inner.workspace,
            self.inner.config.max_results,
        )
    }

    pub async fn hover(&self, path: impl AsRef<Path>, position: Position) -> QueryReport<Hover> {
        let path = normalize_path_from(&self.inner.workspace, path.as_ref());
        if let Some(report) = self.unsupported_or_disabled(&path) {
            return report;
        }
        let results = self
            .query_clients(&path, |client| {
                let path = path.clone();
                let position = position.clone();
                async move { client.hover(&path, position).await }
            })
            .await;
        merge_hover_reports(
            results,
            &self.inner.workspace,
            self.inner.config.max_results,
        )
    }

    pub async fn workspace_symbols(
        &self,
        context_path: impl AsRef<Path>,
        query: &str,
    ) -> QueryReport<Symbol> {
        let path = normalize_path_from(&self.inner.workspace, context_path.as_ref());
        if !self.inner.config.enabled {
            return unavailable_query(Error::Disabled);
        }
        if !self.supports_path(&path) {
            return QueryReport::Unsupported { path };
        }
        let query = query.to_string();
        let results = self
            .query_clients(&path, |client| {
                let query = query.clone();
                async move { client.workspace_symbols(&query).await }
            })
            .await;
        merge_symbol_reports(
            results,
            &self.inner.workspace,
            self.inner.config.max_results,
        )
    }

    pub async fn implementations(
        &self,
        path: impl AsRef<Path>,
        position: Position,
    ) -> QueryReport<Location> {
        let path = normalize_path_from(&self.inner.workspace, path.as_ref());
        if let Some(report) = self.unsupported_or_disabled(&path) {
            return report;
        }
        let results = self
            .query_clients(&path, |client| {
                let path = path.clone();
                let position = position.clone();
                async move { client.implementation(&path, position).await }
            })
            .await;
        merge_location_reports(
            results,
            &self.inner.workspace,
            self.inner.config.max_results,
        )
    }

    pub async fn document_symbols(&self, path: impl AsRef<Path>) -> QueryReport<Symbol> {
        let path = normalize_path_from(&self.inner.workspace, path.as_ref());
        if let Some(report) = self.unsupported_or_disabled(&path) {
            return report;
        }
        let results = self
            .query_clients(&path, |client| {
                let path = path.clone();
                async move { client.document_symbols(&path).await }
            })
            .await;
        merge_symbol_reports(
            results,
            &self.inner.workspace,
            self.inner.config.max_results,
        )
    }

    pub async fn call_hierarchy(
        &self,
        path: impl AsRef<Path>,
        position: Position,
        outgoing: bool,
    ) -> QueryReport<Symbol> {
        let path = normalize_path_from(&self.inner.workspace, path.as_ref());
        if let Some(report) = self.unsupported_or_disabled(&path) {
            return report;
        }
        let results = self
            .query_clients(&path, |client| {
                let path = path.clone();
                let position = position.clone();
                async move { client.call_hierarchy(&path, position, outgoing).await }
            })
            .await;
        merge_symbol_reports(
            results,
            &self.inner.workspace,
            self.inner.config.max_results,
        )
    }

    async fn query_clients<T, F, Fut>(&self, path: &Path, operation: F) -> Vec<QueryReport<T>>
    where
        F: Fn(Arc<LspClient>) -> Fut,
        Fut: std::future::Future<Output = Result<QueryReport<T>, Error>>,
    {
        let clients = self.clients_for(path).await;
        let mut reports = Vec::new();
        let mut calls = Vec::new();
        for result in clients {
            match result {
                Ok(client) => calls.push(operation(client)),
                Err(error) => reports.push(unavailable_query(error)),
            }
        }
        reports.extend(
            join_all(calls)
                .await
                .into_iter()
                .map(|result| result.unwrap_or_else(unavailable_query)),
        );
        reports
    }

    fn unsupported_or_disabled<T>(&self, path: &Path) -> Option<QueryReport<T>> {
        if !self.inner.config.enabled {
            return Some(unavailable_query(Error::Disabled));
        }
        (!self.supports_path(path)).then(|| QueryReport::Unsupported {
            path: path.to_path_buf(),
        })
    }

    fn supports_path(&self, path: &Path) -> bool {
        self.inner
            .config
            .servers
            .iter()
            .any(|adapter| adapter_language_id(adapter, path).is_some())
    }

    pub async fn status(&self) -> Vec<ServerStatus> {
        self.ensure_reaper();
        self.reap_idle().await;
        let now = Instant::now();
        let entries = {
            let clients = self.inner.clients.lock().await;
            clients
                .iter()
                .map(|(key, entry)| {
                    (
                        key.clone(),
                        Arc::clone(&entry.cell),
                        now.saturating_duration_since(entry.last_used),
                    )
                })
                .collect::<Vec<_>>()
        };
        entries
            .into_iter()
            .map(|(key, cell, idle)| match cell.get() {
                None => ServerStatus {
                    server: key.server,
                    root: key.root,
                    state: ServerState::Starting,
                    idle_ms: idle.as_millis() as u64,
                    detail: None,
                },
                Some(Ok(client)) => {
                    let alive = client.is_alive();
                    ServerStatus {
                        server: client.server.clone(),
                        root: key.root,
                        state: if alive {
                            ServerState::Ready
                        } else {
                            ServerState::Crashed
                        },
                        idle_ms: idle.as_millis() as u64,
                        detail: (!alive).then(|| "language server disconnected".to_string()),
                    }
                }
                Some(Err(error)) => ServerStatus {
                    server: key.server,
                    root: key.root,
                    state: ServerState::Broken,
                    idle_ms: idle.as_millis() as u64,
                    detail: Some(error.to_string()),
                },
            })
            .collect()
    }

    pub async fn shutdown_all(&self) {
        let entries = {
            let mut clients = self.inner.clients.lock().await;
            clients.drain().map(|(_, entry)| entry).collect::<Vec<_>>()
        };
        let clients = entries
            .iter()
            .filter_map(|entry| entry.cell.get())
            .filter_map(|result| result.as_ref().ok())
            .cloned()
            .collect::<Vec<_>>();
        for client in clients {
            client.shutdown().await;
        }
    }

    async fn clients_for(&self, path: &Path) -> Vec<Result<Arc<LspClient>, Error>> {
        self.ensure_reaper();
        self.reap_idle().await;
        let resolved = match self.resolve_servers(path) {
            Ok(resolved) => resolved,
            Err(error) => return vec![Err(error)],
        };
        join_all(
            resolved
                .into_iter()
                .map(|(adapter, root, key)| self.client_for_resolved(adapter, root, key)),
        )
        .await
    }

    async fn client_for_resolved(
        &self,
        adapter: ServerAdapter,
        root: PathBuf,
        key: ClientKey,
    ) -> Result<Arc<LspClient>, Error> {
        if adapter.requires_approval && !self.inner.approved_servers.lock().await.contains(&key) {
            return Err(Error::ApprovalRequired {
                server: adapter.id,
                root,
                command: adapter.command,
            });
        }
        let now = Instant::now();
        let mut evicted = None;
        let (cell, retired) = {
            let mut clients = self.inner.clients.lock().await;
            let config = &self.inner.config;
            // Replace a broken server only once its escalating backoff has elapsed;
            // past `max_restart_attempts` it stays parked (broken, no respawn).
            let should_replace = clients.get(&key).is_some_and(|entry| {
                let broken = match entry.cell.get() {
                    Some(Err(_)) => true,
                    Some(Ok(client)) => !client.is_alive(),
                    None => false,
                };
                let backoff = config
                    .restart_backoff
                    .saturating_mul(1u32 << entry.failures.min(6))
                    .min(MAX_RESTART_BACKOFF);
                broken
                    && entry.failures < config.max_restart_attempts
                    && now.saturating_duration_since(entry.created_at) >= backoff
            });
            let retired = should_replace.then(|| clients.remove(&key)).flatten();
            let next_failures = retired
                .as_ref()
                .map_or(0, |entry| entry.failures.saturating_add(1));
            // At capacity, retire the least recently used server rather than
            // refusing. A polyglot repo can easily touch more languages than the
            // ceiling, and denying code intelligence for the ninth one — while
            // some server nobody has queried in an hour holds its slot — is the
            // wrong trade. Only an idle server is evicted; one in use keeps its
            // slot because `last_used` was just bumped.
            if !clients.contains_key(&key) && clients.len() >= config.max_servers {
                let victim = clients
                    .iter()
                    .filter(|(_, entry)| entry.cell.get().is_some())
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(key, _)| key.clone());
                match victim.and_then(|key| clients.remove(&key)) {
                    Some(entry) => evicted = Some(entry),
                    // Every slot is held by a server still starting up; refusing
                    // is right here, since evicting one would abort a spawn the
                    // caller is waiting on.
                    None => return Err(Error::Capacity(config.max_servers)),
                }
            }
            let entry = clients.entry(key).or_insert_with(|| ClientEntry {
                cell: Arc::new(OnceCell::new()),
                created_at: now,
                last_used: now,
                failures: next_failures,
            });
            entry.last_used = now;
            (Arc::clone(&entry.cell), retired)
        };
        // Shut the outgoing servers down outside the map lock — both the broken
        // one being replaced and any evicted to make room.
        for entry in [retired, evicted].into_iter().flatten() {
            if let Some(Ok(client)) = entry.cell.get() {
                client.shutdown().await;
            }
        }
        let config = self.inner.config.clone();
        let result = cell
            .get_or_init(|| async {
                LspClient::start(root, adapter, config)
                    .await
                    .map(Arc::new)
                    .map_err(|error| Arc::<str>::from(error.to_string()))
            })
            .await;
        result
            .as_ref()
            .cloned()
            .map_err(|error| Error::Protocol(error.to_string()))
    }

    fn resolve_start_server(
        &self,
        path: &Path,
        server_id: Option<&str>,
    ) -> Result<(ServerAdapter, PathBuf, ClientKey), Error> {
        let matching = self.resolve_servers(path)?;
        if let Some(server_id) = server_id {
            return matching
                .into_iter()
                .find(|(adapter, _, _)| adapter.id == server_id)
                .ok_or_else(|| Error::UnsupportedFile(path.to_path_buf()));
        }
        let mut approval_required = matching
            .iter()
            .filter(|(adapter, _, _)| adapter.requires_approval)
            .cloned()
            .collect::<Vec<_>>();
        match approval_required.len() {
            0 if matching.len() == 1 => Ok(matching.into_iter().next().expect("one server")),
            1 => Ok(approval_required
                .pop()
                .expect("one approval-required server")),
            _ => {
                let mut ids = if approval_required.is_empty() {
                    matching
                        .into_iter()
                        .map(|(adapter, _, _)| adapter.id)
                        .collect::<Vec<_>>()
                } else {
                    approval_required
                        .into_iter()
                        .map(|(adapter, _, _)| adapter.id)
                        .collect::<Vec<_>>()
                };
                ids.sort();
                ids.dedup();
                Err(Error::AmbiguousServers(ids.join(", ")))
            }
        }
    }

    fn resolve_servers(
        &self,
        path: &Path,
    ) -> Result<Vec<(ServerAdapter, PathBuf, ClientKey)>, Error> {
        if !self.inner.config.enabled {
            return Err(Error::Disabled);
        }
        if !path.starts_with(&self.inner.workspace) {
            return Err(Error::OutsideWorkspace(path.to_path_buf()));
        }
        let adapters = self
            .inner
            .config
            .servers
            .iter()
            .filter(|adapter| adapter_language_id(adapter, path).is_some())
            .cloned()
            .collect::<Vec<_>>();
        if adapters.is_empty() {
            return Err(Error::UnsupportedFile(path.to_path_buf()));
        }
        Ok(adapters
            .into_iter()
            .map(|adapter| {
                let root = detect_root(path, &self.inner.workspace, &adapter.root_markers);
                let key = ClientKey {
                    server: adapter.id.clone(),
                    root: root.clone(),
                };
                (adapter, root, key)
            })
            .collect())
    }

    fn ensure_reaper(&self) {
        if self.inner.reaper_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let cancel = self.inner.cancel.clone();
        let tick = self
            .inner
            .config
            .idle_timeout
            .checked_div(2)
            .unwrap_or(Duration::from_secs(1))
            .clamp(Duration::from_secs(1), Duration::from_secs(60));
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = interval.tick() => {
                        let Some(inner) = weak.upgrade() else { break };
                        reap_idle_inner(&inner).await;
                    }
                }
            }
        });
    }

    async fn reap_idle(&self) {
        reap_idle_inner(&self.inner).await;
    }
}

fn unavailable_query<T>(error: Error) -> QueryReport<T> {
    QueryReport::Unavailable {
        reason: error.to_string(),
    }
}

fn merge_diagnostic_reports(
    reports: Vec<DiagnosticReport>,
    path: &Path,
    workspace: &Path,
    max_results: usize,
) -> DiagnosticReport {
    let mut diagnostics = Vec::new();
    let mut introduced = Vec::new();
    let mut resolved = Vec::new();
    let mut has_delta = false;
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let mut version = 0;
    let mut waited_ms = 0;
    let mut reported_total = 0;
    let mut input_truncated = false;
    let mut no_fresh_sources = Vec::new();

    for report in reports {
        match report {
            DiagnosticReport::Fresh {
                server,
                sources: report_sources,
                warnings: report_warnings,
                version: report_version,
                diagnostics: report_diagnostics,
                overflow: report_overflow,
                total,
                truncated,
                introduced: report_introduced,
                resolved: report_resolved,
                ..
            } => {
                sources.extend(if report_sources.is_empty() {
                    vec![server]
                } else {
                    report_sources
                });
                warnings.extend(report_warnings);
                diagnostics.extend(report_diagnostics);
                diagnostics.extend(report_overflow);
                reported_total += total;
                input_truncated |= truncated;
                version = version.max(report_version);
                if let Some(items) = report_introduced {
                    has_delta = true;
                    introduced.extend(items);
                }
                if let Some(items) = report_resolved {
                    has_delta = true;
                    resolved.extend(items);
                }
            }
            DiagnosticReport::NoFreshData {
                server,
                waited_ms: report_waited,
                version: report_version,
                ..
            } => {
                no_fresh_sources.push(server.clone());
                warnings.push(format!("{server}: no fresh diagnostic data"));
                version = version.max(report_version);
                waited_ms = waited_ms.max(report_waited);
            }
            DiagnosticReport::Unavailable { reason, .. } => warnings.push(reason),
            DiagnosticReport::Unsupported { path } => {
                warnings.push(format!("unsupported file: {}", path.display()));
            }
        }
    }

    sources.sort();
    sources.dedup();
    warnings.sort();
    warnings.dedup();
    if sources.is_empty() {
        no_fresh_sources.sort();
        no_fresh_sources.dedup();
        if !no_fresh_sources.is_empty() {
            return DiagnosticReport::NoFreshData {
                server: no_fresh_sources.join("+"),
                root: workspace.to_path_buf(),
                path: path.to_path_buf(),
                version,
                waited_ms,
            };
        }
        return DiagnosticReport::Unavailable {
            path: path.to_path_buf(),
            reason: if warnings.is_empty() {
                "no matching language server was available".into()
            } else {
                warnings.join("; ")
            },
        };
    }

    sort_dedupe_diagnostics(&mut diagnostics);
    sort_dedupe_diagnostics(&mut introduced);
    sort_dedupe_diagnostics(&mut resolved);
    let known_total = diagnostics.len();
    let overflow = if diagnostics.len() > max_results {
        diagnostics.split_off(max_results)
    } else {
        Vec::new()
    };
    introduced.truncate(max_results);
    resolved.truncate(max_results);
    DiagnosticReport::Fresh {
        server: sources.join("+"),
        root: workspace.to_path_buf(),
        sources,
        warnings,
        path: path.to_path_buf(),
        version,
        diagnostics,
        overflow,
        total: reported_total.max(known_total),
        truncated: input_truncated || known_total > max_results,
        introduced: has_delta.then_some(introduced),
        resolved: has_delta.then_some(resolved),
    }
}

fn merge_location_reports(
    reports: Vec<QueryReport<Location>>,
    workspace: &Path,
    max_results: usize,
) -> QueryReport<Location> {
    let mut items = Vec::new();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let mut reported_total = 0;
    let mut input_truncated = false;
    for report in reports {
        match report {
            QueryReport::Ready {
                sources: report_sources,
                warnings: report_warnings,
                items: report_items,
                overflow,
                total,
                truncated,
                ..
            } => {
                sources.extend(report_sources);
                warnings.extend(report_warnings);
                items.extend(report_items);
                items.extend(overflow);
                reported_total += total;
                input_truncated |= truncated;
            }
            QueryReport::Unavailable { reason } => warnings.push(reason),
            QueryReport::Unsupported { path } => {
                warnings.push(format!("unsupported file: {}", path.display()));
            }
        }
    }
    sort_dedupe_locations(&mut items);
    finish_merged_report(
        items,
        Vec::new(),
        MergeReportMeta {
            sources,
            warnings,
            reported_total,
            input_truncated,
            workspace,
            max_results,
        },
    )
}

fn merge_hover_reports(
    reports: Vec<QueryReport<Hover>>,
    workspace: &Path,
    max_results: usize,
) -> QueryReport<Hover> {
    let mut items = Vec::new();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let mut reported_total = 0;
    let mut input_truncated = false;
    let mut overflow = Vec::new();
    for report in reports {
        match report {
            QueryReport::Ready {
                sources: report_sources,
                warnings: report_warnings,
                items: report_items,
                overflow: report_overflow,
                total,
                truncated,
                ..
            } => {
                sources.extend(report_sources);
                warnings.extend(report_warnings);
                items.extend(report_items);
                overflow.extend(report_overflow);
                reported_total += total;
                input_truncated |= truncated;
            }
            QueryReport::Unavailable { reason } => warnings.push(reason),
            QueryReport::Unsupported { path } => {
                warnings.push(format!("unsupported file: {}", path.display()));
            }
        }
    }
    items.sort_by(|left, right| {
        left.contents
            .cmp(&right.contents)
            .then_with(|| left.range.cmp(&right.range))
    });
    items.dedup();
    finish_merged_report(
        items,
        overflow,
        MergeReportMeta {
            sources,
            warnings,
            reported_total,
            input_truncated,
            workspace,
            max_results,
        },
    )
}

fn merge_symbol_reports(
    reports: Vec<QueryReport<Symbol>>,
    workspace: &Path,
    max_results: usize,
) -> QueryReport<Symbol> {
    let mut items = Vec::new();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let mut reported_total = 0;
    let mut input_truncated = false;
    for report in reports {
        match report {
            QueryReport::Ready {
                sources: report_sources,
                warnings: report_warnings,
                items: report_items,
                overflow,
                total,
                truncated,
                ..
            } => {
                sources.extend(report_sources);
                warnings.extend(report_warnings);
                items.extend(report_items);
                items.extend(overflow);
                reported_total += total;
                input_truncated |= truncated;
            }
            QueryReport::Unavailable { reason } => warnings.push(reason),
            QueryReport::Unsupported { path } => {
                warnings.push(format!("unsupported file: {}", path.display()));
            }
        }
    }
    items.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.location.path.cmp(&right.location.path))
            .then_with(|| {
                left.location
                    .range
                    .start
                    .line
                    .cmp(&right.location.range.start.line)
            })
            .then_with(|| {
                left.location
                    .range
                    .start
                    .character
                    .cmp(&right.location.range.start.character)
            })
    });
    items.dedup();
    finish_merged_report(
        items,
        Vec::new(),
        MergeReportMeta {
            sources,
            warnings,
            reported_total,
            input_truncated,
            workspace,
            max_results,
        },
    )
}

struct MergeReportMeta<'a> {
    sources: Vec<String>,
    warnings: Vec<String>,
    reported_total: usize,
    input_truncated: bool,
    workspace: &'a Path,
    max_results: usize,
}

fn finish_merged_report<T>(
    mut items: Vec<T>,
    mut overflow: Vec<T>,
    meta: MergeReportMeta<'_>,
) -> QueryReport<T> {
    let MergeReportMeta {
        mut sources,
        mut warnings,
        reported_total,
        input_truncated,
        workspace,
        max_results,
    } = meta;
    sources.sort();
    sources.dedup();
    warnings.sort();
    warnings.dedup();
    if sources.is_empty() {
        return QueryReport::Unavailable {
            reason: if warnings.is_empty() {
                "no matching language server was available".into()
            } else {
                warnings.join("; ")
            },
        };
    }
    let known_total = items.len();
    if items.len() > max_results {
        overflow.extend(items.split_off(max_results));
    }
    QueryReport::Ready {
        server: sources.join("+"),
        root: workspace.to_path_buf(),
        sources,
        warnings,
        items,
        overflow,
        total: reported_total.max(known_total),
        truncated: input_truncated || known_total > max_results,
    }
}

async fn reap_idle_inner(inner: &Arc<ManagerInner>) {
    let now = Instant::now();
    let retired = {
        let mut clients = inner.clients.lock().await;
        let keys = clients
            .iter()
            .filter_map(|(key, entry)| {
                let ready = matches!(entry.cell.get(), Some(Ok(_)));
                (ready
                    && now.saturating_duration_since(entry.last_used) >= inner.config.idle_timeout)
                    .then(|| key.clone())
            })
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| clients.remove(&key))
            .collect::<Vec<_>>()
    };
    for entry in retired {
        if let Some(Ok(client)) = entry.cell.get() {
            client.shutdown().await;
        }
    }
}

#[derive(Debug, Clone)]
struct Document {
    version: i64,
    text: String,
    /// Monotonic access stamp for LRU eviction.
    touched: u64,
}

struct DocumentSync {
    version: i64,
    opened: bool,
}

#[derive(Debug, Clone)]
struct DiagnosticSnapshot {
    sequence: u64,
    version: Option<i64>,
    diagnostics: Vec<Diagnostic>,
}

struct LspClient {
    server: String,
    root: PathBuf,
    language_ids: HashMap<String, String>,
    writer: Arc<Mutex<BufWriter<BoxWriter>>>,
    child: Mutex<Option<Child>>,
    pending: Pending,
    documents: Mutex<HashMap<PathBuf, Document>>,
    diagnostics: Arc<StdMutex<HashMap<PathBuf, DiagnosticSnapshot>>>,
    diagnostic_sequence: Arc<AtomicU64>,
    diagnostic_notify: Arc<Notify>,
    next_id: AtomicI64,
    startup_timeout: Duration,
    request_timeout: Duration,
    diagnostics_timeout: Duration,
    diagnostic_settle: Duration,
    max_results: usize,
    max_text_chars: usize,
    max_open_documents: usize,
    document_clock: AtomicU64,
    alive: Arc<AtomicBool>,
    process_group: Arc<AtomicU64>,
    sync_kind: Arc<AtomicU64>,
    settings: Arc<Value>,
    cancel: CancellationToken,
    reader_task: JoinHandle<()>,
    stderr_task: Option<JoinHandle<()>>,
}

impl LspClient {
    async fn start(root: PathBuf, adapter: ServerAdapter, config: Config) -> Result<Self, Error> {
        let (program, arguments) = adapter.command.split_first().ok_or(Error::EmptyCommand)?;
        let resolved_program = {
            let candidate = Path::new(program);
            if candidate.is_relative() && candidate.components().count() > 1 {
                root.join(candidate).to_string_lossy().into_owned()
            } else {
                program.clone()
            }
        };
        if !server_on_path(&resolved_program) {
            return Err(Error::ServerUnavailable {
                server: adapter.id.clone(),
                command: program.clone(),
                hint: server_install_hint(&adapter.id).to_string(),
            });
        }
        let sandbox_config = SandboxConfig {
            backend: BackendKind::Native,
            net: if config.allow_network {
                NetPolicy::Allow
            } else {
                NetPolicy::Deny
            },
            ..SandboxConfig::default()
        };
        let backend = select_backend(&sandbox_config, Vec::new());
        if !config.allow_network && backend.label() == "host" {
            return Err(Error::Sandbox(
                "network-denied native isolation is unavailable; explicitly set lsp.allow_network = true to run without it"
                    .into(),
            ));
        }
        let request = ExecRequest {
            program: resolved_program,
            args: arguments.to_vec(),
            cwd: root.clone(),
            env: language_server_environment(),
            clear_env: true,
        };
        let mut command = backend
            .build_command(&request)
            .map_err(|error| Error::Sandbox(error.to_string()))?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::ServerUnavailable {
                    server: adapter.id.clone(),
                    command: program.clone(),
                    hint: server_install_hint(&adapter.id).to_string(),
                }
            } else {
                Error::Spawn(error)
            }
        })?;
        let stdin = child.stdin.take().ok_or(Error::Disconnected)?;
        let stdout = child.stdout.take().ok_or(Error::Disconnected)?;
        let stderr = child.stderr.take();

        let configuration = adapter.settings;
        let mut client = Self::from_io(
            adapter.id,
            root,
            adapter
                .languages
                .into_iter()
                .map(|language| (language.extension, language.language_id))
                .collect(),
            ClientTransport {
                reader: Box::new(stdout),
                writer: Box::new(stdin),
                child: Some(child),
            },
            ClientSettings {
                startup_timeout: config.startup_timeout,
                request_timeout: config.request_timeout,
                diagnostics_timeout: config.diagnostics_timeout,
                diagnostic_settle: config.diagnostic_settle,
                max_results: config.max_results,
                max_text_chars: config.max_text_chars,
                max_open_documents: config.max_open_documents,
            },
            configuration,
        );
        if let Some(stderr) = stderr {
            let cancel = client.cancel.clone();
            client.stderr_task = Some(tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        line = lines.next_line() => match line {
                            Ok(Some(line)) => tracing::debug!(target: "medha_lsp", "{line}"),
                            _ => break,
                        }
                    }
                }
            }));
        }
        client.initialize().await?;
        Ok(client)
    }

    fn from_io(
        server: String,
        root: PathBuf,
        language_ids: HashMap<String, String>,
        transport: ClientTransport,
        settings: ClientSettings,
        configuration: Value,
    ) -> Self {
        let ClientTransport {
            reader,
            writer,
            child,
        } = transport;
        let configuration = Arc::new(configuration);
        let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
        let diagnostics = Arc::new(StdMutex::new(HashMap::new()));
        let diagnostic_sequence = Arc::new(AtomicU64::new(0));
        let diagnostic_notify = Arc::new(Notify::new());
        let alive = Arc::new(AtomicBool::new(true));
        let process_group = Arc::new(AtomicU64::new(
            child.as_ref().and_then(Child::id).unwrap_or(0) as u64,
        ));
        let sync_kind = Arc::new(AtomicU64::new(1));
        let cancel = CancellationToken::new();
        let writer = Arc::new(Mutex::new(BufWriter::new(writer)));
        let reader_task = tokio::spawn(reader_loop(
            reader,
            ReaderState {
                writer: Arc::clone(&writer),
                root: root.clone(),
                pending: Arc::clone(&pending),
                diagnostics: Arc::clone(&diagnostics),
                sequence: Arc::clone(&diagnostic_sequence),
                diagnostic_notify: Arc::clone(&diagnostic_notify),
                alive: Arc::clone(&alive),
                process_group: Arc::clone(&process_group),
                sync_kind: Arc::clone(&sync_kind),
                settings: Arc::clone(&configuration),
                cancel: cancel.clone(),
            },
        ));
        Self {
            server,
            root,
            language_ids,
            writer,
            child: Mutex::new(child),
            pending,
            documents: Mutex::new(HashMap::new()),
            diagnostics,
            diagnostic_sequence,
            diagnostic_notify,
            next_id: AtomicI64::new(1),
            startup_timeout: settings.startup_timeout,
            request_timeout: settings.request_timeout,
            diagnostics_timeout: settings.diagnostics_timeout,
            diagnostic_settle: settings.diagnostic_settle,
            max_results: settings.max_results,
            max_text_chars: settings.max_text_chars,
            max_open_documents: settings.max_open_documents,
            document_clock: AtomicU64::new(0),
            alive,
            process_group,
            sync_kind,
            settings: configuration,
            cancel,
            reader_task,
            stderr_task: None,
        }
    }

    async fn initialize(&self) -> Result<(), Error> {
        let root_uri = file_uri(&self.root)?;
        let mut params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "medha", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": root_uri,
            "capabilities": {
                "workspace": {
                    "configuration": true,
                    "workspaceFolders": true,
                    "symbol": {
                        "dynamicRegistration": false,
                        "resolveSupport": { "properties": ["location.range"] }
                    }
                },
                "textDocument": {
                    "synchronization": { "dynamicRegistration": true, "didSave": true },
                    "publishDiagnostics": {
                        "versionSupport": true,
                        "relatedInformation": true,
                        "codeDescriptionSupport": true,
                        "tagSupport": { "valueSet": [1, 2] }
                    },
                    "definition": { "dynamicRegistration": false, "linkSupport": true },
                    "implementation": { "dynamicRegistration": false, "linkSupport": true },
                    "references": { "dynamicRegistration": false },
                    "documentSymbol": {
                        "dynamicRegistration": false,
                        "hierarchicalDocumentSymbolSupport": true
                    },
                    "callHierarchy": { "dynamicRegistration": false },
                    "hover": {
                        "dynamicRegistration": false,
                        "contentFormat": ["markdown", "plaintext"]
                    },
                    "diagnostic": {
                        "dynamicRegistration": true,
                        "relatedDocumentSupport": false
                    }
                }
            },
            "workspaceFolders": [{
                "uri": root_uri,
                "name": self.root.file_name().and_then(|v| v.to_str()).unwrap_or("workspace")
            }]
        });
        if !self.settings.is_null() {
            params["initializationOptions"] = (*self.settings).clone();
        }
        let initialized = self
            .request("initialize", params, self.startup_timeout)
            .await?;
        let sync_kind = initialized
            .pointer("/capabilities/textDocumentSync/change")
            .or_else(|| initialized.pointer("/capabilities/textDocumentSync"))
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .min(2);
        self.sync_kind.store(sync_kind, Ordering::Release);
        self.notify("initialized", json!({})).await
    }

    async fn fresh_diagnostics(&self, path: &Path) -> Result<DiagnosticReport, Error> {
        let text = tokio::fs::read_to_string(path).await?;
        self.fresh_diagnostics_for_text(path, text).await
    }

    /// True once the server has published diagnostics at least once, i.e. it has
    /// finished cold-start indexing enough to respond. Used to keep post-edit
    /// feedback from stalling the edit while the server is still warming up.
    fn is_published(&self) -> bool {
        self.diagnostic_sequence.load(Ordering::Acquire) > 0
    }

    /// Forward the current text to the server without waiting for diagnostics.
    /// Warms a cold server so the next edit/query returns fresh results fast.
    async fn sync_only(&self, path: &Path, text: String) -> Result<i64, Error> {
        let sync = self.sync_document(path, text).await?;
        if !sync.opened {
            self.notify(
                "textDocument/didSave",
                json!({ "textDocument": { "uri": file_uri(path)? } }),
            )
            .await?;
        }
        Ok(sync.version)
    }

    async fn fresh_diagnostics_for_text(
        &self,
        path: &Path,
        text: String,
    ) -> Result<DiagnosticReport, Error> {
        let before_sequence = self.diagnostic_sequence.load(Ordering::Acquire);
        let sync = self.sync_document(path, text).await?;
        let version = sync.version;
        // `didOpen` already schedules analysis. Sending an immediate `didSave`
        // makes some servers (notably clangd) start a redundant rebuild while
        // navigation requests arrive. Existing documents still receive save so
        // save-oriented servers such as rust-analyzer refresh after edits.
        if !sync.opened {
            self.notify(
                "textDocument/didSave",
                json!({ "textDocument": { "uri": file_uri(path)? } }),
            )
            .await?;
        }

        let started = Instant::now();
        let wait = async {
            let clean_settle = self.diagnostics_timeout.min(Duration::from_secs(2));
            let diagnostic_settle = self.diagnostics_timeout.min(self.diagnostic_settle);
            let mut candidate: Option<(u64, Instant, DiagnosticSnapshot)> = None;
            loop {
                let notified = self.diagnostic_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(snapshot) = self.snapshot_after(path, before_sequence, version) {
                    let replace = candidate
                        .as_ref()
                        .is_none_or(|(sequence, _, _)| *sequence != snapshot.sequence);
                    if replace {
                        let settle = if snapshot.diagnostics.is_empty() {
                            clean_settle
                        } else {
                            diagnostic_settle
                        };
                        candidate = Some((snapshot.sequence, Instant::now() + settle, snapshot));
                    }
                }
                if let Some((_, deadline, snapshot)) = &candidate {
                    tokio::select! {
                        _ = &mut notified => {}
                        _ = tokio::time::sleep_until(*deadline) => return snapshot.clone(),
                    }
                } else {
                    notified.await;
                }
            }
        };
        match timeout(self.diagnostics_timeout, wait).await {
            Ok(snapshot) => {
                let mut diagnostics = snapshot.diagnostics;
                sort_dedupe_diagnostics(&mut diagnostics);
                let total = diagnostics.len();
                let overflow = if diagnostics.len() > self.max_results {
                    diagnostics.split_off(self.max_results)
                } else {
                    Vec::new()
                };
                Ok(DiagnosticReport::Fresh {
                    server: self.server.clone(),
                    root: self.root.clone(),
                    sources: vec![self.server.clone()],
                    warnings: Vec::new(),
                    path: path.to_path_buf(),
                    version,
                    diagnostics,
                    overflow,
                    total,
                    truncated: total > self.max_results,
                    introduced: None,
                    resolved: None,
                })
            }
            Err(_) => {
                if let Some(mut diagnostics) = self.pull_diagnostics(path).await {
                    sort_dedupe_diagnostics(&mut diagnostics);
                    let total = diagnostics.len();
                    let overflow = if diagnostics.len() > self.max_results {
                        diagnostics.split_off(self.max_results)
                    } else {
                        Vec::new()
                    };
                    return Ok(DiagnosticReport::Fresh {
                        server: self.server.clone(),
                        root: self.root.clone(),
                        sources: vec![self.server.clone()],
                        warnings: Vec::new(),
                        path: path.to_path_buf(),
                        version,
                        diagnostics,
                        overflow,
                        total,
                        truncated: total > self.max_results,
                        introduced: None,
                        resolved: None,
                    });
                }
                Ok(DiagnosticReport::NoFreshData {
                    server: self.server.clone(),
                    root: self.root.clone(),
                    path: path.to_path_buf(),
                    version,
                    waited_ms: started.elapsed().as_millis() as u64,
                })
            }
        }
    }

    async fn pull_diagnostics(&self, path: &Path) -> Option<Vec<Diagnostic>> {
        let result = self
            .request(
                "textDocument/diagnostic",
                json!({
                    "textDocument": { "uri": file_uri(path).ok()? }
                }),
                self.request_timeout.min(self.diagnostics_timeout),
            )
            .await
            .ok()?;
        Some(parse_diagnostics(result.get("items")?))
    }

    async fn diagnostic_baseline(
        &self,
        path: &Path,
        text: String,
    ) -> Result<Option<DiagnosticBaseline>, Error> {
        let cached_version = {
            let documents = self.documents.lock().await;
            documents
                .get(path)
                .filter(|document| document.text == text)
                .map(|document| document.version)
        };
        if let Some(version) = cached_version {
            let cached = self
                .diagnostics
                .lock()
                .expect("diagnostics lock poisoned")
                .get(&protocol_path(path))
                .filter(|snapshot| {
                    snapshot
                        .version
                        .map(|published| published >= version)
                        .unwrap_or(true)
                })
                .map(|snapshot| snapshot.diagnostics.clone());
            if let Some(mut diagnostics) = cached {
                sort_dedupe_diagnostics(&mut diagnostics);
                diagnostics.truncate(self.max_results);
                return Ok(Some(DiagnosticBaseline {
                    entries: vec![DiagnosticBaselineEntry {
                        server: self.server.clone(),
                        text,
                        diagnostics,
                    }],
                }));
            }
        }
        // Cache-only: never fire a blocking query just to snapshot a baseline.
        // Without one the post-edit report is absolute (no introduced/resolved
        // split) — the delta returns on the next edit once diagnostics are cached.
        Ok(None)
    }

    async fn sync_document(&self, path: &Path, text: String) -> Result<DocumentSync, Error> {
        let uri = file_uri(path)?;
        let touched = self.document_clock.fetch_add(1, Ordering::Relaxed);
        let mut documents = self.documents.lock().await;
        match documents.get_mut(path) {
            Some(document) if document.text == text => {
                document.touched = touched;
                Ok(DocumentSync {
                    version: document.version,
                    opened: false,
                })
            }
            Some(document) => {
                let old_text = document.text.clone();
                document.version += 1;
                document.text.clone_from(&text);
                document.touched = touched;
                let version = document.version;
                let sync_kind = self.sync_kind.load(Ordering::Acquire);
                if sync_kind != 0 {
                    let change = if sync_kind == 2 {
                        incremental_content_change(&old_text, &text)
                    } else {
                        json!({ "text": text })
                    };
                    self.notify(
                        "textDocument/didChange",
                        json!({
                            "textDocument": { "uri": uri, "version": version },
                            "contentChanges": [change]
                        }),
                    )
                    .await?;
                }
                Ok(DocumentSync {
                    version,
                    opened: false,
                })
            }
            None => {
                let version = 1;
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": self.language_id(path)?,
                            "version": version,
                            "text": text
                        }
                    }),
                )
                .await?;
                documents.insert(
                    path.to_path_buf(),
                    Document {
                        version,
                        text,
                        touched,
                    },
                );
                self.evict_documents(&mut documents, path).await;
                Ok(DocumentSync {
                    version,
                    opened: true,
                })
            }
        }
    }

    /// Close least-recently-used documents past the cap, freeing the server's
    /// per-document state. `keep` (the just-opened file) is never evicted.
    async fn evict_documents(&self, documents: &mut HashMap<PathBuf, Document>, keep: &Path) {
        while documents.len() > self.max_open_documents {
            let Some(victim) = documents
                .iter()
                .filter(|(path, _)| path.as_path() != keep)
                .min_by_key(|(_, document)| document.touched)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            documents.remove(&victim);
            self.diagnostics
                .lock()
                .expect("diagnostics lock poisoned")
                .remove(&protocol_path(&victim));
            if let Ok(uri) = file_uri(&victim) {
                let _ = self
                    .notify(
                        "textDocument/didClose",
                        json!({ "textDocument": { "uri": uri } }),
                    )
                    .await;
            }
        }
    }

    fn language_id(&self, path: &Path) -> Result<&str, Error> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        self.language_ids
            .get(extension)
            .map(String::as_str)
            .ok_or_else(|| Error::UnsupportedFile(path.to_path_buf()))
    }

    async fn prepare_document(&self, path: &Path) -> Result<(), Error> {
        let text = tokio::fs::read_to_string(path).await?;
        self.sync_document(path, text).await?;
        Ok(())
    }

    async fn definition(
        &self,
        path: &Path,
        position: Position,
    ) -> Result<QueryReport<Location>, Error> {
        self.prepare_document(path).await?;
        let result = self
            .request(
                "textDocument/definition",
                text_document_position(path, position)?,
                self.request_timeout,
            )
            .await?;
        let mut items = parse_locations(result);
        sort_dedupe_locations(&mut items);
        Ok(self.location_report(items))
    }

    async fn references(
        &self,
        path: &Path,
        position: Position,
        include_declaration: bool,
    ) -> Result<QueryReport<Location>, Error> {
        self.prepare_document(path).await?;
        let mut params = text_document_position(path, position)?;
        params["context"] = json!({ "includeDeclaration": include_declaration });
        let result = self
            .request("textDocument/references", params, self.request_timeout)
            .await?;
        let mut items = parse_locations(result);
        sort_dedupe_locations(&mut items);
        Ok(self.location_report(items))
    }

    async fn hover(&self, path: &Path, position: Position) -> Result<QueryReport<Hover>, Error> {
        self.prepare_document(path).await?;
        let result = self
            .request(
                "textDocument/hover",
                text_document_position(path, position)?,
                self.request_timeout,
            )
            .await?;
        let full = parse_hover(result, usize::MAX);
        let overflow = full
            .as_ref()
            .filter(|hover| hover.contents.chars().count() > self.max_text_chars)
            .cloned()
            .into_iter()
            .collect::<Vec<_>>();
        let truncated = !overflow.is_empty();
        let items = full
            .map(|mut hover| {
                truncate_chars(&mut hover.contents, self.max_text_chars);
                hover
            })
            .into_iter()
            .collect::<Vec<_>>();
        let total = items.len();
        Ok(QueryReport::Ready {
            server: self.server.clone(),
            root: self.root.clone(),
            sources: vec![self.server.clone()],
            warnings: Vec::new(),
            items,
            overflow,
            total,
            truncated,
        })
    }

    async fn workspace_symbols(&self, query: &str) -> Result<QueryReport<Symbol>, Error> {
        let result = self
            .request(
                "workspace/symbol",
                json!({ "query": query }),
                self.request_timeout,
            )
            .await?;
        let mut items = parse_symbols(result);
        items.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.location.path.cmp(&right.location.path))
                .then_with(|| {
                    left.location
                        .range
                        .start
                        .line
                        .cmp(&right.location.range.start.line)
                })
        });
        items.dedup();
        let total = items.len();
        let overflow = if items.len() > self.max_results {
            items.split_off(self.max_results)
        } else {
            Vec::new()
        };
        Ok(QueryReport::Ready {
            server: self.server.clone(),
            root: self.root.clone(),
            sources: vec![self.server.clone()],
            warnings: Vec::new(),
            truncated: total > items.len(),
            total,
            items,
            overflow,
        })
    }

    async fn implementation(
        &self,
        path: &Path,
        position: Position,
    ) -> Result<QueryReport<Location>, Error> {
        self.prepare_document(path).await?;
        let result = self
            .request(
                "textDocument/implementation",
                text_document_position(path, position)?,
                self.request_timeout,
            )
            .await?;
        let mut items = parse_locations(result);
        sort_dedupe_locations(&mut items);
        Ok(self.location_report(items))
    }

    async fn document_symbols(&self, path: &Path) -> Result<QueryReport<Symbol>, Error> {
        self.prepare_document(path).await?;
        let result = self
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": file_uri(path)? } }),
                self.request_timeout,
            )
            .await?;
        Ok(self.symbol_report(parse_document_symbols(&result, path)))
    }

    /// Two-step call hierarchy: prepare at `position`, then resolve callers
    /// (`outgoing = false`) or callees (`outgoing = true`) of the first item.
    async fn call_hierarchy(
        &self,
        path: &Path,
        position: Position,
        outgoing: bool,
    ) -> Result<QueryReport<Symbol>, Error> {
        self.prepare_document(path).await?;
        let prepared = self
            .request(
                "textDocument/prepareCallHierarchy",
                text_document_position(path, position)?,
                self.request_timeout,
            )
            .await?;
        let Some(item) = prepared.as_array().and_then(|items| items.first()).cloned() else {
            return Ok(self.symbol_report(Vec::new()));
        };
        let method = if outgoing {
            "callHierarchy/outgoingCalls"
        } else {
            "callHierarchy/incomingCalls"
        };
        let result = self
            .request(method, json!({ "item": item }), self.request_timeout)
            .await?;
        Ok(self.symbol_report(parse_call_hierarchy(&result, outgoing)))
    }

    fn location_report(&self, mut items: Vec<Location>) -> QueryReport<Location> {
        let total = items.len();
        let overflow = if items.len() > self.max_results {
            items.split_off(self.max_results)
        } else {
            Vec::new()
        };
        QueryReport::Ready {
            server: self.server.clone(),
            root: self.root.clone(),
            sources: vec![self.server.clone()],
            warnings: Vec::new(),
            truncated: total > items.len(),
            total,
            items,
            overflow,
        }
    }

    fn symbol_report(&self, mut items: Vec<Symbol>) -> QueryReport<Symbol> {
        items.dedup();
        let total = items.len();
        let overflow = if items.len() > self.max_results {
            items.split_off(self.max_results)
        } else {
            Vec::new()
        };
        QueryReport::Ready {
            server: self.server.clone(),
            root: self.root.clone(),
            sources: vec![self.server.clone()],
            warnings: Vec::new(),
            truncated: total > items.len(),
            total,
            items,
            overflow,
        }
    }

    fn snapshot_after(
        &self,
        path: &Path,
        sequence: u64,
        version: i64,
    ) -> Option<DiagnosticSnapshot> {
        let diagnostics = self.diagnostics.lock().expect("diagnostics lock poisoned");
        diagnostics.get(&protocol_path(path)).and_then(|snapshot| {
            let fresh_sequence = snapshot.sequence > sequence;
            let fresh_version = snapshot
                .version
                .map(|published| published >= version)
                .unwrap_or(true);
            (fresh_sequence && fresh_version).then(|| snapshot.clone())
        })
    }

    async fn request(
        &self,
        method: &'static str,
        params: Value,
        duration: Duration,
    ) -> Result<Value, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending request lock poisoned")
            .insert(id, sender);
        if let Err(error) = self
            .send(json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params
            }))
            .await
        {
            self.pending
                .lock()
                .expect("pending request lock poisoned")
                .remove(&id);
            return Err(error);
        }
        let response = match timeout(duration, receiver).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return Err(Error::Disconnected),
            Err(_) => {
                self.pending
                    .lock()
                    .expect("pending request lock poisoned")
                    .remove(&id);
                let _ = self.notify("$/cancelRequest", json!({ "id": id })).await;
                return Err(Error::Timeout(method));
            }
        };
        if let Some(error) = response.get("error") {
            return Err(Error::Protocol(error.to_string()));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &'static str, params: Value) -> Result<(), Error> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn send(&self, message: Value) -> Result<(), Error> {
        let payload =
            serde_json::to_vec(&message).map_err(|error| Error::Protocol(error.to_string()))?;
        let mut writer = self.writer.lock().await;
        write_frame(&mut *writer, &payload).await?;
        Ok(())
    }

    async fn shutdown(&self) {
        let _ = self
            .request("shutdown", Value::Null, Duration::from_secs(1))
            .await;
        let _ = self.notify("exit", Value::Null).await;
        let mut child_guard = self.child.lock().await;
        if let Some(child) = child_guard.as_mut()
            && timeout(Duration::from_millis(500), child.wait())
                .await
                .is_err()
        {
            kill_process_group(self.process_group.swap(0, Ordering::AcqRel));
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.process_group.store(0, Ordering::Release);
        self.cancel.cancel();
        self.reader_task.abort();
        if let Some(task) = &self.stderr_task {
            task.abort();
        }
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.cancel.cancel();
        kill_process_group(self.process_group.swap(0, Ordering::AcqRel));
        self.reader_task.abort();
        if let Some(task) = &self.stderr_task {
            task.abort();
        }
        // `kill_on_drop(true)` terminates a still-running child.
    }
}

async fn reader_loop(reader: BoxReader, state: ReaderState) {
    let ReaderState {
        writer,
        root,
        pending,
        diagnostics,
        sequence,
        diagnostic_notify,
        alive,
        process_group,
        sync_kind,
        settings,
        cancel,
    } = state;
    let mut reader = BufReader::new(reader);
    loop {
        let message = tokio::select! {
            _ = cancel.cancelled() => break,
            message = read_frame(&mut reader) => match message {
                Ok(Some(message)) => message,
                Ok(None) | Err(_) => break,
            }
        };
        if message.get("method").is_none()
            && let Some(id) = message.get("id").and_then(Value::as_i64)
            && let Some(sender) = pending
                .lock()
                .expect("pending request lock poisoned")
                .remove(&id)
        {
            let _ = sender.send(message.clone());
        }
        if let (Some(id), Some(method)) = (
            message.get("id").cloned(),
            message.get("method").and_then(Value::as_str),
        ) {
            let result = match method {
                "workspace/configuration" => {
                    let response = message
                        .pointer("/params/items")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| {
                                    resolve_configuration_section(
                                        settings.as_ref(),
                                        item.get("section").and_then(Value::as_str),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Ok(Value::Array(response))
                }
                "workspace/workspaceFolders" => Ok(json!([{
                    "uri": file_uri(&root).unwrap_or_default(),
                    "name": root.file_name().and_then(|value| value.to_str()).unwrap_or("workspace")
                }])),
                "client/registerCapability" => {
                    if let Some(kind) = registered_sync_kind(&message) {
                        sync_kind.store(kind, Ordering::Release);
                    }
                    Ok(Value::Null)
                }
                "client/unregisterCapability"
                | "window/workDoneProgress/create"
                | "workspace/diagnostic/refresh"
                | "workspace/inlayHint/refresh"
                | "workspace/semanticTokens/refresh"
                | "workspace/codeLens/refresh" => Ok(Value::Null),
                _ => Err(json!({ "code": -32601, "message": "method not supported by Medha" })),
            };
            let response = match result {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
            };
            if let Ok(payload) = serde_json::to_vec(&response) {
                let mut writer = writer.lock().await;
                if write_frame(&mut *writer, &payload).await.is_err() {
                    break;
                }
            }
        }
        if message.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            && let Some(params) = message.get("params")
            && let (Some(uri), Some(items)) = (
                params.get("uri").and_then(Value::as_str),
                params.get("diagnostics"),
            )
            && let Ok(uri) = Url::parse(uri)
            && let Ok(path) = uri.to_file_path()
        {
            let parsed = parse_diagnostics(items);
            let next = sequence.fetch_add(1, Ordering::AcqRel) + 1;
            let version = params.get("version").and_then(Value::as_i64);
            diagnostics
                .lock()
                .expect("diagnostics lock poisoned")
                .insert(
                    normalize_path(&path),
                    DiagnosticSnapshot {
                        sequence: next,
                        version,
                        diagnostics: parsed,
                    },
                );
            diagnostic_notify.notify_waiters();
        }
    }
    pending
        .lock()
        .expect("pending request lock poisoned")
        .clear();
    alive.store(false, Ordering::Release);
    kill_process_group(process_group.swap(0, Ordering::AcqRel));
}

async fn read_frame<R>(reader: &mut R) -> Result<Option<Value>, Error>
where
    R: AsyncBufRead + Unpin,
{
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| Error::Protocol(error.to_string()))?,
            );
        }
    }
    let length =
        content_length.ok_or_else(|| Error::Protocol("missing Content-Length".to_string()))?;
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| Error::Protocol(error.to_string()))
}

async fn write_frame<W>(writer: &mut W, payload: &[u8]) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes())
        .await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

fn apply_diagnostic_delta(
    mut report: DiagnosticReport,
    baseline: Option<&DiagnosticBaseline>,
    new_text: &str,
) -> DiagnosticReport {
    let DiagnosticReport::Fresh {
        server,
        diagnostics,
        overflow,
        introduced,
        resolved,
        ..
    } = &mut report
    else {
        return report;
    };
    let Some(baseline) = baseline else {
        return report;
    };
    let Some(baseline) = baseline
        .entries
        .iter()
        .find(|entry| entry.server == *server)
    else {
        return report;
    };
    let shifted = baseline
        .diagnostics
        .iter()
        .filter_map(|diagnostic| shift_diagnostic(diagnostic, &baseline.text, new_text))
        .collect::<Vec<_>>();
    let current = diagnostics
        .iter()
        .chain(overflow.iter())
        .collect::<Vec<_>>();
    *introduced = Some(
        current
            .iter()
            .filter(|diagnostic| !shifted.contains(diagnostic))
            .map(|diagnostic| (*diagnostic).clone())
            .collect(),
    );
    *resolved = Some(
        shifted
            .into_iter()
            .filter(|diagnostic| !current.contains(&diagnostic))
            .collect(),
    );
    report
}

fn shift_diagnostic(diagnostic: &Diagnostic, old_text: &str, new_text: &str) -> Option<Diagnostic> {
    let diff = TextDiff::from_lines(old_text, new_text);
    let map_line = |line: u32| -> Option<u32> {
        let line = line as usize;
        for operation in diff.ops() {
            let old = operation.old_range();
            if old.contains(&line) {
                return (operation.tag() == DiffTag::Equal)
                    .then(|| (operation.new_range().start + line - old.start) as u32);
            }
        }
        let old_lines = old_text.split_inclusive('\n').count();
        let new_lines = new_text.split_inclusive('\n').count();
        (line == old_lines).then_some(new_lines as u32)
    };
    let mut shifted = diagnostic.clone();
    shifted.range.start.line = map_line(diagnostic.range.start.line)?;
    shifted.range.end.line = map_line(diagnostic.range.end.line)?;
    Some(shifted)
}

fn sort_dedupe_diagnostics(items: &mut Vec<Diagnostic>) {
    items.sort_by(|left, right| {
        left.range
            .start
            .line
            .cmp(&right.range.start.line)
            .then_with(|| left.range.start.character.cmp(&right.range.start.character))
            .then_with(|| left.range.end.line.cmp(&right.range.end.line))
            .then_with(|| left.range.end.character.cmp(&right.range.end.character))
            .then_with(|| left.message.cmp(&right.message))
            .then_with(|| left.source.cmp(&right.source))
            .then_with(|| {
                left.code
                    .as_ref()
                    .map(Value::to_string)
                    .cmp(&right.code.as_ref().map(Value::to_string))
            })
    });
    items.dedup();
}

/// Resolve one `workspace/configuration` item against the server's settings.
/// A missing `section` returns the whole settings object; a dotted section
/// (`rust-analyzer.check.command`) walks nested objects; anything unresolved is
/// `null`, which servers treat as "use your default".
fn resolve_configuration_section(settings: &Value, section: Option<&str>) -> Value {
    match section {
        None | Some("") => settings.clone(),
        Some(section) => {
            let mut current = settings;
            for key in section.split('.') {
                match current.get(key) {
                    Some(next) => current = next,
                    None => return Value::Null,
                }
            }
            current.clone()
        }
    }
}

fn parse_diagnostics(value: &Value) -> Vec<Diagnostic> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_diagnostic)
        .collect()
}

fn parse_diagnostic(value: &Value) -> Option<Diagnostic> {
    let range = serde_json::from_value(value.get("range")?.clone()).ok()?;
    let message = value.get("message")?.as_str()?.to_string();
    let code_description = value
        .pointer("/codeDescription/href")
        .and_then(Value::as_str)
        .map(|href| CodeDescription {
            href: href.to_string(),
        });
    let tags = value
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_u64)
                .map(|tag| tag as u8)
                .collect()
        })
        .unwrap_or_default();
    let related_information = value
        .get("relatedInformation")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(RelatedInformation {
                        location: parse_location(item.get("location")?)?,
                        message: item.get("message")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Diagnostic {
        range,
        severity: value
            .get("severity")
            .and_then(Value::as_u64)
            .map(|s| s as u8),
        code: value.get("code").cloned(),
        code_description,
        source: value
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_owned),
        message,
        tags,
        related_information,
        data: value.get("data").cloned(),
    })
}

fn text_document_position(path: &Path, position: Position) -> Result<Value, Error> {
    Ok(json!({
        "textDocument": { "uri": file_uri(path)? },
        "position": position
    }))
}

fn incremental_content_change(old_text: &str, new_text: &str) -> Value {
    let old_chars = old_text.char_indices().collect::<Vec<_>>();
    let new_chars = new_text.char_indices().collect::<Vec<_>>();
    let prefix = old_chars
        .iter()
        .zip(&new_chars)
        .take_while(|((_, left), (_, right))| left == right)
        .count();
    let max_suffix = old_chars.len().min(new_chars.len()).saturating_sub(prefix);
    let suffix = old_chars
        .iter()
        .rev()
        .zip(new_chars.iter().rev())
        .take(max_suffix)
        .take_while(|((_, left), (_, right))| left == right)
        .count();
    let old_start = old_chars
        .get(prefix)
        .map_or(old_text.len(), |(index, _)| *index);
    let new_start = new_chars
        .get(prefix)
        .map_or(new_text.len(), |(index, _)| *index);
    let old_end_index = old_chars.len().saturating_sub(suffix);
    let new_end_index = new_chars.len().saturating_sub(suffix);
    let old_end = old_chars
        .get(old_end_index)
        .map_or(old_text.len(), |(index, _)| *index);
    let new_end = new_chars
        .get(new_end_index)
        .map_or(new_text.len(), |(index, _)| *index);
    json!({
        "range": {
            "start": position_at_byte(old_text, old_start),
            "end": position_at_byte(old_text, old_end)
        },
        "text": &new_text[new_start..new_end]
    })
}

fn position_at_byte(text: &str, byte: usize) -> Position {
    let prefix = &text[..byte.min(text.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_text = prefix.rsplit_once('\n').map_or(prefix, |(_, line)| line);
    Position {
        line,
        character: line_text.encode_utf16().count() as u32,
    }
}

fn registered_sync_kind(message: &Value) -> Option<u64> {
    message
        .pointer("/params/registrations")
        .and_then(Value::as_array)?
        .iter()
        .find(|registration| {
            registration.get("method").and_then(Value::as_str) == Some("textDocument/didChange")
        })
        .and_then(|registration| registration.pointer("/registerOptions/syncKind"))
        .and_then(Value::as_u64)
        .map(|kind| kind.min(2))
}

fn parse_locations(value: Value) -> Vec<Location> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().filter_map(parse_location).collect(),
        item => parse_location(&item).into_iter().collect(),
    }
}

fn parse_location(value: &Value) -> Option<Location> {
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))
        .and_then(Value::as_str)?;
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))
        .or_else(|| value.get("targetRange"))?;
    let path = Url::parse(uri).ok()?.to_file_path().ok()?;
    Some(Location {
        path: protocol_path(&path),
        range: serde_json::from_value(range.clone()).ok()?,
    })
}

fn sort_dedupe_locations(items: &mut Vec<Location>) {
    items.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.range.start.line.cmp(&right.range.start.line))
            .then_with(|| left.range.start.character.cmp(&right.range.start.character))
            .then_with(|| left.range.end.line.cmp(&right.range.end.line))
            .then_with(|| left.range.end.character.cmp(&right.range.end.character))
    });
    items.dedup();
}

fn parse_hover(value: Value, max_chars: usize) -> Option<Hover> {
    if value.is_null() {
        return None;
    }
    let contents = value.get("contents")?;
    let mut text = match contents {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
        item => marked_string_text(item).unwrap_or_default(),
    };
    truncate_chars(&mut text, max_chars);
    let range = value
        .get("range")
        .and_then(|range| serde_json::from_value(range.clone()).ok());
    Some(Hover {
        contents: text,
        range,
    })
}

fn marked_string_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => object
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

fn truncate_chars(text: &mut String, max_chars: usize) {
    if text.chars().count() <= max_chars {
        return;
    }
    let cut = text
        .char_indices()
        .nth(max_chars)
        .map_or(text.len(), |(index, _)| index);
    text.truncate(cut);
    text.push_str("\n… [truncated]");
}

fn parse_symbols(value: Value) -> Vec<Symbol> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let location = parse_location(item.get("location")?)?;
            Some(Symbol {
                name: item.get("name")?.as_str()?.to_string(),
                kind: item.get("kind")?.as_u64()?.try_into().ok()?,
                container_name: item
                    .get("containerName")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                location,
            })
        })
        .collect()
}

/// Accepts both LSP result shapes: hierarchical `DocumentSymbol` (flattened,
/// child `containerName` set to its parent) and flat `SymbolInformation`.
fn parse_document_symbols(value: &Value, path: &Path) -> Vec<Symbol> {
    let mut out = Vec::new();
    for item in value.as_array().into_iter().flatten() {
        collect_document_symbol(item, path, None, &mut out);
    }
    out
}

fn collect_document_symbol(
    item: &Value,
    path: &Path,
    container: Option<&str>,
    out: &mut Vec<Symbol>,
) {
    if let Some(location) = item.get("location") {
        if let Some(symbol) = symbol_from_information(item, location) {
            out.push(symbol);
        }
        return;
    }
    let (Some(name), Some(kind)) = (
        item.get("name").and_then(Value::as_str),
        item.get("kind").and_then(Value::as_u64),
    ) else {
        return;
    };
    if let Some(range) = item
        .get("selectionRange")
        .or_else(|| item.get("range"))
        .and_then(|range| serde_json::from_value::<Range>(range.clone()).ok())
    {
        out.push(Symbol {
            name: name.to_string(),
            kind: kind as u8,
            container_name: container.map(str::to_owned),
            location: Location {
                path: protocol_path(path),
                range,
            },
        });
    }
    for child in item
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        collect_document_symbol(child, path, Some(name), out);
    }
}

fn symbol_from_information(item: &Value, location: &Value) -> Option<Symbol> {
    Some(Symbol {
        name: item.get("name")?.as_str()?.to_string(),
        kind: item.get("kind")?.as_u64()?.try_into().ok()?,
        container_name: item
            .get("containerName")
            .and_then(Value::as_str)
            .map(str::to_owned),
        location: parse_location(location)?,
    })
}

/// Map incoming (`from`) or outgoing (`to`) call-hierarchy items to symbols.
fn parse_call_hierarchy(value: &Value, outgoing: bool) -> Vec<Symbol> {
    let key = if outgoing { "to" } else { "from" };
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|call| {
            let item = call.get(key)?;
            let path = Url::parse(item.get("uri")?.as_str()?)
                .ok()?
                .to_file_path()
                .ok()?;
            let range = item.get("selectionRange").or_else(|| item.get("range"))?;
            Some(Symbol {
                name: item.get("name")?.as_str()?.to_string(),
                kind: item.get("kind")?.as_u64()?.try_into().ok()?,
                container_name: item
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                location: Location {
                    path: protocol_path(&path),
                    range: serde_json::from_value(range.clone()).ok()?,
                },
            })
        })
        .collect()
}

fn adapter_language_id<'a>(adapter: &'a ServerAdapter, path: &Path) -> Option<&'a str> {
    let extension = path.extension()?.to_str()?;
    adapter
        .languages
        .iter()
        .find(|language| language.extension == extension)
        .map(|language| language.language_id.as_str())
}

/// Where Medha keeps language servers it installed for the user. Nothing is
/// written to `/usr/local`, the global npm root, or any other shared location —
/// an agent installing a package should not alter the machine outside its own
/// directory, and removing this one directory undoes all of it.
pub fn server_install_dir() -> Option<PathBuf> {
    let base = std::env::var_os("MEDHA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".medha")))?;
    Some(base.join("lsp"))
}

/// The directories holding Medha-installed server binaries, in PATH order.
pub fn server_bin_dirs() -> Vec<PathBuf> {
    server_install_dir()
        .map(|dir| vec![dir.join("bin"), dir.join("node_modules/.bin")])
        .unwrap_or_default()
}

/// True when `program` is runnable, counting Medha's own install directory as
/// well as the inherited PATH.
pub fn server_on_path(program: &str) -> bool {
    if sandbox::program_on_path(program) {
        return true;
    }
    server_bin_dirs()
        .into_iter()
        .any(|dir| dir.join(program).exists())
}

/// Language servers need local toolchain discovery, but they should not inherit
/// API keys, cloud credentials, or arbitrary session secrets from Medha.
fn language_server_environment() -> Vec<(String, String)> {
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
        "CARGO_HOME",
        "RUSTUP_HOME",
        "GOPATH",
        "GOROOT",
        "VIRTUAL_ENV",
        "NODE_PATH",
        "SystemRoot",
        "PATHEXT",
    ];
    let mut environment: Vec<(String, String)> = ALLOWED
        .iter()
        .filter_map(|name| {
            std::env::var_os(name)
                .map(|value| ((*name).to_string(), value.to_string_lossy().into_owned()))
        })
        .collect();
    // Prepend Medha's own install directory so a server installed through
    // `medha lsp install` is found without the user editing their shell PATH.
    let extra = server_bin_dirs();
    if !extra.is_empty() {
        let inherited = environment
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        let joined = extra
            .iter()
            .map(|dir| dir.to_string_lossy().into_owned())
            .chain(std::iter::once(inherited))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(":");
        environment.retain(|(key, _)| key != "PATH");
        environment.push(("PATH".into(), joined));
    }
    environment
}

#[allow(unused_variables)]
fn kill_process_group(group: u64) {
    if group == 0 {
        return;
    }
    // Each server leads its own group/tree; tear down the helpers it spawned too.
    #[cfg(unix)]
    unsafe {
        libc::kill(-(group as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &group.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

fn server_install_hint(server: &str) -> &'static str {
    match server {
        "rust-analyzer" => "install rust-analyzer with rustup or your system package manager",
        "typescript-language-server" => {
            "install typescript-language-server and typescript (for example with npm)"
        }
        "pyright" => "install pyright-langserver (for example with npm or pip)",
        "gopls" => "install gopls with the Go toolchain",
        "clangd" => "install clangd with LLVM or your system package manager",
        _ => "install the configured language-server executable",
    }
}

fn detect_root(path: &Path, workspace: &Path, markers: &[String]) -> PathBuf {
    let start = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(workspace)
    };
    for directory in start.ancestors() {
        if !directory.starts_with(workspace) {
            break;
        }
        if markers.iter().any(|marker| directory.join(marker).exists()) {
            return normalize_path(directory);
        }
        if directory == workspace {
            break;
        }
    }
    workspace.to_path_buf()
}

fn normalize_path_from(workspace: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(&workspace.join(path))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn file_uri(path: &Path) -> Result<String, Error> {
    Url::from_file_path(path)
        .map(String::from)
        .map_err(|_| Error::InvalidFileUri(path.to_path_buf()))
}

fn protocol_path(path: &Path) -> PathBuf {
    Url::from_file_path(path)
        .ok()
        .and_then(|uri| uri.to_file_path().ok())
        .map(|path| normalize_path(&path))
        .unwrap_or_else(|| normalize_path(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::io::{AsyncWrite, duplex, split};
    use tokio::process::Command;

    fn fake_languages() -> HashMap<String, String> {
        HashMap::from([("rs".to_string(), "rust".to_string())])
    }

    fn missing_adapter() -> ServerAdapter {
        let mut adapter = ServerAdapter::rust_analyzer();
        adapter.command = vec!["medha-definitely-missing-language-server".to_string()];
        adapter
    }

    fn test_settings(
        diagnostics_timeout: Duration,
        max_results: usize,
        max_text_chars: usize,
    ) -> ClientSettings {
        ClientSettings {
            startup_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
            diagnostics_timeout,
            diagnostic_settle: diagnostics_timeout.min(Duration::from_millis(20)),
            max_results,
            max_text_chars,
            max_open_documents: 64,
        }
    }

    async fn publish_empty<W>(writer: &mut W, document: &Value)
    where
        W: AsyncWrite + Unpin,
    {
        let payload = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": document["uri"],
                "version": document["version"],
                "diagnostics": []
            }
        }))
        .unwrap();
        write_frame(writer, &payload).await.unwrap();
    }

    #[tokio::test]
    async fn fresh_empty_diagnostics_are_distinct_from_no_fresh_data() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("main.rs");
        tokio::fs::write(&path, "fn main() {}").await.unwrap();
        let (client_stream, server_stream) = duplex(16 * 1024);
        let (client_read, client_write) = split(client_stream);
        let (server_read, mut server_write) = split(server_stream);

        let server = tokio::spawn(async move {
            let mut server_read = BufReader::new(server_read);
            let mut configuration_answered = false;
            let mut pending_document = None;
            while let Some(message) = read_frame(&mut server_read).await.unwrap() {
                match message.get("method").and_then(Value::as_str) {
                    Some("initialize") => {
                        let id = message["id"].as_i64().unwrap();
                        let payload = serde_json::to_vec(&json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "capabilities": {} }
                        }))
                        .unwrap();
                        write_frame(&mut server_write, &payload).await.unwrap();
                        let request = serde_json::to_vec(&json!({
                            "jsonrpc": "2.0",
                            "id": 99,
                            "method": "workspace/configuration",
                            "params": { "items": [{ "section": "rust-analyzer" }] }
                        }))
                        .unwrap();
                        write_frame(&mut server_write, &request).await.unwrap();
                    }
                    Some("textDocument/didOpen") => {
                        let params = &message["params"]["textDocument"];
                        if configuration_answered {
                            publish_empty(&mut server_write, params).await;
                        } else {
                            pending_document = Some(params.clone());
                        }
                    }
                    Some("shutdown") => {
                        let id = message["id"].as_i64().unwrap();
                        let payload = serde_json::to_vec(&json!({
                            "jsonrpc": "2.0", "id": id, "result": null
                        }))
                        .unwrap();
                        write_frame(&mut server_write, &payload).await.unwrap();
                    }
                    Some("exit") => break,
                    _ if message.get("id") == Some(&json!(99)) => {
                        assert_eq!(message["result"], json!([null]));
                        configuration_answered = true;
                        if let Some(document) = pending_document.take() {
                            publish_empty(&mut server_write, &document).await;
                        }
                    }
                    _ => {}
                }
            }
        });

        let client = LspClient::from_io(
            "fake-rust-analyzer".to_string(),
            directory.path().to_path_buf(),
            fake_languages(),
            ClientTransport {
                reader: Box::new(client_read),
                writer: Box::new(client_write),
                child: None,
            },
            // Generous timeout so the publish roundtrip never races the deadline
            // under parallel-test CPU load; NoFreshData is covered separately.
            test_settings(Duration::from_secs(10), 200, 16_000),
            Value::Null,
        );
        client.initialize().await.unwrap();
        let report = client.fresh_diagnostics(&path).await.unwrap();
        match report {
            DiagnosticReport::Fresh { diagnostics, .. } => assert!(diagnostics.is_empty()),
            other => panic!("expected a fresh, clean diagnostic report, got {other:?}"),
        }
        assert!(
            client.is_published(),
            "a server that has published is warm; post-edit diagnostics may wait"
        );
        client.shutdown().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stale_diagnostics_are_not_reported_as_current() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("main.rs");
        tokio::fs::write(&path, "fn main() {}").await.unwrap();
        let (client_stream, server_stream) = duplex(16 * 1024);
        let (client_read, client_write) = split(client_stream);
        let (server_read, mut server_write) = split(server_stream);
        let server = tokio::spawn(async move {
            let mut server_read = BufReader::new(server_read);
            while let Some(message) = read_frame(&mut server_read).await.unwrap() {
                if message.get("method").and_then(Value::as_str) == Some("initialize") {
                    let payload = serde_json::to_vec(&json!({
                        "jsonrpc": "2.0", "id": message["id"],
                        "result": { "capabilities": {} }
                    }))
                    .unwrap();
                    write_frame(&mut server_write, &payload).await.unwrap();
                }
            }
        });
        let client = LspClient::from_io(
            "fake-rust-analyzer".to_string(),
            directory.path().to_path_buf(),
            fake_languages(),
            ClientTransport {
                reader: Box::new(client_read),
                writer: Box::new(client_write),
                child: None,
            },
            test_settings(Duration::from_millis(20), 200, 16_000),
            Value::Null,
        );
        client.initialize().await.unwrap();
        let report = client.fresh_diagnostics(&path).await.unwrap();
        assert!(matches!(report, DiagnosticReport::NoFreshData { .. }));
        client.cancel.cancel();
        server.abort();
    }

    #[tokio::test]
    async fn hung_navigation_request_times_out_without_killing_client() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("main.rs");
        tokio::fs::write(&path, "fn main() {}").await.unwrap();
        let (client_stream, server_stream) = duplex(16 * 1024);
        let (client_read, client_write) = split(client_stream);
        let (server_read, mut server_write) = split(server_stream);
        let server = tokio::spawn(async move {
            let mut server_read = BufReader::new(server_read);
            while let Some(message) = read_frame(&mut server_read).await.unwrap() {
                match message.get("method").and_then(Value::as_str) {
                    Some("initialize") | Some("shutdown") => {
                        let payload = serde_json::to_vec(&json!({
                            "jsonrpc": "2.0",
                            "id": message["id"],
                            "result": if message["method"] == "initialize" {
                                json!({ "capabilities": {} })
                            } else {
                                Value::Null
                            }
                        }))
                        .unwrap();
                        write_frame(&mut server_write, &payload).await.unwrap();
                    }
                    Some("exit") => break,
                    // Deliberately never answer definition.
                    _ => {}
                }
            }
        });
        let client = LspClient::from_io(
            "hung".into(),
            directory.path().to_path_buf(),
            fake_languages(),
            ClientTransport {
                reader: Box::new(client_read),
                writer: Box::new(client_write),
                child: None,
            },
            ClientSettings {
                startup_timeout: Duration::from_secs(1),
                request_timeout: Duration::from_millis(20),
                diagnostics_timeout: Duration::from_millis(20),
                diagnostic_settle: Duration::from_millis(10),
                max_results: 20,
                max_text_chars: 1000,
                max_open_documents: 64,
            },
            Value::Null,
        );
        client.initialize().await.unwrap();
        let error = client
            .definition(
                &path,
                Position {
                    line: 0,
                    character: 3,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Timeout("textDocument/definition")));
        assert!(
            !client.is_published(),
            "a server that never published is cold; post-edit diagnostics must not wait on it"
        );
        assert!(
            client.is_alive(),
            "one hung request must not kill the session"
        );
        client.shutdown().await;
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_initialization_kills_server_process_group() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("main.testlang");
        std::fs::write(&path, "content").unwrap();
        let manager = LspManager::new(
            directory.path().to_path_buf(),
            Config {
                servers: vec![ServerAdapter {
                    id: "hanging-server".into(),
                    command: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "sleep 30 & echo $! > helper.pid; wait".into(),
                    ],
                    languages: vec![Language {
                        extension: "testlang".into(),
                        language_id: "testlang".into(),
                    }],
                    root_markers: Vec::new(),
                    requires_approval: false,
                    settings: Value::Null,
                }],
                startup_timeout: Duration::from_millis(100),
                // Test runners may already be sandboxed and reject nested
                // Seatbelt; this test targets process-group teardown.
                allow_network: true,
                ..Config::default()
            },
        );
        assert!(matches!(
            manager.diagnostics(&path).await,
            DiagnosticReport::Unavailable { .. }
        ));
        let helper_pid: i32 = std::fs::read_to_string(directory.path().join("helper.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut gone = false;
        for _ in 0..20 {
            if unsafe { libc::kill(helper_pid, 0) } == -1 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gone, "language-server helper process was orphaned");
    }

    #[tokio::test]
    async fn failed_start_is_visible_and_cached_per_root() {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\n",
        )
        .unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        let manager = LspManager::new(
            directory.path().to_path_buf(),
            Config {
                servers: vec![missing_adapter()],
                ..Config::default()
            },
        );

        assert!(matches!(
            manager.diagnostics(&path).await,
            DiagnosticReport::Unavailable { .. }
        ));
        assert!(matches!(
            manager.diagnostics(&path).await,
            DiagnosticReport::Unavailable { .. }
        ));
        let status = manager.status().await;
        assert_eq!(status.len(), 1);
        assert!(matches!(status[0].state, ServerState::Broken));
    }

    #[test]
    fn every_built_in_adapter_is_well_formed() {
        let servers = Config::default().servers;
        let mut extensions: std::collections::HashMap<&str, &str> =
            std::collections::HashMap::new();
        for adapter in &servers {
            assert!(!adapter.command.is_empty(), "{} has no command", adapter.id);
            assert!(
                !adapter.languages.is_empty(),
                "{} claims no extensions",
                adapter.id
            );
            for marker in &adapter.root_markers {
                // Roots are found with `join(marker).exists()`, so a glob is a
                // marker that can never match — silently, and the server would
                // just always fall back to the workspace root.
                assert!(
                    !marker.contains('*'),
                    "{}: root marker {marker:?} is a glob and will never match",
                    adapter.id
                );
            }
            for language in &adapter.languages {
                // Two servers claiming one extension means both are started for
                // every such file, doubling spawn cost and merging two opinions.
                if let Some(previous) = extensions.insert(&language.extension, &adapter.id) {
                    panic!(
                        "extension {:?} is claimed by both {previous} and {}",
                        language.extension, adapter.id
                    );
                }
            }
        }
    }

    /// A parked server has to be revivable without restarting Medha: a few
    /// transient crashes during a heavy build can exhaust the restart budget,
    /// and the cached error would otherwise disable code intelligence for the
    /// rest of the session.
    #[tokio::test]
    async fn an_explicit_start_clears_a_parked_entry() {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\n",
        )
        .unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        let manager = LspManager::new(
            directory.path().to_path_buf(),
            Config {
                servers: vec![missing_adapter()],
                max_restart_attempts: 0,
                ..Config::default()
            },
        );

        // Fail it hard enough that the backoff path will never replace it.
        let _ = manager.diagnostics(&path).await;
        let _ = manager.diagnostics(&path).await;
        assert!(matches!(
            manager.status().await[0].state,
            ServerState::Broken
        ));

        // The start still fails (the binary really is missing), but it must have
        // retired the parked entry rather than handing back the cached error.
        let _ = manager.approve_and_start(&path).await;
        let entries = manager.inner.clients.lock().await;
        assert!(
            entries.values().all(|entry| entry.failures == 0),
            "an explicit start must reset the restart budget"
        );
    }

    #[test]
    fn diagnostic_delta_shifts_unchanged_ranges_and_reports_real_changes() {
        let existing = Diagnostic {
            range: Range {
                start: Position {
                    line: 1,
                    character: 4,
                },
                end: Position {
                    line: 1,
                    character: 7,
                },
            },
            severity: Some(1),
            code: Some(json!("E1")),
            code_description: None,
            source: Some("fake".into()),
            message: "existing".into(),
            tags: Vec::new(),
            related_information: Vec::new(),
            data: None,
        };
        let mut shifted = existing.clone();
        shifted.range.start.line = 2;
        shifted.range.end.line = 2;
        let introduced_diagnostic = Diagnostic {
            range: Range {
                start: Position {
                    line: 3,
                    character: 0,
                },
                end: Position {
                    line: 3,
                    character: 3,
                },
            },
            message: "introduced".into(),
            severity: Some(2),
            code: None,
            code_description: None,
            source: Some("fake".into()),
            tags: Vec::new(),
            related_information: Vec::new(),
            data: None,
        };
        let baseline = DiagnosticBaseline {
            entries: vec![DiagnosticBaselineEntry {
                server: "fake".into(),
                text: "fn first() {}\nbad();\n".into(),
                diagnostics: vec![existing],
            }],
        };
        let report = DiagnosticReport::Fresh {
            server: "fake".into(),
            root: PathBuf::from("."),
            sources: vec!["fake".into()],
            warnings: Vec::new(),
            path: PathBuf::from("main.rs"),
            version: 2,
            diagnostics: vec![shifted, introduced_diagnostic.clone()],
            overflow: Vec::new(),
            total: 2,
            truncated: false,
            introduced: None,
            resolved: None,
        };
        let report = apply_diagnostic_delta(
            report,
            Some(&baseline),
            "// inserted\nfn first() {}\nbad();\nnew();\n",
        );
        let DiagnosticReport::Fresh {
            introduced,
            resolved,
            ..
        } = report
        else {
            panic!("expected fresh diagnostics");
        };
        assert_eq!(introduced, Some(vec![introduced_diagnostic]));
        assert_eq!(resolved, Some(Vec::new()));
    }

    #[test]
    fn document_symbols_accept_both_result_shapes() {
        let path = Path::new("/tmp/x.rs");
        let hierarchical = json!([{
            "name": "Parent", "kind": 5,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 9, "character": 0 } },
            "selectionRange": { "start": { "line": 0, "character": 6 }, "end": { "line": 0, "character": 12 } },
            "children": [{
                "name": "child", "kind": 6,
                "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 2, "character": 0 } },
                "selectionRange": { "start": { "line": 1, "character": 4 }, "end": { "line": 1, "character": 9 } }
            }]
        }]);
        let symbols = parse_document_symbols(&hierarchical, path);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[1].name, "child");
        assert_eq!(symbols[1].container_name.as_deref(), Some("Parent"));

        let flat = json!([{
            "name": "Flat", "kind": 12,
            "location": {
                "uri": file_uri(path).unwrap(),
                "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 4 } }
            }
        }]);
        let symbols = parse_document_symbols(&flat, path);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Flat");
    }

    #[test]
    fn call_hierarchy_maps_incoming_and_outgoing() {
        let uri = file_uri(Path::new("/tmp/x.rs")).unwrap();
        let incoming = json!([{
            "from": {
                "name": "caller", "kind": 12, "uri": uri,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 5 } },
                "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 6 } }
            },
            "fromRanges": []
        }]);
        assert_eq!(parse_call_hierarchy(&incoming, false)[0].name, "caller");
        let outgoing = json!([{ "to": {
            "name": "callee", "kind": 12, "uri": uri,
            "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 5 } }
        } }]);
        assert_eq!(parse_call_hierarchy(&outgoing, true)[0].name, "callee");
    }

    #[test]
    fn configuration_section_resolves_dotted_paths() {
        let settings = json!({ "rust-analyzer": { "check": { "command": "clippy" } } });
        assert_eq!(
            resolve_configuration_section(&settings, Some("rust-analyzer.check.command")),
            json!("clippy")
        );
        assert_eq!(
            resolve_configuration_section(&settings, Some("nope")),
            Value::Null
        );
        assert_eq!(resolve_configuration_section(&settings, None), settings);
    }

    #[tokio::test]
    async fn broken_server_parks_after_max_attempts() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        let manager = LspManager::new(
            directory.path().to_path_buf(),
            Config {
                servers: vec![missing_adapter()],
                restart_backoff: Duration::ZERO,
                max_restart_attempts: 2,
                ..Config::default()
            },
        );
        for _ in 0..6 {
            let _ = manager.diagnostics(&path).await;
        }
        let clients = manager.inner.clients.lock().await;
        assert_eq!(
            clients.values().next().unwrap().failures,
            2,
            "a repeatedly-broken server parks at the attempt cap instead of respawning forever"
        );
    }

    #[test]
    fn default_adapters_cover_the_v1_language_matrix() {
        let manager = LspManager::new(PathBuf::from("."), Config::default());
        for path in [
            "src/lib.rs",
            "web/app.ts",
            "web/view.tsx",
            "web/index.js",
            "service/main.py",
            "cmd/server.go",
            "native/main.c",
            "native/lib.cpp",
        ] {
            assert!(manager.supports(path), "missing adapter for {path}");
        }
        assert!(!manager.supports("README.md"));
        assert_eq!(
            server_install_hint("typescript-language-server"),
            "install typescript-language-server and typescript (for example with npm)"
        );
    }

    #[test]
    fn incremental_change_uses_utf16_positions() {
        let change = incremental_content_change("a😀b\nz", "a😀xy\nz");
        assert_eq!(
            change["range"]["start"],
            json!({ "line": 0, "character": 3 })
        );
        assert_eq!(change["range"]["end"], json!({ "line": 0, "character": 4 }));
        assert_eq!(change["text"], "xy");
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_failed_start() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        let manager = LspManager::new(
            directory.path().to_path_buf(),
            Config {
                servers: vec![missing_adapter()],
                ..Config::default()
            },
        );
        let (left, right) = tokio::join!(manager.diagnostics(&path), manager.diagnostics(&path));
        assert!(matches!(left, DiagnosticReport::Unavailable { .. }));
        assert!(matches!(right, DiagnosticReport::Unavailable { .. }));
        assert_eq!(manager.status().await.len(), 1);
    }

    #[tokio::test]
    async fn matching_servers_are_all_started_and_failures_are_merged() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        let mut first = missing_adapter();
        first.id = "first-rust-server".into();
        let mut second = missing_adapter();
        second.id = "second-rust-server".into();
        let manager = LspManager::new(
            directory.path().to_path_buf(),
            Config {
                servers: vec![first, second],
                ..Config::default()
            },
        );
        let report = manager
            .definition(
                &path,
                Position {
                    line: 0,
                    character: 3,
                },
            )
            .await;
        let QueryReport::Unavailable { reason } = report else {
            panic!("both unavailable servers should produce a visible failure");
        };
        assert!(reason.contains("first-rust-server"));
        assert!(reason.contains("second-rust-server"));
        assert_eq!(manager.status().await.len(), 2);
    }

    #[tokio::test]
    async fn project_defined_server_is_inert_until_explicit_approval() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("main.zig");
        std::fs::write(&path, "pub fn main() void {}").unwrap();
        let manager = LspManager::new(
            directory.path().to_path_buf(),
            Config {
                servers: vec![ServerAdapter {
                    id: "zls".into(),
                    command: vec!["medha-definitely-missing-zls".into()],
                    languages: language_mappings(&["zig".into()]),
                    root_markers: vec!["build.zig".into(), ".git".into()],
                    requires_approval: true,
                    settings: Value::Null,
                }],
                ..Config::default()
            },
        );
        let report = manager.diagnostics(&path).await;
        let DiagnosticReport::Unavailable { reason, .. } = report else {
            panic!("custom server must require approval");
        };
        assert!(reason.contains("requires approval"));
        let preview = manager.start_preview(&path).unwrap();
        assert_eq!(preview.command, vec!["medha-definitely-missing-zls"]);
        assert!(preview.approval_required);
        let error = manager.approve_and_start(&path).await.unwrap_err();
        assert!(matches!(error, Error::Protocol(_)));
    }

    #[tokio::test]
    async fn navigation_is_typed_sorted_deduplicated_and_bounded() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("main.rs");
        tokio::fs::write(&path, "fn target() {}\nfn main() { target(); }")
            .await
            .unwrap();
        let uri = file_uri(&path).unwrap();
        let (client_stream, server_stream) = duplex(64 * 1024);
        let (client_read, client_write) = split(client_stream);
        let (server_read, mut server_write) = split(server_stream);
        let server_uri = uri.clone();
        let server = tokio::spawn(async move {
            let mut server_read = BufReader::new(server_read);
            while let Some(message) = read_frame(&mut server_read).await.unwrap() {
                let Some(method) = message.get("method").and_then(Value::as_str) else {
                    continue;
                };
                let result = match method {
                    "initialize" => Some(json!({ "capabilities": {} })),
                    "textDocument/definition" => Some(json!([
                        {
                            "uri": server_uri,
                            "range": {
                                "start": { "line": 0, "character": 3 },
                                "end": { "line": 0, "character": 9 }
                            }
                        },
                        {
                            "targetUri": server_uri,
                            "targetSelectionRange": {
                                "start": { "line": 0, "character": 3 },
                                "end": { "line": 0, "character": 9 }
                            }
                        }
                    ])),
                    "textDocument/references" => Some(json!([{
                        "uri": server_uri,
                        "range": {
                            "start": { "line": 1, "character": 12 },
                            "end": { "line": 1, "character": 18 }
                        }
                    }])),
                    "textDocument/hover" => Some(json!({
                        "contents": { "kind": "markdown", "value": "123456789" }
                    })),
                    "workspace/symbol" => Some(json!([
                        {
                            "name": "Zulu", "kind": 12,
                            "location": {
                                "uri": server_uri,
                                "range": {
                                    "start": { "line": 1, "character": 3 },
                                    "end": { "line": 1, "character": 7 }
                                }
                            }
                        },
                        {
                            "name": "Alpha", "kind": 12,
                            "location": {
                                "uri": server_uri,
                                "range": {
                                    "start": { "line": 0, "character": 3 },
                                    "end": { "line": 0, "character": 9 }
                                }
                            }
                        },
                        {
                            "name": "Middle", "kind": 12,
                            "location": {
                                "uri": server_uri,
                                "range": {
                                    "start": { "line": 1, "character": 3 },
                                    "end": { "line": 1, "character": 7 }
                                }
                            }
                        }
                    ])),
                    "shutdown" => Some(Value::Null),
                    "exit" => break,
                    _ => None,
                };
                if let (Some(id), Some(result)) = (message.get("id"), result) {
                    let payload = serde_json::to_vec(&json!({
                        "jsonrpc": "2.0", "id": id, "result": result
                    }))
                    .unwrap();
                    write_frame(&mut server_write, &payload).await.unwrap();
                }
            }
        });
        let client = LspClient::from_io(
            "fake-rust-analyzer".to_string(),
            directory.path().to_path_buf(),
            fake_languages(),
            ClientTransport {
                reader: Box::new(client_read),
                writer: Box::new(client_write),
                child: None,
            },
            test_settings(Duration::from_secs(1), 2, 5),
            Value::Null,
        );
        client.initialize().await.unwrap();

        let QueryReport::Ready { items, total, .. } = client
            .definition(
                &path,
                Position {
                    line: 1,
                    character: 14,
                },
            )
            .await
            .unwrap()
        else {
            panic!("definition should be ready");
        };
        assert_eq!(total, 1, "Location and LocationLink duplicates merge");
        assert_eq!(items[0].range.start.line, 0);

        let QueryReport::Ready { items, .. } = client
            .hover(
                &path,
                Position {
                    line: 1,
                    character: 14,
                },
            )
            .await
            .unwrap()
        else {
            panic!("hover should be ready");
        };
        assert!(items[0].contents.starts_with("12345"));
        assert!(items[0].contents.contains("truncated"));

        let QueryReport::Ready {
            items,
            total,
            truncated,
            ..
        } = client.workspace_symbols("").await.unwrap()
        else {
            panic!("symbols should be ready");
        };
        assert_eq!(total, 3);
        assert!(truncated);
        assert_eq!(items[0].name, "Alpha");
        assert_eq!(items[1].name, "Middle");

        client.shutdown().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn broken_client_is_replaced_after_backoff() {
        let directory = tempdir().unwrap();
        std::fs::write(directory.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        let manager = LspManager::new(
            directory.path().to_path_buf(),
            Config {
                servers: vec![missing_adapter()],
                restart_backoff: Duration::ZERO,
                ..Config::default()
            },
        );
        let _ = manager.diagnostics(&path).await;
        let first = {
            let clients = manager.inner.clients.lock().await;
            Arc::clone(&clients.values().next().unwrap().cell)
        };
        let _ = manager.diagnostics(&path).await;
        let second = {
            let clients = manager.inner.clients.lock().await;
            Arc::clone(&clients.values().next().unwrap().cell)
        };
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn idle_clients_are_reaped_and_removed_from_status() {
        let directory = tempdir().unwrap();
        let (client_stream, server_stream) = duplex(1024);
        drop(server_stream);
        let (client_read, client_write) = split(client_stream);
        let client = Arc::new(LspClient::from_io(
            "fake-rust-analyzer".to_string(),
            directory.path().to_path_buf(),
            fake_languages(),
            ClientTransport {
                reader: Box::new(client_read),
                writer: Box::new(client_write),
                child: None,
            },
            ClientSettings {
                startup_timeout: Duration::from_millis(20),
                request_timeout: Duration::from_millis(20),
                diagnostics_timeout: Duration::from_millis(20),
                diagnostic_settle: Duration::from_millis(10),
                max_results: 20,
                max_text_chars: 1000,
                max_open_documents: 64,
            },
            Value::Null,
        ));
        let manager = LspManager::new(
            directory.path().to_path_buf(),
            Config {
                idle_timeout: Duration::ZERO,
                ..Config::default()
            },
        );
        let cell = Arc::new(OnceCell::new());
        assert!(cell.set(Ok(client)).is_ok());
        manager.inner.clients.lock().await.insert(
            ClientKey {
                server: "fake-rust-analyzer".into(),
                root: directory.path().to_path_buf(),
            },
            ClientEntry {
                cell,
                created_at: Instant::now(),
                last_used: Instant::now(),
                failures: 0,
            },
        );

        assert!(manager.status().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_rust_analyzer_end_to_end_when_available() {
        let available = Command::new("rust-analyzer")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success());
        if !available {
            eprintln!("skipping real rust-analyzer test: binary is unavailable");
            return;
        }

        let directory = tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = \"medha_lsp_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let source =
            "pub fn target() -> i32 { 1 }\n\npub fn caller() -> i32 { target() + missing_name }\n";
        let path = directory.path().join("src/lib.rs");
        std::fs::write(&path, source).unwrap();
        let manager = LspManager::new(
            directory.path().to_path_buf(),
            Config {
                startup_timeout: Duration::from_secs(30),
                request_timeout: Duration::from_secs(20),
                diagnostics_timeout: Duration::from_secs(30),
                // The test environment can prohibit nested OS sandboxes.
                allow_network: true,
                ..Config::default()
            },
        );

        timeout(Duration::from_secs(45), async {
            let diagnostics = manager.diagnostics(&path).await;
            let DiagnosticReport::Fresh { diagnostics, .. } = &diagnostics else {
                panic!("rust-analyzer did not return fresh diagnostics: {diagnostics:?}");
            };
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message.contains("missing_name")),
                "expected unresolved-name diagnostic, got {diagnostics:?}"
            );

            let call_character = source.lines().nth(2).unwrap().find("target").unwrap() as u32;
            let QueryReport::Ready { items, .. } = manager
                .definition(
                    &path,
                    Position {
                        line: 2,
                        character: call_character,
                    },
                )
                .await
            else {
                panic!("definition request was unavailable");
            };
            assert_eq!(items.first().map(|item| item.range.start.line), Some(0));

            let QueryReport::Ready { items, .. } = manager
                .references(
                    &path,
                    Position {
                        line: 0,
                        character: 7,
                    },
                    true,
                )
                .await
            else {
                panic!("references request was unavailable");
            };
            assert!(items.len() >= 2);

            let QueryReport::Ready { items, .. } = manager
                .hover(
                    &path,
                    Position {
                        line: 2,
                        character: call_character,
                    },
                )
                .await
            else {
                panic!("hover request was unavailable");
            };
            assert!(items.iter().any(|hover| hover.contents.contains("target")));

            let QueryReport::Ready { items, .. } = manager.workspace_symbols(&path, "target").await
            else {
                panic!("workspace-symbol request was unavailable");
            };
            assert!(items.iter().any(|symbol| symbol.name == "target"));

            let QueryReport::Ready { items, .. } = manager.document_symbols(&path).await else {
                panic!("document-symbol request was unavailable");
            };
            assert!(items.iter().any(|symbol| symbol.name == "target"));

            let QueryReport::Ready { items, .. } = manager
                .call_hierarchy(
                    &path,
                    Position {
                        line: 0,
                        character: 7,
                    },
                    false,
                )
                .await
            else {
                panic!("call-hierarchy request was unavailable");
            };
            assert!(items.iter().any(|symbol| symbol.name == "caller"));

            let baseline = manager
                .diagnostic_baseline(&path, source.to_string())
                .await
                .expect("current diagnostics should be reusable as a baseline");
            let edited = format!("// inserted line\n{source}pub fn other() {{ missing_other; }}\n");
            std::fs::write(&path, &edited).unwrap();
            let report = manager
                .diagnostics_after_edit(&path, edited, Some(baseline))
                .await;
            let DiagnosticReport::Fresh {
                introduced,
                resolved,
                ..
            } = report
            else {
                panic!("post-edit diagnostics were unavailable");
            };
            let introduced = introduced.expect("baseline should produce a delta");
            assert!(
                introduced
                    .iter()
                    .any(|diagnostic| diagnostic.message.contains("missing_other")),
                "expected missing_other in introduced delta, got {introduced:?}"
            );
            assert!(
                introduced
                    .iter()
                    .all(|diagnostic| !diagnostic.message.contains("missing_name")),
                "shifted pre-existing diagnostics must not be reported as new"
            );
            assert_eq!(resolved, Some(Vec::new()));
        })
        .await
        .expect("real rust-analyzer E2E timed out");

        manager.shutdown_all().await;
        assert!(manager.status().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_clangd_end_to_end_when_available() {
        let available = Command::new("clangd")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success());
        if !available {
            eprintln!("skipping real clangd test: binary is unavailable");
            return;
        }

        let directory = tempdir().unwrap();
        let source = "int target(void) { return 1; }\n\
                      int main(void) { return target(); }\n\
                      int broken(void) { return missing_name; }\n";
        let path = directory.path().join("main.c");
        std::fs::write(&path, source).unwrap();
        let manager = LspManager::new(
            directory.path().to_path_buf(),
            Config {
                startup_timeout: Duration::from_secs(20),
                request_timeout: Duration::from_secs(10),
                diagnostics_timeout: Duration::from_secs(15),
                // The test environment can prohibit nested OS sandboxes.
                allow_network: true,
                ..Config::default()
            },
        );

        timeout(Duration::from_secs(30), async {
            let DiagnosticReport::Fresh { diagnostics, .. } = manager.diagnostics(&path).await
            else {
                panic!("clangd did not return fresh diagnostics");
            };
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message.contains("missing_name")),
                "expected undeclared identifier diagnostic, got {diagnostics:?}"
            );

            let call_character = source.lines().nth(1).unwrap().find("target").unwrap() as u32 + 1;
            let QueryReport::Ready { items, .. } = manager
                .definition(
                    &path,
                    Position {
                        line: 1,
                        character: call_character,
                    },
                )
                .await
            else {
                panic!("clangd definition request was unavailable");
            };
            assert_eq!(items.first().map(|item| item.range.start.line), Some(0));

            let QueryReport::Ready { items, .. } = manager
                .hover(
                    &path,
                    Position {
                        line: 1,
                        character: call_character,
                    },
                )
                .await
            else {
                panic!("clangd hover request was unavailable");
            };
            assert!(items.iter().any(|hover| hover.contents.contains("target")));

            let QueryReport::Ready { items, .. } = manager.workspace_symbols(&path, "target").await
            else {
                panic!("clangd workspace-symbol request was unavailable");
            };
            assert!(items.iter().any(|symbol| symbol.name == "target"));
        })
        .await
        .expect("real clangd E2E timed out");

        manager.shutdown_all().await;
        assert!(manager.status().await.is_empty());
    }
}
