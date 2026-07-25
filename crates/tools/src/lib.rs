//! Tool families and the registry (§4.5). A `Tool` is a schema-bearing,
//! blast-radius-tagged capability; the `ToolRegistry` implements the kernel's
//! `Executor`, exposing specs (K2) and dispatching validated intents to the
//! right tool. Phase 0 ships fs + shell over the workspace sandbox.

use async_trait::async_trait;
use ignore::WalkBuilder;
use kernel::{BlastRadius, Executor, Observation, ToolCategory, ToolIntent, ToolSpec};

pub mod hub;
pub mod judge;
pub mod memory_tools;
pub mod skills;
pub use hub::{LockEntry, SearchResults, SkillHit, SkillLock, Tap, TapStore};
pub use judge::{JudgeOutcome, JudgeRequest, JudgeVerdict, SkillJudge};
use regex::{Regex, RegexBuilder};
use sandbox::WorkspaceSandbox;
use scraper::{Html, Selector};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
pub use skills::{InstallReport, SkillScope, SkillStore};
use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};

/// Build a compact unified-style diff (with a few lines of context) for display.
fn make_diff(path: &str, before: &str, after: &str) -> String {
    let diff = TextDiff::from_lines(before, after);
    let mut out = format!("--- {path}\n+++ {path}\n");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value());
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Hard ceiling on any single tool call (defense against stuck tools).
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("missing or invalid argument: {0}")]
    Args(String),
    #[error("{0}")]
    Failed(String),
    #[error("structured tool error")]
    Structured(Value),
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn blast_radius(&self) -> BlastRadius;
    /// JSON Schema for the parameters (what the model is told it can pass).
    fn schema(&self) -> Value;
    async fn execute(&self, args: &Value) -> Result<Value, ToolError>;

    /// Presentation category surfaces map to a glyph/verb (§4.13). Defaults from
    /// the blast radius; tools override for a finer class (Search/Web/Vcs/…).
    /// Declared here so surfaces read it, never re-deriving from the tool name.
    fn category(&self) -> ToolCategory {
        match self.blast_radius() {
            BlastRadius::Read => ToolCategory::Read,
            _ => ToolCategory::Write,
        }
    }

    /// The tool's display glyph (one grapheme). Defaults per category; a tool
    /// overrides it to keep a distinct icon. Declared here so surfaces render the
    /// tool's own glyph without holding a name→glyph table.
    fn icon(&self) -> &'static str {
        match self.category() {
            ToolCategory::Read => "◇",
            ToolCategory::Write => "✎",
            ToolCategory::Search => "⌕",
            ToolCategory::Web => "◍",
            ToolCategory::Shell => "❯",
            ToolCategory::Vcs => "⎇",
            ToolCategory::Diagnostic => "⚑",
            ToolCategory::Plan => "☑",
            ToolCategory::Other => "•",
        }
    }

    /// A side-effect-free preview of what this call would do (e.g. a diff or the
    /// command), shown at the human gate. Async so a preview can read the file's
    /// current contents to render a real before→after diff. Default: none.
    async fn preview(&self, _args: &Value) -> Option<String> {
        None
    }

    /// Hard wall-clock ceiling for one call. Default `Some(60s)` protects against
    /// a stuck tool. Tools that legitimately run long or self-manage their own
    /// bound return a larger value or `None` (no cap): `shell.exec` promotes a
    /// slow command to a background task instead of being killed, and
    /// `diagnostics`/`web.crawl` can exceed 60s on a big workspace/site. A `None`
    /// here means the tool is trusted to bound itself.
    fn timeout(&self) -> Option<std::time::Duration> {
        Some(TOOL_TIMEOUT)
    }
}

/// Cap a rendered diff so a huge change doesn't flood the approval card.
fn cap_preview(s: &str) -> String {
    const MAX_LINES: usize = 60;
    let total = s.lines().count();
    let mut out: Vec<&str> = s.lines().take(MAX_LINES).collect();
    if total > MAX_LINES {
        out.push("…");
        return format!("{}\n(+{} more lines)", out.join("\n"), total - MAX_LINES);
    }
    out.join("\n")
}

/// Helper: required string argument.
fn arg_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolError::Args(format!("expected string '{key}'")))
}

async fn pre_edit_lsp(
    handle: &LspHandle,
    sbx: &WorkspaceSandbox,
    path: &str,
    text: &str,
) -> Option<lsp::DiagnosticBaseline> {
    let manager = handle.lock().ok()?.clone()?;
    if !manager.supports(std::path::Path::new(path)) {
        return None;
    }
    let absolute = sbx.resolve(path).await.ok()?;
    manager
        .diagnostic_baseline(absolute, text.to_string())
        .await
}

async fn post_edit_lsp(
    handle: &LspHandle,
    sbx: &WorkspaceSandbox,
    artifacts: &Arc<dyn kernel::ArtifactStore>,
    path: &str,
    text: &str,
    baseline: Option<lsp::DiagnosticBaseline>,
) -> Option<Value> {
    let manager = handle.lock().ok()?.clone()?;
    if !manager.supports(std::path::Path::new(path)) {
        return None;
    }
    let absolute = sbx.resolve(path).await.ok()?;
    lsp_diagnostic_value(
        manager
            .diagnostics_after_edit(absolute, text.to_string(), baseline)
            .await,
        artifacts,
    )
    .ok()
}

fn attach_artifact(value: &mut Value, bytes: Vec<u8>, store: &dyn kernel::ArtifactStore) {
    let Ok(hash) = store.put(&bytes) else {
        return;
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("artifact_hash".into(), Value::String(hash));
        object.insert("artifact_bytes".into(), json!(bytes.len()));
    }
}

fn lsp_query_value<T: serde::Serialize>(
    report: lsp::QueryReport<T>,
    artifacts: &Arc<dyn kernel::ArtifactStore>,
) -> Result<Value, ToolError> {
    let artifact = match &report {
        lsp::QueryReport::Ready {
            server,
            root,
            sources,
            warnings,
            items,
            overflow,
            total,
            truncated: true,
        } => serde_json::to_vec(&json!({
            "server": server,
            "root": root,
            "sources": sources,
            "warnings": warnings,
            "items": items,
            "overflow": overflow,
            "total": total
        }))
        .ok(),
        _ => None,
    };
    let mut value =
        serde_json::to_value(report).map_err(|error| ToolError::Failed(error.to_string()))?;
    if let Some(bytes) = artifact {
        attach_artifact(&mut value, bytes, artifacts.as_ref());
    }
    Ok(value)
}

fn lsp_diagnostic_value(
    report: lsp::DiagnosticReport,
    artifacts: &Arc<dyn kernel::ArtifactStore>,
) -> Result<Value, ToolError> {
    let artifact = match &report {
        lsp::DiagnosticReport::Fresh {
            server,
            root,
            sources,
            warnings,
            path,
            version,
            diagnostics,
            overflow,
            total,
            introduced,
            resolved,
            truncated: true,
        } => serde_json::to_vec(&json!({
            "server": server,
            "root": root,
            "sources": sources,
            "warnings": warnings,
            "path": path,
            "version": version,
            "diagnostics": diagnostics,
            "overflow": overflow,
            "total": total,
            "introduced": introduced,
            "resolved": resolved
        }))
        .ok(),
        _ => None,
    };
    let mut value =
        serde_json::to_value(report).map_err(|error| ToolError::Failed(error.to_string()))?;
    if let Some(bytes) = artifact {
        attach_artifact(&mut value, bytes, artifacts.as_ref());
    }
    Ok(value)
}

struct LspStatus {
    manager: Arc<lsp::LspManager>,
}

struct LspStart {
    manager: Arc<lsp::LspManager>,
    sbx: Arc<WorkspaceSandbox>,
}

#[async_trait]
impl Tool for LspStart {
    fn name(&self) -> &str {
        "lsp.start"
    }
    fn description(&self) -> &str {
        "Approve and start the configured language server for a source file. Built-in servers \
         start lazily without this; project-defined commands require this human-gated action once \
         per session/root. Pass `install: true` when a report says the server's binary is missing \
         and Medha can fetch it — that downloads it into Medha's own directory first. Offer that \
         rather than silently falling back to text search: the user should know why code \
         intelligence is unavailable."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::External
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Source file selecting the configured server and project root" },
                "server": { "type": "string", "description": "Server id; required only when multiple project-defined servers match" },
                "install": { "type": "boolean", "description": "Fetch the server's binary first if Medha can (default false)" }
            },
            "required": ["path"]
        })
    }
    async fn preview(&self, args: &Value) -> Option<String> {
        let path = args.get("path")?.as_str()?;
        let server = args.get("server").and_then(Value::as_str);
        let absolute = self.sbx.resolve(path).await.ok()?;
        let preview = self.manager.start_preview_for(absolute, server).ok()?;
        let mut text = format!(
            "start language server '{}' in {}\ncommand: {}",
            preview.server,
            preview.root.display(),
            preview.command.join(" ")
        );
        // The install runs a package manager, so the approval has to show
        // exactly what will be fetched and where it lands.
        if args.get("install").and_then(Value::as_bool) == Some(true)
            && let Some((program, arguments)) = lsp::install_command(&preview.server)
        {
            text.push_str(&format!(
                "\n\nfirst install it:\n$ {program} {}\ninto {}",
                arguments.join(" "),
                lsp::server_install_dir()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_default()
            ));
        }
        Some(text)
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let server = args.get("server").and_then(Value::as_str);
        let absolute = self
            .sbx
            .resolve(&path)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        let mut installed = None;
        if args.get("install").and_then(Value::as_bool) == Some(true) {
            let target = match server {
                Some(id) => id.to_string(),
                None => self
                    .manager
                    .start_preview_for(absolute.clone(), None)
                    .map(|preview| preview.server)
                    .map_err(|error| ToolError::Failed(error.to_string()))?,
            };
            installed = Some(
                lsp::install_server(&target)
                    .await
                    .map_err(|error| ToolError::Failed(error.to_string()))?,
            );
        }
        let mut status = serde_json::to_value(
            self.manager
                .approve_and_start_for(absolute, server)
                .await
                .map_err(|error| ToolError::Failed(error.to_string()))?,
        )
        .map_err(|error| ToolError::Failed(error.to_string()))?;
        if let Some(note) = installed
            && let Some(object) = status.as_object_mut()
        {
            object.insert("installed".into(), Value::String(note));
        }
        Ok(status)
    }
}

#[async_trait]
impl Tool for LspStatus {
    fn name(&self) -> &str {
        "lsp.status"
    }

    fn description(&self) -> &str {
        "Show language-server sessions and whether each project-root server is starting, ready, or broken."
    }

    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: &Value) -> Result<Value, ToolError> {
        // Sessions alone cannot answer "why was that a text match" — servers
        // start lazily, so an empty list means either "nothing asked yet" or
        // "nothing installed". The inventory separates the two.
        Ok(json!({
            "servers": self.manager.status().await,
            "available": self.manager.inventory(),
        }))
    }
}

struct LspDiagnostics {
    manager: Arc<lsp::LspManager>,
    sbx: Arc<WorkspaceSandbox>,
    artifacts: Arc<dyn kernel::ArtifactStore>,
}

#[async_trait]
impl Tool for LspDiagnostics {
    fn name(&self) -> &str {
        "lsp.diagnostics"
    }

    fn description(&self) -> &str {
        "Synchronize a supported source file with its language server and return only a fresh diagnostic result. A timeout is no_fresh_data, never clean."
    }

    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative supported source file" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let absolute = self
            .sbx
            .resolve(&path)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        lsp_diagnostic_value(self.manager.diagnostics(absolute).await, &self.artifacts)
    }
}

fn lsp_position(args: &Value) -> Result<lsp::Position, ToolError> {
    let line = args
        .get("line")
        .and_then(Value::as_u64)
        .filter(|line| *line > 0)
        .ok_or_else(|| ToolError::Args("expected 1-based integer 'line'".into()))?;
    let character = args.get("character").and_then(Value::as_u64).unwrap_or(0);
    Ok(lsp::Position {
        line: (line - 1)
            .try_into()
            .map_err(|_| ToolError::Args("'line' is too large".into()))?,
        character: character
            .try_into()
            .map_err(|_| ToolError::Args("'character' is too large".into()))?,
    })
}

fn lsp_position_schema() -> Value {
    json!({
        "path": { "type": "string", "description": "Workspace-relative supported source file" },
        "line": { "type": "integer", "minimum": 1, "description": "1-based source line" },
        "character": { "type": "integer", "minimum": 0, "description": "0-based UTF-16 character offset (default 0)" }
    })
}

struct LspDefinition {
    manager: Arc<lsp::LspManager>,
    sbx: Arc<WorkspaceSandbox>,
    artifacts: Arc<dyn kernel::ArtifactStore>,
}

#[async_trait]
impl Tool for LspDefinition {
    fn name(&self) -> &str {
        "lsp.definition"
    }
    fn description(&self) -> &str {
        "Resolve the semantic definition at a source position using its long-lived language server."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": lsp_position_schema(),
            "required": ["path", "line"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let absolute = self
            .sbx
            .resolve(&path)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        lsp_query_value(
            self.manager.definition(absolute, lsp_position(args)?).await,
            &self.artifacts,
        )
    }
}

struct LspReferences {
    manager: Arc<lsp::LspManager>,
    sbx: Arc<WorkspaceSandbox>,
    artifacts: Arc<dyn kernel::ArtifactStore>,
}

#[async_trait]
impl Tool for LspReferences {
    fn name(&self) -> &str {
        "lsp.references"
    }
    fn description(&self) -> &str {
        "Find semantic references at a source position, including declaration by default."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }
    fn schema(&self) -> Value {
        let mut properties = lsp_position_schema();
        properties["include_declaration"] = json!({
            "type": "boolean",
            "description": "Include the declaration (default true)"
        });
        json!({
            "type": "object",
            "properties": properties,
            "required": ["path", "line"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let absolute = self
            .sbx
            .resolve(&path)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        let include = args
            .get("include_declaration")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        lsp_query_value(
            self.manager
                .references(absolute, lsp_position(args)?, include)
                .await,
            &self.artifacts,
        )
    }
}

struct LspHover {
    manager: Arc<lsp::LspManager>,
    sbx: Arc<WorkspaceSandbox>,
    artifacts: Arc<dyn kernel::ArtifactStore>,
}

#[async_trait]
impl Tool for LspHover {
    fn name(&self) -> &str {
        "lsp.hover"
    }
    fn description(&self) -> &str {
        "Return bounded semantic type/documentation hover text at a source position."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": lsp_position_schema(),
            "required": ["path", "line"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let absolute = self
            .sbx
            .resolve(&path)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        lsp_query_value(
            self.manager.hover(absolute, lsp_position(args)?).await,
            &self.artifacts,
        )
    }
}

struct LspSymbols {
    manager: Arc<lsp::LspManager>,
    sbx: Arc<WorkspaceSandbox>,
    artifacts: Arc<dyn kernel::ArtifactStore>,
}

#[async_trait]
impl Tool for LspSymbols {
    fn name(&self) -> &str {
        "lsp.symbols"
    }
    fn description(&self) -> &str {
        "Search semantic workspace symbols in the project containing path; results are sorted, deduplicated, and bounded."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Search
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Symbol-name query" },
                "path": { "type": "string", "description": "Source file selecting the language server and project root" }
            },
            "required": ["query", "path"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let query = arg_str(args, "query")?;
        let path = arg_str(args, "path")?;
        let absolute = self
            .sbx
            .resolve(&path)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        lsp_query_value(
            self.manager.workspace_symbols(absolute, &query).await,
            &self.artifacts,
        )
    }
}

struct LspImplementation {
    manager: Arc<lsp::LspManager>,
    sbx: Arc<WorkspaceSandbox>,
    artifacts: Arc<dyn kernel::ArtifactStore>,
}

#[async_trait]
impl Tool for LspImplementation {
    fn name(&self) -> &str {
        "lsp.implementation"
    }
    fn description(&self) -> &str {
        "Resolve implementations of the symbol at a source position (trait/interface methods, abstract definitions)."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": lsp_position_schema(),
            "required": ["path", "line"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let absolute = self
            .sbx
            .resolve(&path)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        lsp_query_value(
            self.manager
                .implementations(absolute, lsp_position(args)?)
                .await,
            &self.artifacts,
        )
    }
}

struct LspDocumentSymbols {
    manager: Arc<lsp::LspManager>,
    sbx: Arc<WorkspaceSandbox>,
    artifacts: Arc<dyn kernel::ArtifactStore>,
}

#[async_trait]
impl Tool for LspDocumentSymbols {
    fn name(&self) -> &str {
        "lsp.document_symbols"
    }
    fn description(&self) -> &str {
        "List the semantic symbol outline of a single source file, sorted, deduplicated, and bounded."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Search
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative supported source file" }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let absolute = self
            .sbx
            .resolve(&path)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        lsp_query_value(
            self.manager.document_symbols(absolute).await,
            &self.artifacts,
        )
    }
}

struct LspCallHierarchy {
    manager: Arc<lsp::LspManager>,
    sbx: Arc<WorkspaceSandbox>,
    artifacts: Arc<dyn kernel::ArtifactStore>,
}

#[async_trait]
impl Tool for LspCallHierarchy {
    fn name(&self) -> &str {
        "lsp.call_hierarchy"
    }
    fn description(&self) -> &str {
        "List callers (direction=incoming, default) or callees (direction=outgoing) of the symbol at a source position."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Search
    }
    fn schema(&self) -> Value {
        let mut properties = lsp_position_schema();
        properties["direction"] = json!({
            "type": "string",
            "enum": ["incoming", "outgoing"],
            "description": "incoming = callers (default), outgoing = callees"
        });
        json!({
            "type": "object",
            "properties": properties,
            "required": ["path", "line"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let absolute = self
            .sbx
            .resolve(&path)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        let outgoing = args.get("direction").and_then(Value::as_str) == Some("outgoing");
        lsp_query_value(
            self.manager
                .call_hierarchy(absolute, lsp_position(args)?, outgoing)
                .await,
            &self.artifacts,
        )
    }
}

struct McpStatus {
    manager: Arc<mcp::McpManager>,
}

#[async_trait]
impl Tool for McpStatus {
    fn name(&self) -> &str {
        "mcp.status"
    }
    fn description(&self) -> &str {
        "Show configured MCP servers and whether each is needs_approval, connecting, ready, or failed, with tool counts."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &Value) -> Result<Value, ToolError> {
        Ok(json!({ "servers": self.manager.status().await }))
    }
}

struct McpStart {
    manager: Arc<mcp::McpManager>,
}

#[async_trait]
impl Tool for McpStart {
    fn name(&self) -> &str {
        "mcp.start"
    }
    fn description(&self) -> &str {
        "Approve and start a configured MCP server so its tools become callable. Project-defined servers require this once per session."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::External
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "Configured MCP server id" }
            },
            "required": ["server"]
        })
    }
    async fn preview(&self, args: &Value) -> Option<String> {
        let server = args.get("server")?.as_str()?;
        let preview = self.manager.start_preview(server).await.ok()?;
        Some(format!(
            "start MCP server '{}'\n{}: {}",
            preview.server, preview.transport, preview.target
        ))
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let server = arg_str(args, "server")?;
        serde_json::to_value(
            self.manager
                // `None`: a model-invoked tool must never open a browser. A
                // server needing sign-in reports that state for the user to act on.
                .approve_and_connect(&server, None)
                .await
                .map_err(|error| ToolError::Failed(error.to_string()))?,
        )
        .map_err(|error| ToolError::Failed(error.to_string()))
    }
}

/// Delegate a bounded task to a child agent. The child is built from this
/// objective — nothing has to be registered first — and runs with its own
/// transcript, so its intermediate work never enters the parent's context.
struct AgentSpawn {
    control: Arc<orchestrator::AgentControl>,
    executor: Arc<Mutex<Option<Arc<dyn kernel::Executor>>>>,
    artifacts: Option<Arc<dyn kernel::ArtifactStore>>,
    /// Operator ceiling on one child's turns, from `[agents] max_turns`.
    max_turns: u32,
    /// The dispatching session. A background report is addressed to whoever
    /// asked for it at dispatch time, so this has to be known before the child
    /// runs — not resolved when it finishes.
    session: SessionHandle,
}

#[async_trait]
impl Tool for AgentSpawn {
    fn name(&self) -> &str {
        "agent.spawn"
    }
    fn icon(&self) -> &'static str {
        "⚇"
    }
    fn description(&self) -> &str {
        "Delegate a self-contained read-only investigation to a child agent and get back a summary. \
         The child works in its own context, so none of its searching lands in this conversation.\n\
         \n\
         Reach for this on your own judgement — you do not need to be asked. Delegate when:\n\
         · answering needs a broad sweep whose intermediate output you will never need again — \
         'how is X used across the codebase', 'what does this unfamiliar module do';\n\
         · two or more questions are independent, so children can run at once;\n\
         · a side investigation would otherwise crowd out the context you need for the real task;\n\
         · you want background gathered while you keep working — set `background`.\n\
         \n\
         Do not delegate work you can finish in a couple of tool calls, or anything you already \
         have the context for: a child starts cold and pays to rediscover what you know. Never hand \
         your entire task to one child — that is pass-through, and it doubles the cost for nothing. \
         Split off a *part*, or do it yourself.\n\
         \n\
         State the objective in full: the child cannot see this conversation and cannot ask you \
         anything. Give `contract` when the answer must have a particular shape. The child is \
         read-only and cannot use any tool you do not already have. Its report comes back to you, \
         not to the user — relay what matters."
    }
    fn blast_radius(&self) -> BlastRadius {
        // Read-only children: no mutation, but real model spend, so it stays
        // above a plain read.
        BlastRadius::ReversibleLocal
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Other
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "The complete task. The child sees only this — not this conversation."
                },
                "name": { "type": "string", "description": "Short label for the agent (optional)" },
                "contract": {
                    "type": "string",
                    "description": "What the result must contain, e.g. 'a list of file:line with one sentence each'"
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Narrow the child to these tools. Omit to inherit yours. Cannot exceed yours."
                },
                "max_turns": { "type": "integer", "description": "Turn ceiling, clamped to what remains" },
                "background": {
                    "type": "boolean",
                    "description": "Return immediately instead of waiting. The report arrives at the start of a later turn. Use for work you do not need before answering; keep it false when you need the answer now."
                }
            },
            "required": ["objective"]
        })
    }
    async fn preview(&self, args: &Value) -> Option<String> {
        Some(format!(
            "delegate to a read-only agent:\n{}",
            args.get("objective")?.as_str()?
        ))
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let objective = arg_str(args, "objective")?;
        let Some(parent) = self.executor.lock().ok().and_then(|e| e.clone()) else {
            return Err(ToolError::Failed(
                "the agent runtime is not available in this session".into(),
            ));
        };
        let spec = orchestrator::AgentSpec {
            name: args
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            objective,
            contract: args
                .get("contract")
                .and_then(Value::as_str)
                .map(str::to_string),
            tools: args.get("tools").and_then(Value::as_array).map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            }),
            max_turns: args
                .get("max_turns")
                .and_then(Value::as_u64)
                .map(|turns| turns as u32),
        };
        // Background: hand back the handle now, report arrives on a later turn.
        if args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let owner = self.session.lock().ok().and_then(|id| *id).ok_or_else(|| {
                ToolError::Failed("no session to deliver a background report to".into())
            })?;
            let handle = self
                .control
                .spawn_background(spec, parent, self.max_turns, owner)
                .await
                .map_err(|error| ToolError::Failed(error.to_string()))?;
            return Ok(json!({
                "agent": handle.agent,
                "session": handle.session,
                "status": "running",
                "note": "running in the background; its report will arrive at the start of a later turn. Do not wait for it — carry on, and do not invent its findings.",
            }));
        }
        // The parent's live turn counter is not visible at the tool boundary, so
        // the operator's ceiling is the bound. A child's turns are its own
        // session's anyway; the tokens are the shared cost, which this caps.
        let mut result = self
            .control
            .spawn(spec, parent, self.max_turns)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        // Bound what reaches the model, but persist the whole report first —
        // truncating before spilling would lose exactly the part worth keeping.
        let cap = orchestrator::MAX_SUMMARY_CHARS;
        if let Some(cut) = result
            .summary
            .char_indices()
            .nth(cap)
            .map(|(index, _)| index)
        {
            if let Some(store) = &self.artifacts
                && let Ok(hash) = store.put(result.summary.as_bytes())
            {
                result.artifact = Some(hash);
            }
            result.summary.truncate(cut);
            result.summary.push_str(match &result.artifact {
                Some(_) => "\n… truncated; read the rest with `read_artifact`",
                None => "\n… truncated",
            });
        }
        serde_json::to_value(result).map_err(|error| ToolError::Failed(error.to_string()))
    }
}

/// Shared slot for the session a background report belongs to.
pub type SessionHandle = Arc<Mutex<Option<ulid::Ulid>>>;

/// Inspect and stop running agents. Read-only listing plus a targeted stop, so
/// the model can abandon work it no longer needs rather than paying for it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentAction {
    List,
    Cancel,
    Transcript,
}

struct AgentControlTool {
    control: Arc<orchestrator::AgentControl>,
    action: AgentAction,
}

#[async_trait]
impl Tool for AgentControlTool {
    fn name(&self) -> &str {
        match self.action {
            AgentAction::List => "agent.list",
            AgentAction::Cancel => "agent.cancel",
            AgentAction::Transcript => "agent.transcript",
        }
    }
    fn icon(&self) -> &'static str {
        "⚇"
    }
    fn description(&self) -> &str {
        match self.action {
            AgentAction::Cancel => {
                "Stop one running agent by its name or session id. Its siblings keep running, and \
                 whatever it had found is still reported."
            }
            AgentAction::List => {
                "List the agents running right now, with what each was asked to do."
            }
            AgentAction::Transcript => {
                "Read what an agent actually did, by its session id. A report is a summary; when \
                 one looks thin, wrong, or was cut short, read the work behind it instead of \
                 guessing or re-running the search yourself."
            }
        }
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }
    fn schema(&self) -> Value {
        match self.action {
            AgentAction::List => json!({ "type": "object", "properties": {} }),
            AgentAction::Cancel => json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "Agent name or session id" }
                },
                "required": ["agent"]
            }),
            AgentAction::Transcript => json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "The agent's session id, as returned by agent.spawn" },
                    "tail": { "type": "integer", "description": "Only the last N steps (default: all)" }
                },
                "required": ["agent"]
            }),
        }
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        match self.action {
            AgentAction::List => Ok(json!({ "agents": self.control.active() })),
            AgentAction::Cancel => {
                let id = arg_str(args, "agent")?;
                let stopped = self.control.cancel(&id);
                if stopped.is_empty() {
                    return Err(ToolError::Failed(format!("no running agent '{id}'")));
                }
                Ok(json!({ "cancelled": stopped }))
            }
            AgentAction::Transcript => {
                let id = arg_str(args, "agent")?;
                let mut steps = self
                    .control
                    .transcript(&id)
                    .await
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
                if steps.is_empty() {
                    return Err(ToolError::Failed(format!(
                        "no transcript for '{id}' — pass the session id from agent.spawn, not the name"
                    )));
                }
                let total = steps.len();
                // A child can run for dozens of turns; the tail is usually where
                // the answer was forming when it stopped.
                if let Some(tail) = args.get("tail").and_then(Value::as_u64)
                    && (tail as usize) < total
                {
                    steps = steps.split_off(total - tail as usize);
                }
                Ok(json!({ "agent": id, "steps": steps, "total": total }))
            }
        }
    }
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// The workspace sandbox, kept so the executor can report its containment
    /// level (§4.8) to the kernel's trust-flow escalation. `None` for a bare
    /// registry built via [`ToolRegistry::new`].
    sandbox: Option<Arc<WorkspaceSandbox>>,
    /// Shared background-task table (promoted `shell.exec` runs), kept so the
    /// executor can report live tasks to a surface. `None` for a bare registry.
    tasks: Option<Arc<TaskTable>>,
    /// Live web-search configuration shared with the `web.*` tools. The TUI's
    /// `/search` writes it; the tools read it per call, so a provider change
    /// takes effect mid-session. Defaults to DuckDuckGo (with env fallback).
    search: SearchHandle,
    /// The surface's question-asker, populated by `main` per surface (TUI vs
    /// headless). `None` inside = no interactive user → `clarify` reports skipped.
    clarify: ClarifyHandle,
    /// Shared optional LSP manager. File mutation tools retain this handle so
    /// registering LSP after the default registry is built updates all of them.
    lsp: LspHandle,
    /// Optional MCP host. Its tools are projected dynamically each turn (so a
    /// server that connects mid-session appears without rebuilding the registry).
    mcp: Option<Arc<mcp::McpManager>>,
    artifacts: Option<Arc<dyn kernel::ArtifactStore>>,
    /// The executor a child inherits from — this registry itself, installed
    /// after the kernel is built. A child's tools are narrowed from it.
    agent_parent: Arc<Mutex<Option<Arc<dyn kernel::Executor>>>>,
    agent_session: SessionHandle,
}

/// Shared handle to the surface's question-asker. `main` sets it after building
/// the registry (TUI → an interactive asker; headless → left `None`).
pub type ClarifyHandle = Arc<Mutex<Option<Arc<dyn kernel::Asker>>>>;
type LspHandle = Arc<Mutex<Option<Arc<lsp::LspManager>>>>;

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            sandbox: None,
            tasks: None,
            search: Arc::new(Mutex::new(SearchSettings::default())),
            clarify: Arc::new(Mutex::new(None)),
            lsp: Arc::new(Mutex::new(None)),
            mcp: None,
            artifacts: None,
            agent_parent: Arc::new(Mutex::new(None)),
            agent_session: Arc::new(Mutex::new(None)),
        }
    }

    /// The shared search-settings handle. `main` populates it from the user's
    /// config after building the registry and hands a clone to the TUI, so the
    /// web tools and `/search` all read and write the same settings.
    pub fn search_handle(&self) -> SearchHandle {
        self.search.clone()
    }

    /// The shared clarify-asker handle. `main` sets it to the surface's asker
    /// after building the registry; left `None` for headless (no interactive user).
    pub fn clarify_handle(&self) -> ClarifyHandle {
        self.clarify.clone()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.insert(tool.name().to_string(), tool);
        self
    }

    /// Enable code intelligence and expose its explicit status/diagnostic
    /// tools. Existing file mutation tools immediately begin attaching fresh
    /// post-edit diagnostic reports through the shared handle.
    pub fn register_lsp(&mut self, manager: Arc<lsp::LspManager>) -> &mut Self {
        *self.lsp.lock().expect("LSP handle lock poisoned") = Some(manager.clone());
        self.register(Arc::new(LspStatus {
            manager: manager.clone(),
        }));
        if let Some(sbx) = self.sandbox.clone() {
            let Some(artifacts) = self.artifacts.clone() else {
                return self;
            };
            self.register(Arc::new(LspStart {
                manager: manager.clone(),
                sbx: sbx.clone(),
            }));
            self.register(Arc::new(LspDiagnostics {
                manager: manager.clone(),
                sbx: sbx.clone(),
                artifacts: artifacts.clone(),
            }));
            self.register(Arc::new(LspDefinition {
                manager: manager.clone(),
                sbx: sbx.clone(),
                artifacts: artifacts.clone(),
            }));
            self.register(Arc::new(LspReferences {
                manager: manager.clone(),
                sbx: sbx.clone(),
                artifacts: artifacts.clone(),
            }));
            self.register(Arc::new(LspHover {
                manager: manager.clone(),
                sbx: sbx.clone(),
                artifacts: artifacts.clone(),
            }));
            self.register(Arc::new(LspImplementation {
                manager: manager.clone(),
                sbx: sbx.clone(),
                artifacts: artifacts.clone(),
            }));
            self.register(Arc::new(LspDocumentSymbols {
                manager: manager.clone(),
                sbx: sbx.clone(),
                artifacts: artifacts.clone(),
            }));
            self.register(Arc::new(LspCallHierarchy {
                manager: manager.clone(),
                sbx: sbx.clone(),
                artifacts: artifacts.clone(),
            }));
            self.register(Arc::new(LspSymbols {
                manager,
                sbx,
                artifacts,
            }));
        }
        self
    }

    /// Enable the MCP host. Its discovered tools are projected via `specs()` and
    /// dispatched via `execute()`; `mcp.status`/`mcp.start` control the servers.
    pub fn register_mcp(&mut self, manager: Arc<mcp::McpManager>) -> &mut Self {
        self.mcp = Some(manager.clone());
        self.register(Arc::new(McpStatus {
            manager: manager.clone(),
        }));
        self.register(Arc::new(McpStart { manager }));
        self
    }

    /// Enable `agent.spawn`. The control plane is built before the kernel (the
    /// kernel owns this registry), so the parent executor is installed later via
    /// the returned handle — see [`Self::agent_parent_handle`].
    pub fn register_agents(
        &mut self,
        control: Arc<orchestrator::AgentControl>,
        max_turns: u32,
    ) -> &mut Self {
        self.register(Arc::new(AgentSpawn {
            control: control.clone(),
            executor: Arc::clone(&self.agent_parent),
            artifacts: self.artifacts.clone(),
            max_turns: max_turns.max(1),
            session: Arc::clone(&self.agent_session),
        }));
        for action in [
            AgentAction::List,
            AgentAction::Cancel,
            AgentAction::Transcript,
        ] {
            self.register(Arc::new(AgentControlTool {
                control: control.clone(),
                action,
            }));
        }
        self
    }

    /// Handle for the session background reports belong to. `main` fills it once
    /// the session id exists.
    pub fn agent_session_handle(&self) -> SessionHandle {
        Arc::clone(&self.agent_session)
    }

    /// Handle for installing the executor children inherit from. `main` fills it
    /// with the finished registry once the kernel exists.
    pub fn agent_parent_handle(&self) -> Arc<Mutex<Option<Arc<dyn kernel::Executor>>>> {
        Arc::clone(&self.agent_parent)
    }

    /// The names of every registered tool — used to validate a skill's
    /// `required_tools` against what this session can actually call.
    pub fn tool_names(&self) -> std::collections::HashSet<String> {
        self.tools.keys().cloned().collect()
    }

    /// Register the skill tools over the given store. Include the skill tools
    /// themselves in the frozen capability set so manifest availability, save
    /// validation, and actual loads agree.
    pub fn register_skills(&mut self, store: Arc<SkillStore>) -> &mut Self {
        let mut names = self.tool_names();
        names.extend(["skill.load", "skill.save", "skill.list"].map(String::from));
        let known = Arc::new(names);
        self.register(Arc::new(skills::SkillLoad {
            store: store.clone(),
            known_tools: known.clone(),
        }));
        self.register(Arc::new(skills::SkillList {
            store: store.clone(),
            known_tools: known.clone(),
        }));
        self.register(Arc::new(skills::SkillSave {
            store,
            known_tools: known,
        }));
        self
    }

    /// Persistent typed memory (D5): write/update/forget over the projection.
    /// Trust fields arrive kernel-injected at dispatch — see `memory_tools`.
    pub fn register_memory(&mut self, store: Arc<memory::MemoryProjection>) -> &mut Self {
        self.register_memory_configured(
            store,
            memory::recall::DEFAULT_K3_BUDGET_TOKENS,
            memory::recall::DEFAULT_STALE_AFTER_DAYS,
        )
    }

    pub fn register_memory_configured(
        &mut self,
        store: Arc<memory::MemoryProjection>,
        budget_tokens: u32,
        stale_after_days: u32,
    ) -> &mut Self {
        self.register(Arc::new(memory_tools::MemoryWrite::new_configured(
            store.clone(),
            budget_tokens,
            stale_after_days,
        )));
        self.register(Arc::new(memory_tools::MemoryUpdate {
            store: store.clone(),
        }));
        self.register(Arc::new(memory_tools::MemoryForget {
            store: store.clone(),
        }));
        self.register(Arc::new(memory_tools::MemorySearch { store }));
        self
    }

    /// Verbatim episodic recall over the persistent event log (D4).
    pub fn register_session_search(
        &mut self,
        log: Arc<store::SqliteLog>,
        artifacts: Arc<dyn kernel::ArtifactStore>,
    ) -> &mut Self {
        self.register(Arc::new(memory_tools::SessionsSearch { log, artifacts }));
        self
    }

    /// Convenience: a registry with the default fs + shell + search/edit tools,
    /// plus `read_artifact` over the given artifact store.
    pub fn with_workspace(
        sandbox: Arc<WorkspaceSandbox>,
        artifacts: Arc<dyn kernel::ArtifactStore>,
    ) -> Self {
        let mut r = Self::new();
        r.sandbox = Some(sandbox.clone());
        r.artifacts = Some(artifacts.clone());
        r.register(Arc::new(FsRead {
            sbx: sandbox.clone(),
        }));
        r.register(Arc::new(FsWrite {
            sbx: sandbox.clone(),
            lsp: r.lsp.clone(),
            artifacts: artifacts.clone(),
        }));
        r.register(Arc::new(FsList {
            sbx: sandbox.clone(),
        }));
        r.register(Arc::new(FsEdit {
            sbx: sandbox.clone(),
            pins: Default::default(),
            lsp: r.lsp.clone(),
            artifacts: artifacts.clone(),
        }));
        r.register(Arc::new(WordCount {
            sbx: sandbox.clone(),
        }));
        r.register(Arc::new(Grep {
            sbx: sandbox.clone(),
        }));
        r.register(Arc::new(Glob {
            sbx: sandbox.clone(),
        }));
        r.register(Arc::new(CodeOutline {
            sbx: sandbox.clone(),
        }));
        r.register(Arc::new(References {
            sbx: sandbox.clone(),
            lsp: Arc::clone(&r.lsp),
        }));
        r.register(Arc::new(Tree {
            sbx: sandbox.clone(),
        }));
        r.register(Arc::new(MultiEdit {
            sbx: sandbox.clone(),
            pins: Default::default(),
            lsp: r.lsp.clone(),
            artifacts: artifacts.clone(),
        }));
        r.register(Arc::new(Git {
            sbx: sandbox.clone(),
        }));
        r.register(Arc::new(Diagnostics {
            sbx: sandbox.clone(),
        }));
        // Background-task facility (§2): a slow `shell.exec` promotes to a task in
        // this shared table; `task.output`/`task.kill`/`task.list` operate on it,
        // and the executor exposes it to surfaces via `background_tasks()`.
        let tasks = Arc::new(TaskTable::default());
        r.tasks = Some(tasks.clone());
        r.register(Arc::new(ShellExec {
            sbx: sandbox,
            tasks: tasks.clone(),
        }));
        r.register(Arc::new(TaskOutput {
            tasks: tasks.clone(),
        }));
        r.register(Arc::new(TaskKill {
            tasks: tasks.clone(),
        }));
        r.register(Arc::new(TaskList { tasks }));
        r.register(Arc::new(ReadArtifact { store: artifacts }));
        r.register(Arc::new(WebSearch {
            search: r.search.clone(),
        }));
        r.register(Arc::new(WebFetch {
            search: r.search.clone(),
        }));
        r.register(Arc::new(WebCrawl {
            search: r.search.clone(),
        }));
        r.register(Arc::new(Clarify {
            asker: r.clarify.clone(),
        }));
        r.register(Arc::new(UpdatePlan));
        r
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for ToolRegistry {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .values()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                schema: t.schema(),
                blast_radius: t.blast_radius(),
                category: t.category(),
                icon: t.icon().to_string(),
            })
            .collect();
        // MCP tools are dynamic (a server may connect mid-session) and untrusted:
        // exposed as External so every call routes through the human gate.
        if let Some(mcp) = &self.mcp {
            specs.extend(mcp.tool_specs().into_iter().map(|t| ToolSpec {
                name: t.name,
                description: t.description,
                schema: t.schema,
                blast_radius: BlastRadius::External,
                category: ToolCategory::Other,
                icon: "•".to_string(),
            }));
        }
        specs.sort_by(|a, b| a.name.cmp(&b.name)); // stable exposure order
        specs
    }

    fn blast_radius(&self, tool: &str) -> Option<BlastRadius> {
        if mcp::McpManager::is_mcp_tool(tool) {
            return Some(BlastRadius::External);
        }
        self.tools.get(tool).map(|t| t.blast_radius())
    }

    fn category(&self, tool: &str) -> Option<ToolCategory> {
        if mcp::McpManager::is_mcp_tool(tool) {
            return Some(ToolCategory::Other);
        }
        self.tools.get(tool).map(|t| t.category())
    }

    fn containment(&self) -> kernel::Containment {
        self.sandbox
            .as_ref()
            .map(|s| s.containment())
            .unwrap_or(kernel::Containment::None)
    }

    fn background_tasks(&self) -> Vec<kernel::BackgroundTask> {
        self.tasks.as_ref().map(|t| t.info()).unwrap_or_default()
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        if mcp::McpManager::is_mcp_tool(&intent.tool) {
            let Some(mcp) = &self.mcp else {
                return Observation::denial(&intent.id, format!("unknown tool '{}'", intent.tool));
            };
            return match mcp.call(&intent.tool, &intent.args).await {
                Ok(out) => {
                    // Bound what reaches the model; the full result is preserved
                    // losslessly in the artifact store rather than discarded.
                    let cap = mcp.max_text_chars();
                    let cut = out.text.char_indices().nth(cap).map(|(index, _)| index);
                    let mut payload = json!({
                        "server": out.server,
                        "tool": out.tool,
                        "content": cut.map_or(out.text.as_str(), |index| &out.text[..index]),
                        "truncated": cut.is_some(),
                    });
                    if let (Some(_), Some(store)) = (cut, &self.artifacts) {
                        attach_artifact(&mut payload, out.text.into_bytes(), store.as_ref());
                    }
                    if out.is_error {
                        Observation {
                            intent_id: intent.id.clone(),
                            status: kernel::ObsStatus::Error,
                            payload,
                        }
                    } else {
                        Observation::ok(&intent.id, payload)
                    }
                }
                Err(e) => Observation::error(&intent.id, e.to_string()),
            };
        }
        let Some(tool) = self.tools.get(&intent.tool) else {
            return Observation::denial(&intent.id, format!("unknown tool '{}'", intent.tool));
        };
        // Per-tool timeout so a stuck tool never hangs the session. The ceiling
        // is the tool's own (default 60s); tools that self-manage a longer run
        // (shell.exec promotes to background; diagnostics/web.crawl) return a
        // larger value or `None` for no cap. On timeout the run future is dropped
        // — and for exec-backed tools that drop tears down the whole process
        // group (see `GroupReaper`), so nothing is orphaned.
        let run = tool.execute(&intent.args);
        let result = match tool.timeout() {
            Some(limit) => match tokio::time::timeout(limit, run).await {
                Ok(r) => r,
                Err(_) => {
                    return Observation::error(
                        &intent.id,
                        format!(
                            "tool '{}' timed out after {}s",
                            intent.tool,
                            limit.as_secs()
                        ),
                    );
                }
            },
            None => run.await,
        };
        match result {
            Ok(payload) => Observation::ok(&intent.id, payload),
            Err(ToolError::Structured(payload)) => Observation {
                intent_id: intent.id.clone(),
                status: kernel::ObsStatus::Error,
                payload,
            },
            Err(e) => Observation::error(&intent.id, e.to_string()),
        }
    }

    async fn preview(&self, intent: &ToolIntent) -> Option<String> {
        match self.tools.get(&intent.tool) {
            Some(t) => t.preview(&intent.args).await,
            None => None,
        }
    }
}

// ── fs family ──────────────────────────────────────────────────────────────

struct FsRead {
    sbx: Arc<WorkspaceSandbox>,
}
#[async_trait]
impl Tool for FsRead {
    fn name(&self) -> &str {
        "fs.read"
    }
    fn description(&self) -> &str {
        "Read a UTF-8 text file (workspace, or outside with permission). Reads the \
         whole file by default; for large files pass `offset` (1-based start line) \
         and/or `limit` (line count) to read just a slice and save context. Typical \
         flow: `glob`/`grep` to locate the file and line, then read the range around \
         it rather than the whole file."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative or absolute path" },
                "offset": { "type": "integer", "description": "1-based line to start at (optional; default whole file)" },
                "limit": { "type": "integer", "description": "Max lines to return from offset (optional)" }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let offset = args.get("offset").and_then(Value::as_u64);
        let limit = args.get("limit").and_then(Value::as_u64);
        // Size guard (P2): refuse a whole-file read of a huge file BEFORE
        // loading it — otherwise it lands in memory and the event log intact
        // (only model context was protected by the 16KB spill). Ranged reads
        // stay allowed; point the model at them.
        const MAX_WHOLE_READ: u64 = 2_000_000;
        if offset.is_none() && limit.is_none() {
            if let Ok(p) = self.sbx.resolve(&path).await {
                if let Ok(md) = std::fs::metadata(&p) {
                    if md.len() > MAX_WHOLE_READ {
                        return Err(ToolError::Failed(format!(
                            "{path} is {} bytes — too large for a whole-file read (cap {MAX_WHOLE_READ}). \
                             Read a range with offset/limit, or grep it.",
                            md.len()
                        )));
                    }
                }
            }
        }
        let content = self
            .sbx
            .read(&path)
            .await
            .map_err(|e| ToolError::Failed(format!("read failed: {}", e)))?;
        // Whole-file read (default) — unchanged behavior.
        if offset.is_none() && limit.is_none() {
            return Ok(json!({ "path": path, "content": content }));
        }
        // Line-range read: return just the requested slice plus positioning info.
        // Split with `split_inclusive` so each line KEEPS its terminator — CRLF
        // stays CRLF and the final line keeps (or lacks) its trailing newline.
        // `content.lines()` used to strip `\r` and the last `\n`, so the model
        // copied LF-only text that then failed byte-exact against a CRLF file in
        // `fs.edit`/`multi_edit` ("old_string not found"). Raw slices fix that.
        let lines: Vec<&str> = content.split_inclusive('\n').collect();
        let total = lines.len();
        let start = (offset.unwrap_or(1).max(1) as usize - 1).min(total);
        // `saturating_add` so `start + limit` can't wrap in release and silently
        // return empty content for a large `limit`.
        let end = limit
            .map(|l| start.saturating_add(l as usize).min(total))
            .unwrap_or(total);
        let slice = lines.get(start..end).unwrap_or(&[]).concat();
        // Flag an offset that lands past EOF, so an empty slice isn't read as
        // "the file ends here" when really the offset overshot.
        let beyond_eof = offset.is_some() && start >= total && total > 0;
        Ok(json!({
            "path": path,
            "content": slice,
            "start_line": start + 1,
            "end_line": end,
            "total_lines": total,
            "note": if beyond_eof {
                Some(format!("offset {} is past end of file ({total} lines)", offset.unwrap_or(0)))
            } else {
                None
            },
        }))
    }
}

struct FsWrite {
    sbx: Arc<WorkspaceSandbox>,
    lsp: LspHandle,
    artifacts: Arc<dyn kernel::ArtifactStore>,
}
#[async_trait]
impl Tool for FsWrite {
    fn name(&self) -> &str {
        "fs.write"
    }
    fn description(&self) -> &str {
        "Write a whole UTF-8 text file (creates parent dirs; snapshots any prior \
         version; returns a diff). Use this for NEW files or full rewrites; prefer \
         `fs.edit` to change part of an existing file (smaller, reviewable diff). \
         Supports paths outside workspace with permission."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::ReversibleLocal
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative or absolute path" },
                "content": { "type": "string", "description": "File contents" }
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let content = arg_str(args, "content")?;
        // Serialize same-file writes within a turn (P0-4).
        let _guard = self.sbx.path_guard(&path).await;
        // Capture the prior contents (empty for a new file) so surfaces can render
        // a proper diff: a new file shows as all-additions (empty left column),
        // an overwrite shows the real before/after — same view as fs.edit.
        let (old, unreadable) = read_or_flag_unreadable(&self.sbx, &path).await;
        let baseline = if unreadable {
            None
        } else {
            pre_edit_lsp(&self.lsp, &self.sbx, &path, &old).await
        };
        let snapshot = self
            .sbx
            .write(&path, &content)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let mut out = json!({ "path": path, "written": true, "snapshot": snapshot, "old": old, "new": content });
        if unreadable {
            out["note"] = json!(
                "prior file existed but was unreadable (non-UTF-8 or permission denied); diff shows it as empty"
            );
        }
        if let Some(report) = post_edit_lsp(
            &self.lsp,
            &self.sbx,
            &self.artifacts,
            &path,
            &content,
            baseline,
        )
        .await
        {
            out["lsp"] = report;
        }
        Ok(out)
    }

    /// The gate preview for a write is a diff against the file's current state
    /// (all additions for a brand-new file, or a real change diff when it
    /// already exists), so the approval card shows exactly what will land —
    /// capped so a large file doesn't flood the prompt.
    async fn preview(&self, args: &Value) -> Option<String> {
        let path = args.get("path")?.as_str()?;
        let content = args.get("content")?.as_str()?;
        // Diff against the file's current state: a brand-new file shows as all
        // additions; an overwrite shows the real before→after change. An existing
        // but unreadable file must NOT masquerade as "new file" — that hides a
        // destructive overwrite from the approval card.
        let (old, unreadable) = read_or_flag_unreadable(&self.sbx, path).await;
        let diff = cap_preview(&make_diff(path, &old, content));
        if unreadable {
            Some(format!(
                "⚠ OVERWRITES an existing file whose contents can't be read (non-UTF-8 or permission denied) — this is not a new file.\n{diff}"
            ))
        } else {
            Some(diff)
        }
    }
}

/// Read a file's contents for diffing; distinguish "doesn't exist" (empty, false)
/// from "exists but unreadable" (empty, true) so previews can't call a
/// destructive overwrite a new file.
async fn read_or_flag_unreadable(sbx: &WorkspaceSandbox, path: &str) -> (String, bool) {
    match sbx.read(path).await {
        Ok(s) => (s, false),
        Err(_) => {
            let exists = match sbx.resolve(path).await {
                Ok(p) => p.exists(),
                Err(_) => false,
            };
            (String::new(), exists)
        }
    }
}

struct FsList {
    sbx: Arc<WorkspaceSandbox>,
}
#[async_trait]
impl Tool for FsList {
    fn name(&self) -> &str {
        "fs.list"
    }
    fn icon(&self) -> &'static str {
        "▸"
    }
    fn description(&self) -> &str {
        "List the immediate entries of a directory (directories suffixed with '/'). \
         For recursive discovery by name use `glob`; to search file contents use \
         `grep`. Supports paths outside workspace with permission."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Workspace-relative or absolute dir (default '.')" } }
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let entries = self
            .sbx
            .list(path)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(json!({ "path": path, "entries": entries }))
    }
}

/// Preview→execute content pins (P1-1). `preview` records a hash of the file
/// content the approval card was rendered from, keyed by the exact tool args;
/// `execute` takes the pin and refuses if the file changed in between — the
/// approved diff and the applied diff must be the same diff.
#[derive(Default)]
struct PreviewPins(std::sync::Mutex<std::collections::HashMap<u64, u64>>);

impl PreviewPins {
    fn pin(&self, args: &Value, content: &str) {
        let mut map = self.0.lock().unwrap();
        if map.len() > 64 {
            map.clear(); // bound stale pins from denied/abandoned previews
        }
        map.insert(content_hash(&args.to_string()), content_hash(content));
    }
    /// Take the pin for these args (if any) and verify the content still matches.
    fn check(&self, args: &Value, path: &str, content: &str) -> Result<(), ToolError> {
        let pinned = self
            .0
            .lock()
            .unwrap()
            .remove(&content_hash(&args.to_string()));
        match pinned {
            Some(h) if h != content_hash(content) => Err(ToolError::Failed(format!(
                "{path} changed after the approved preview; re-run the edit to preview the current content"
            ))),
            _ => Ok(()),
        }
    }
}

fn content_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

struct FsEdit {
    sbx: Arc<WorkspaceSandbox>,
    pins: PreviewPins,
    lsp: LspHandle,
    artifacts: Arc<dyn kernel::ArtifactStore>,
}
#[async_trait]
impl Tool for FsEdit {
    fn name(&self) -> &str {
        "fs.edit"
    }
    fn description(&self) -> &str {
        "Edit a file by replacing an exact substring. `old_string` must match uniquely unless `replace_all` is true. Returns a unified diff. Supports paths outside workspace with permission."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::ReversibleLocal
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative or absolute path" },
                "old_string": { "type": "string", "description": "Exact text to replace" },
                "new_string": { "type": "string", "description": "Replacement text" },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let old_s = arg_str(args, "old_string")?;
        let new_s = arg_str(args, "new_string")?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        validate_edit(&old_s, &new_s)?;

        // Serialize same-file edits within a turn so a concurrent edit can't read
        // the same original and clobber this one (P0-4).
        let _guard = self.sbx.path_guard(&path).await;
        let content = self
            .sbx
            .read(&path)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        // Refuse if the file changed since the approved preview (P1-1).
        self.pins.check(args, &path, &content)?;
        // CRLF-tolerant byte-exact match (see `resolve_edit`).
        let (old_s, new_s) = resolve_edit(&content, &old_s, &new_s)
            .ok_or_else(|| ToolError::Failed(format!("old_string not found in {path}")))?;
        let count = content.matches(&old_s).count();
        if count > 1 && !replace_all {
            return Err(ToolError::Failed(format!(
                "old_string appears {count} times in {path}; pass replace_all or use a more specific string"
            )));
        }
        let updated = if replace_all {
            content.replace(&old_s, &new_s)
        } else {
            content.replacen(&old_s, &new_s, 1)
        };
        let baseline = pre_edit_lsp(&self.lsp, &self.sbx, &path, &content).await;
        let snapshot = self
            .sbx
            .write(&path, &updated)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let diff = make_diff(&path, &content, &updated);
        let mut out = json!({
            "path": path,
            "diff": diff,
            // Raw before/after content too, so a richer surface (e.g. a TUI)
            // can build its own view (side-by-side) instead of re-parsing the
            // pre-rendered unified diff string.
            "old": content,
            "new": updated,
            "replacements": if replace_all { count } else { 1 },
            "snapshot": snapshot
        });
        if let Some(report) = post_edit_lsp(
            &self.lsp,
            &self.sbx,
            &self.artifacts,
            &path,
            &updated,
            baseline,
        )
        .await
        {
            out["lsp"] = report;
        }
        Ok(out)
    }

    /// Compute the diff without writing — the real preview for the gate. Mirrors
    /// `execute`'s match logic so the card reflects what will actually happen
    /// (including the "not found" / "ambiguous match" cases that would fail).
    async fn preview(&self, args: &Value) -> Option<String> {
        let path = args.get("path")?.as_str()?;
        let old_s = args.get("old_string")?.as_str()?;
        let new_s = args.get("new_string")?.as_str()?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let content = self.sbx.read(path).await.ok()?;
        // Pin what the approval card will show (P1-1).
        self.pins.pin(args, &content);
        let count = content.matches(old_s).count();
        if count == 0 {
            return Some(format!(
                "(old_string not found in {path} — this edit would fail)"
            ));
        }
        if count > 1 && !replace_all {
            return Some(format!(
                "(old_string appears {count}× in {path}; needs replace_all or a more specific match)"
            ));
        }
        let updated = if replace_all {
            content.replace(old_s, new_s)
        } else {
            content.replacen(old_s, new_s, 1)
        };
        Some(cap_preview(&make_diff(path, &content, &updated)))
    }
}

// ── word_count ───────────────────────────────────────────────────────────────
//
// A simple read-only tool that counts words, lines, and characters in a workspace
// file. Useful for the model to get a quick sense of file size without reading
// the full content.

struct WordCount {
    sbx: Arc<WorkspaceSandbox>,
}

#[async_trait]
impl Tool for WordCount {
    fn name(&self) -> &str {
        "word_count"
    }
    fn icon(&self) -> &'static str {
        "#"
    }
    fn description(&self) -> &str {
        "Count words, lines, and characters in a workspace file. Supports paths outside workspace with permission."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative or absolute path to the file" }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let content = self
            .sbx
            .read(&path)
            .await
            .map_err(|e| ToolError::Failed(format!("read failed: {}", e)))?;
        let lines: Vec<&str> = content.lines().collect();
        let line_count = lines.len();
        let char_count = content.chars().count();
        // Word count: split on whitespace
        let word_count = content.split_whitespace().count();
        Ok(json!({
            "path": path,
            "lines": line_count,
            "words": word_count,
            "chars": char_count,
        }))
    }
}

// ── search family ────────────────────────────────────────────────────────────

struct Grep {
    sbx: Arc<WorkspaceSandbox>,
}
#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Search
    }
    fn description(&self) -> &str {
        "Search file contents by regular expression across the workspace, \
         gitignore-aware (skips ignored/hidden files, target/, node_modules, binaries). \
         Returns matching {path, line, text}. Use `context` to include surrounding \
         lines, `case_insensitive` for loose matching, `path` to scope to a subtree."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression (Rust regex syntax)" },
                "path": { "type": "string", "description": "Workspace-relative dir to scope the search (default '.')" },
                "case_insensitive": { "type": "boolean", "description": "Match ignoring case (default false)" },
                "context": { "type": "integer", "description": "Lines of context to include around each match (default 0)" },
                "max_results": { "type": "integer", "description": "Cap on matches returned (default 200)" }
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let pattern = arg_str(args, "pattern")?;
        let ci = args
            .get("case_insensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let context = args.get("context").and_then(Value::as_u64).unwrap_or(0) as usize;
        let max = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(200) as usize;
        let rel_path = args.get("path").and_then(Value::as_str).unwrap_or(".");

        let re = RegexBuilder::new(&pattern)
            .case_insensitive(ci)
            .build()
            .map_err(|e| ToolError::Args(format!("bad regex: {e}")))?;
        let start = self
            .sbx
            .resolve(rel_path)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let root = self.sbx.root().to_path_buf();

        // gitignore-aware, parallel-capable walk (ripgrep's engine). Standard
        // filters skip .git, hidden files (incl .medha), and gitignored paths;
        // we additionally skip build dirs that may not be gitignored.
        // Full walk + reads are blocking work — off the async runtime.
        tokio::task::spawn_blocking(move || {
            let mut matches: Vec<Value> = Vec::new();
            let mut truncated = false;
            let mut skipped_large = 0usize;
            let walk = WalkBuilder::new(&start)
                .standard_filters(true)
                .filter_entry(|e| !skip_dir(e))
                .build();

            'outer: for dent in walk.flatten() {
                if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                if dent.metadata().map(|m| m.len()).unwrap_or(0) > 1_000_000 {
                    skipped_large += 1;
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(dent.path()) else {
                    continue; // skip binaries (non-UTF-8)
                };
                let rel = dent.path().strip_prefix(&root).unwrap_or(dent.path()).to_string_lossy().into_owned();
                let lines: Vec<&str> = content.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    if re.is_match(line) {
                        if matches.len() >= max {
                            truncated = true;
                            break 'outer;
                        }
                        let mut m = json!({
                            "path": rel,
                            "line": i + 1,
                            "text": clip_marked(line, 200)
                        });
                        if context > 0 {
                            let lo = i.saturating_sub(context);
                            let hi = (i + context + 1).min(lines.len());
                            let ctx: Vec<String> =
                                (lo..hi).map(|j| format!("{}: {}", j + 1, lines[j])).collect();
                            m["context"] = json!(ctx);
                        }
                        matches.push(m);
                    }
                }
            }
            let count = matches.len();
            let mut out = json!({ "matches": matches, "count": count, "truncated": truncated });
            if skipped_large > 0 {
                out["note"] = json!(format!(
                    "[skipped {skipped_large} file(s) >1MB — not searched; use shell.exec grep for those]"
                ));
            }
            Ok(out)
        })
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?
    }
}

/// Clip a line to `max` chars with an explicit marker (never silently truncate).
fn clip_marked(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut s: String = line.chars().take(max).collect();
    s.push_str(" …[line truncated]");
    s
}

/// Skip build dirs that may not be gitignored (`.git`/hidden/.medha are already
/// excluded by `standard_filters`).
fn skip_dir(e: &ignore::DirEntry) -> bool {
    e.file_type().map(|t| t.is_dir()).unwrap_or(false)
        && matches!(e.file_name().to_str(), Some("target" | "node_modules"))
}

struct Glob {
    sbx: Arc<WorkspaceSandbox>,
}
#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }
    fn icon(&self) -> &'static str {
        "✦"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Search
    }
    fn description(&self) -> &str {
        "Find files by name pattern across the whole workspace (recursive, \
         gitignore-aware), e.g. '**/*.rs' or 'src/**/test_*.py'. Faster and more \
         reliable than `find` via shell.exec for locating files by name; use \
         `grep` instead when searching by file *content*."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, e.g. '**/*.rs'" },
                "max_results": { "type": "integer", "description": "Cap on matches (default 500)" }
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let pattern = arg_str(args, "pattern")?;
        let max = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(500) as usize;
        let matcher = glob::Pattern::new(&pattern).map_err(|e| ToolError::Args(e.to_string()))?;
        let root = self.sbx.root().to_path_buf();

        // Full workspace walk is blocking work — off the async runtime.
        tokio::task::spawn_blocking(move || {
            let mut matches: Vec<String> = Vec::new();
            let mut truncated = false;
            let walk = WalkBuilder::new(&root).standard_filters(true).filter_entry(|e| !skip_dir(e)).build();
            for dent in walk.flatten() {
                if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let rel = dent.path().strip_prefix(&root).unwrap_or(dent.path()).to_string_lossy().into_owned();
                // `require_literal_separator` so `*` and `?` do NOT cross `/`:
                // `*.rs` matches `main.rs` but not `src/main.rs`, and `src/*.rs`
                // doesn't reach `src/a/b.rs`. Use `**` to span directories. Without
                // this, plain `*` spans path separators and over-matches wildly.
                if matcher.matches_with(&rel, glob::MatchOptions {
                    require_literal_separator: true,
                    ..Default::default()
                }) {
                    if matches.len() >= max {
                        truncated = true;
                        break;
                    }
                    matches.push(rel);
                }
            }
            matches.sort();
            let count = matches.len();
            Ok(json!({ "pattern": pattern, "matches": matches, "count": count, "truncated": truncated }))
        })
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?
    }
}

// ── code intelligence ─────────────────────────────────────────────────────────

struct CodeOutline {
    sbx: Arc<WorkspaceSandbox>,
}

/// Language-aware, line-based symbol patterns. Each rule has a `kind` label and a
/// regex with a named `name` capture. Deliberately heuristic (no full parse): a
/// fast, dependency-light table of contents, not a compiler front-end.
fn outline_rules(path: &str) -> Vec<(&'static str, Regex)> {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    let r = |p: &str| Regex::new(p).unwrap();
    match ext.as_str() {
        "rs" => vec![
            (
                "fn",
                r(
                    r"^\s*(?:pub\s+)?(?:pub\([^)]*\)\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?(?:extern\s+\S+\s+)?fn\s+(?P<name>\w+)",
                ),
            ),
            (
                "struct",
                r(r"^\s*(?:pub\s+)?(?:pub\([^)]*\)\s+)?struct\s+(?P<name>\w+)"),
            ),
            ("enum", r(r"^\s*(?:pub\s+)?enum\s+(?P<name>\w+)")),
            (
                "trait",
                r(r"^\s*(?:pub\s+)?(?:unsafe\s+)?trait\s+(?P<name>\w+)"),
            ),
            ("impl", r(r"^\s*impl(?:<[^>]*>)?\s+(?P<name>[\w:]+)")),
            ("mod", r(r"^\s*(?:pub\s+)?mod\s+(?P<name>\w+)")),
            ("type", r(r"^\s*(?:pub\s+)?type\s+(?P<name>\w+)")),
            ("macro", r(r"^\s*macro_rules!\s*(?P<name>\w+)")),
        ],
        "py" => vec![
            ("class", r(r"^\s*class\s+(?P<name>\w+)")),
            ("def", r(r"^\s*(?:async\s+)?def\s+(?P<name>\w+)")),
        ],
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => vec![
            (
                "class",
                r(r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+(?P<name>\w+)"),
            ),
            (
                "function",
                r(r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\*?\s+(?P<name>\w+)"),
            ),
            (
                "const",
                r(
                    r"^\s*(?:export\s+)?(?:const|let|var)\s+(?P<name>\w+)\s*=\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*=>|\w+\s*=>)",
                ),
            ),
            (
                "interface",
                r(r"^\s*(?:export\s+)?interface\s+(?P<name>\w+)"),
            ),
            ("type", r(r"^\s*(?:export\s+)?type\s+(?P<name>\w+)")),
            (
                "enum",
                r(r"^\s*(?:export\s+)?(?:const\s+)?enum\s+(?P<name>\w+)"),
            ),
        ],
        "go" => vec![
            ("func", r(r"^\s*func\s+(?:\([^)]*\)\s*)?(?P<name>\w+)")),
            (
                "type",
                r(r"^\s*type\s+(?P<name>\w+)\s+(?:struct|interface)"),
            ),
        ],
        "rb" => vec![
            ("class", r(r"^\s*class\s+(?P<name>\w+)")),
            ("module", r(r"^\s*module\s+(?P<name>\w+)")),
            ("def", r(r"^\s*def\s+(?P<name>[\w.?!]+)")),
        ],
        "java" | "kt" | "scala" => vec![
            (
                "type",
                r(
                    r"^\s*(?:public|private|protected|abstract|final|static|sealed|open|data|\s)*\s*(?:class|interface|enum|object)\s+(?P<name>\w+)",
                ),
            ),
            (
                "method",
                r(
                    r"^\s*(?:public|private|protected|static|final|abstract|synchronized|override|fun|def|\s)+[\w<>\[\],.\s]+?\s+(?P<name>\w+)\s*\([^;{]*\)\s*\{?\s*$",
                ),
            ),
        ],
        "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" => vec![
            ("type", r(r"^\s*(?:class|struct)\s+(?P<name>\w+)")),
            (
                "fn",
                r(r"^\s*(?:[\w:<>\*&]+\s+)+(?P<name>\w+)\s*\([^;]*\)\s*\{?\s*$"),
            ),
        ],
        _ => vec![],
    }
}

#[async_trait]
impl Tool for CodeOutline {
    fn name(&self) -> &str {
        "code_outline"
    }
    fn icon(&self) -> &'static str {
        "⌗"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Search
    }
    fn description(&self) -> &str {
        "Extract a symbol map — functions, classes, structs, traits, methods, etc., \
         each with its line number — from a source file. A fast table of contents so \
         you can jump straight to a symbol with `fs.read` (offset/limit) instead of \
         reading the whole file. Supports Rust, Python, JS/TS, Go, Ruby, Java/Kotlin, \
         C/C++. Heuristic (line-based), so it's cheap but not a full parse."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Workspace-relative or absolute path to a source file" } },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let content = self
            .sbx
            .read(&path)
            .await
            .map_err(|e| ToolError::Failed(format!("read failed: {e}")))?;
        let rules = outline_rules(&path);
        if rules.is_empty() {
            return Ok(
                json!({ "path": path, "symbols": [], "count": 0, "note": "unsupported file type for outline" }),
            );
        }
        let mut symbols: Vec<Value> = Vec::new();
        for (i, line) in content.lines().enumerate() {
            for (kind, re) in &rules {
                if let Some(caps) = re.captures(line) {
                    if let Some(name) = caps.name("name") {
                        symbols.push(json!({ "kind": kind, "name": name.as_str(), "line": i + 1 }));
                        break; // one symbol per line
                    }
                }
            }
        }
        let count = symbols.len();
        Ok(json!({ "path": path, "symbols": symbols, "count": count }))
    }
}

struct References {
    sbx: Arc<WorkspaceSandbox>,
    /// Answers from the language server when one can serve this file. The text
    /// scan still runs first — it is what turns a bare symbol name into the
    /// position the server needs — but its matches are a fallback, not the
    /// answer, whenever the server can do better.
    lsp: LspHandle,
}

/// Upgrade a text hit to compiler-resolved references.
///
/// A language server needs a position, and the caller only has a name; the text
/// scan bridges that. Any occurrence works as the anchor — the server resolves
/// whatever symbol is at that position — so the first hit is enough.
async fn lsp_upgrade(
    manager: &lsp::LspManager,
    hits: &[Value],
    symbol: &str,
    max: usize,
) -> Option<Value> {
    let first = hits.first()?;
    let path = first.get("path")?.as_str()?;
    let line = first.get("line")?.as_u64()? as u32;
    // Recorded during the scan against the raw line: the column has to land
    // inside the identifier or the server resolves nothing.
    let character = first.get("col")?.as_u64()? as u32;
    let report = manager
        .references(
            path,
            lsp::Position {
                line: line.saturating_sub(1),
                character,
            },
            true,
        )
        .await;
    let lsp::QueryReport::Ready { server, items, .. } = report else {
        return None;
    };
    // An empty result is not an upgrade: the anchor may have been a comment or
    // a string, and reporting "0 references" would be worse than the text hits.
    if items.is_empty() {
        return None;
    }
    let references: Vec<Value> = items
        .iter()
        .take(max)
        .map(|location| {
            json!({
                "path": location.path,
                "line": location.range.start.line + 1,
            })
        })
        .collect();
    Some(json!({
        "symbol": symbol,
        "backend": server,
        "references": references,
        "count": items.len(),
        "truncated": items.len() > max,
    }))
}

#[async_trait]
impl Tool for References {
    fn name(&self) -> &str {
        "references"
    }
    fn icon(&self) -> &'static str {
        "↗"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Search
    }
    fn description(&self) -> &str {
        "Find every place a symbol (function/type/variable name) is used. Give it a name; \
         it works out the rest. When a language server can serve the file you get \
         compiler-resolved references — the actual symbol, not same-named ones elsewhere \
         or mentions in comments — and `backend` names the server. Otherwise it falls back to a \
         whole-word text scan ('run' won't match 'running') and `backend` says \"text\". \
         This is the tool for 'where is X used' and for finding call sites before you \
         change or rename something. Use `grep` for free-form regex or non-code text."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Identifier to locate (matched as a whole word)" },
                "path": { "type": "string", "description": "Workspace-relative dir to scope the search (default '.')" },
                "max_results": { "type": "integer", "description": "Cap on references returned (default 200)" }
            },
            "required": ["symbol"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let symbol = arg_str(args, "symbol")?;
        let rel_path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let max = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(200) as usize;

        // Deterministic whole-word match — the reason to use this over `grep`. We do
        // NOT classify definition-vs-use here: the caller sees the line text and can
        // judge that far more reliably than a language-specific keyword heuristic could.
        let esc = regex::escape(&symbol);
        let word = Regex::new(&format!(r"\b{esc}\b"))
            .map_err(|e| ToolError::Args(format!("bad symbol: {e}")))?;

        let start = self
            .sbx
            .resolve(rel_path)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let root = self.sbx.root().to_path_buf();
        let symbol_for_lsp = symbol.clone();

        // Full walk + reads are blocking work — off the async runtime.
        let scan = tokio::task::spawn_blocking(move || {
            let mut refs: Vec<Value> = Vec::new();
            let mut files = std::collections::HashSet::new();
            let mut truncated = false;
            let mut skipped_large = 0usize;
            let walk = WalkBuilder::new(&start).standard_filters(true).filter_entry(|e| !skip_dir(e)).build();
            'outer: for dent in walk.flatten() {
                if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                if dent.metadata().map(|m| m.len()).unwrap_or(0) > 1_000_000 {
                    skipped_large += 1;
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(dent.path()) else { continue };
                let rel = dent.path().strip_prefix(&root).unwrap_or(dent.path()).to_string_lossy().into_owned();
                for (i, line) in content.lines().enumerate() {
                    if let Some(found) = word.find(line) {
                        if refs.len() >= max {
                            truncated = true;
                            break 'outer;
                        }
                        files.insert(rel.clone());
                        refs.push(json!({
                            "path": rel,
                            "line": i + 1,
                            // The language server wants a 0-based UTF-16 offset
                            // into the raw line. Deriving it later from the
                            // trimmed, clipped `text` would be short by the
                            // indent and wrong again on any non-ASCII line.
                            "col": line[..found.start()].encode_utf16().count(),
                            "text": clip_marked(line.trim(), 200),
                        }));
                    }
                }
            }
            let count = refs.len();
            let file_count = files.len();
            let mut out = json!({ "symbol": symbol, "backend": "text", "references": refs, "count": count, "files": file_count, "truncated": truncated });
            if skipped_large > 0 {
                out["note"] = json!(format!(
                    "[skipped {skipped_large} file(s) >1MB — not searched; use shell.exec grep for those]"
                ));
            }
            Ok(out)
        })
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;

        let text_hits = scan?;
        // Prefer the server's answer. Text matching cannot tell this symbol from
        // an unrelated one of the same name, or from a mention in a comment; a
        // language server resolves the actual symbol. The caller asks the same
        // question either way and `backend` says which answered.
        let manager = self.lsp.lock().ok().and_then(|slot| slot.clone());
        if let Some(manager) = manager
            && let Some(hits) = text_hits.get("references").and_then(Value::as_array)
            && let Some(upgraded) = lsp_upgrade(&manager, hits, &symbol_for_lsp, max).await
        {
            return Ok(upgraded);
        }
        Ok(text_hits)
    }
}

struct Tree {
    sbx: Arc<WorkspaceSandbox>,
}

#[async_trait]
impl Tool for Tree {
    fn name(&self) -> &str {
        "tree"
    }
    fn icon(&self) -> &'static str {
        "├"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Search
    }
    fn description(&self) -> &str {
        "Show a directory as an indented, depth-limited tree (gitignore-aware, skips \
         .git/target/node_modules) — the fastest way to orient in an unfamiliar \
         project. Use `glob` to match files by pattern, `fs.list` for one directory."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative dir to root the tree (default '.')" },
                "depth": { "type": "integer", "description": "Max levels deep (default 2)" },
                "max_entries": { "type": "integer", "description": "Cap on entries listed (default 300)" }
            }
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let rel_path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let depth = args
            .get("depth")
            .and_then(Value::as_u64)
            .unwrap_or(2)
            .max(1) as usize;
        let max = args
            .get("max_entries")
            .and_then(Value::as_u64)
            .unwrap_or(300) as usize;
        let start = self
            .sbx
            .resolve(rel_path)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let rel_path = rel_path.to_string();

        // Directory walk is blocking work — off the async runtime.
        tokio::task::spawn_blocking(move || {
            let mut out = String::new();
            let mut count = 0usize;
            let mut truncated = false;
            let walk = WalkBuilder::new(&start)
                .max_depth(Some(depth))
                .standard_filters(true)
                .filter_entry(|e| !skip_dir(e))
                .sort_by_file_name(std::cmp::Ord::cmp)
                .build();
            for dent in walk.flatten() {
                let d = dent.depth();
                if d == 0 {
                    continue; // skip the root itself
                }
                if count >= max {
                    truncated = true;
                    break;
                }
                let is_dir = dent.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let name = dent.file_name().to_string_lossy();
                out.push_str(&"  ".repeat(d - 1));
                out.push_str(&name);
                if is_dir {
                    out.push('/');
                }
                out.push('\n');
                count += 1;
            }
            if truncated {
                out.push_str(&format!(
                    "[truncated: first {max} entries shown — raise max_entries or narrow path]\n"
                ));
            }
            Ok(json!({ "path": rel_path, "tree": out, "entries": count, "truncated": truncated }))
        })
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?
    }
}

// ── artifact recovery ────────────────────────────────────────────────────────

struct ReadArtifact {
    store: Arc<dyn kernel::ArtifactStore>,
}
#[async_trait]
impl Tool for ReadArtifact {
    fn name(&self) -> &str {
        "read_artifact"
    }
    fn icon(&self) -> &'static str {
        "⎘"
    }
    fn description(&self) -> &str {
        "Continue reading a large output that was spilled to the artifact store — \
         whenever a tool result shows a hash and says only the first N chars are \
         shown, use this to read the rest. Page through with `offset`/`length` \
         until you have what you need. Never tell the user an output was truncated \
         or that you can't see it: the full content is here, so fetch it."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "hash": { "type": "string", "description": "Artifact content hash" },
                "offset": { "type": "integer", "description": "Start byte (default 0)" },
                "length": { "type": "integer", "description": "Bytes to read (default: to end)" }
            },
            "required": ["hash"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let hash = arg_str(args, "hash")?;
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let length = args
            .get("length")
            .and_then(Value::as_u64)
            .map(|l| l as usize);
        let total = self.store.size(&hash).map_err(ToolError::Failed)?;
        let bytes = self
            .store
            .get(&hash, offset, length)
            .map_err(ToolError::Failed)?;
        // Snap page edges to char boundaries (P2): a byte offset can land
        // mid-UTF-8-sequence; lossy decoding put U+FFFD at the edges and made
        // stitched pages corrupt exact strings. Skip leading continuation
        // bytes, drop an incomplete trailing char, and report `next_offset`
        // so the dropped tail bytes re-appear at the start of the next page.
        let lead = if offset > 0 {
            bytes.iter().take_while(|b| (**b & 0xC0) == 0x80).count()
        } else {
            0
        };
        let slice = &bytes[lead.min(bytes.len())..];
        let (content, consumed) = match std::str::from_utf8(slice) {
            Ok(s) => (s.to_string(), slice.len()),
            Err(e) if e.error_len().is_none() && e.valid_up_to() > 0 => {
                // Clean text cut mid-char at the page end — keep the valid prefix.
                let v = e.valid_up_to();
                (String::from_utf8_lossy(&slice[..v]).into_owned(), v)
            }
            // Genuinely non-UTF-8 (binary artifact) — lossy as before.
            Err(_) => (String::from_utf8_lossy(slice).into_owned(), slice.len()),
        };
        let next_offset = offset + lead + consumed;
        let mut out = json!({
            "hash": hash,
            "offset": offset,
            "length": lead + consumed,
            "total_size": total,
            "content": content
        });
        if next_offset < total as usize {
            out["next_offset"] = json!(next_offset);
        }
        Ok(out)
    }
}

// ── web family (free: DuckDuckGo search, no key) ─────────────────────────────
//
// NOTE: web output is *untrusted* content (P7, `TrustLabel::Web`). The full
// trust-flow escalation (taint: a tool whose params derive from web content is
// escalated) lands with the governance layer; for now these are read-only.

/// A realistic desktop-browser User-Agent. Search engines (DuckDuckGo especially)
/// and many sites return empty/blocked responses to non-browser agents, so a
/// browser UA is the single biggest reliability win for both search and fetch.
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";

fn http_client() -> Result<reqwest::Client, ToolError> {
    reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| ToolError::Failed(e.to_string()))
}

/// True if `ip` is a non-public address that an agent-supplied fetch must never
/// reach — loopback, RFC1918 private, link-local (incl. the 169.254.169.254
/// cloud-metadata endpoint), CGNAT, unspecified/broadcast/multicast, and the
/// IPv6 equivalents (including IPv4-mapped forms). This is the SSRF blocklist.
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.octets()[0] == 0
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40) // 100.64.0.0/10 CGNAT
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_ip(IpAddr::V4(v4));
            }
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg0 & 0xffc0) == 0xfe80 // link-local  fe80::/10
                || (seg0 & 0xfe00) == 0xfc00 // unique-local fc00::/7
        }
    }
}

/// SSRF guard: require http/https and confirm the host does not resolve to any
/// non-public address. Called before the initial request AND re-checked on
/// every redirect hop (a redirect to `http://169.254.169.254/…` is the classic
/// bypass). DNS is resolved here so a hostname pointing at an internal IP is
/// caught; the per-hop re-check is the pragmatic defense against rebinding.
fn validate_public_url(url: &reqwest::Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "blocked URL scheme '{other}' (only http/https allowed)"
            ));
        }
    }
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // IP literal: check directly, no DNS.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_blocked_ip(ip) {
            Err(format!("blocked non-public address: {ip}"))
        } else {
            Ok(())
        };
    }

    // Hostname: reject if ANY resolved address is non-public.
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for '{host}': {e}"))?;
    let mut saw_any = false;
    for addr in addrs {
        saw_any = true;
        if is_blocked_ip(addr.ip()) {
            return Err(format!(
                "blocked non-public address {} for host '{host}'",
                addr.ip()
            ));
        }
    }
    if !saw_any {
        return Err(format!("no addresses resolved for host '{host}'"));
    }
    Ok(())
}

/// Read a response body into memory, but abort as soon as it exceeds `max`
/// bytes — streaming rather than buffering the whole (possibly unbounded,
/// chunked) body first. A declared oversized `Content-Length` is rejected up
/// front. This is the cap the PDF/HTML size limits used to apply only *after*
/// the entire body was already in memory.
async fn read_body_capped(resp: reqwest::Response, max: usize) -> Result<Vec<u8>, ToolError> {
    use futures::StreamExt;
    if let Some(len) = resp.content_length() {
        if len as usize > max {
            return Err(ToolError::Failed(format!(
                "response too large: Content-Length {len} exceeds {max}-byte cap"
            )));
        }
    }
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ToolError::Failed(e.to_string()))?;
        if buf.len() + chunk.len() > max {
            return Err(ToolError::Failed(format!(
                "response exceeded {max}-byte cap"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Like [`http_client`], but for agent-supplied `web.fetch` targets: redirects
/// are NOT auto-followed — `fetch_plain` follows them manually so each hop's
/// target passes [`validate_public_url`] with DNS resolved off the async
/// workers (a redirect policy closure is sync, which forced blocking DNS onto
/// the runtime, P2). A public URL still can't 30x-bounce into the internal net.
fn fetch_client() -> Result<reqwest::Client, ToolError> {
    reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ToolError::Failed(e.to_string()))
}

/// Validate a URL as public with DNS resolution on the blocking pool.
async fn validate_public_url_async(url: &reqwest::Url) -> Result<(), ToolError> {
    let u = url.clone();
    tokio::task::spawn_blocking(move || validate_public_url(&u))
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?
        .map_err(ToolError::Failed)
}

/// Which backend `web.search` uses. `DuckDuckGo` needs no key and is always the
/// fallback; the others need a key (Tavily/Brave) or an instance URL (SearXNG)
/// supplied via `/search` in the TUI (or the matching env var).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchProvider {
    #[default]
    DuckDuckGo,
    Tavily,
    Brave,
    Searxng,
}

impl SearchProvider {
    /// Stable lowercase id used in config.toml and credential keys.
    pub fn as_str(self) -> &'static str {
        match self {
            SearchProvider::DuckDuckGo => "duckduckgo",
            SearchProvider::Tavily => "tavily",
            SearchProvider::Brave => "brave",
            SearchProvider::Searxng => "searxng",
        }
    }
    /// Parse a config/id string; anything unrecognized → DuckDuckGo (safe default).
    pub fn from_id(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "tavily" => SearchProvider::Tavily,
            "brave" => SearchProvider::Brave,
            "searxng" => SearchProvider::Searxng,
            _ => SearchProvider::DuckDuckGo,
        }
    }
    /// Human-facing label for TUI notices/pickers.
    pub fn label(self) -> &'static str {
        match self {
            SearchProvider::DuckDuckGo => "DuckDuckGo",
            SearchProvider::Tavily => "Tavily",
            SearchProvider::Brave => "Brave",
            SearchProvider::Searxng => "SearXNG",
        }
    }
}

/// User-chosen web-search configuration, shared live between the TUI (writer)
/// and the `web.*` tools (readers) via [`SearchHandle`]. Any `None` secret
/// falls back to the matching env var, so pre-existing env-based setups keep
/// working untouched.
#[derive(Debug, Clone, Default)]
pub struct SearchSettings {
    pub provider: SearchProvider,
    pub tavily_key: Option<String>,
    pub brave_key: Option<String>,
    pub searxng_url: Option<String>,
}

/// Shared, mutable search settings. Cloned into each web tool and into the TUI
/// so `/search` takes effect mid-session without a restart.
pub type SearchHandle = Arc<Mutex<SearchSettings>>;

impl SearchSettings {
    /// Tavily key — explicit value first, else `TAVILY_API_KEY`.
    fn tavily_key(&self) -> Option<String> {
        resolve_secret(self.tavily_key.as_deref(), "TAVILY_API_KEY")
    }
    /// Brave key — explicit value first, else `BRAVE_API_KEY`.
    fn brave_key(&self) -> Option<String> {
        resolve_secret(self.brave_key.as_deref(), "BRAVE_API_KEY")
    }
    /// SearXNG base URL — explicit value first, else `MEDHA_SEARXNG_URL`.
    fn searxng_url(&self) -> Option<String> {
        resolve_secret(self.searxng_url.as_deref(), "MEDHA_SEARXNG_URL")
    }
}

/// Prefer an explicit non-empty value; otherwise fall back to a non-empty env var.
fn resolve_secret(explicit: Option<&str>, env: &str) -> Option<String> {
    if let Some(v) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(v.to_string());
    }
    std::env::var(env)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read a clone of the current search settings from a handle, tolerating a
/// poisoned lock (a tool must never panic just because some other holder did).
fn read_search(handle: &SearchHandle) -> SearchSettings {
    handle
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|p| p.into_inner().clone())
}

struct WebSearch {
    search: SearchHandle,
}
#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web.search"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Web
    }
    fn description(&self) -> &str {
        "Search the web. Returns {title, url, snippet}. Uses the provider the \
         user configured via /search (Tavily, Brave, or SearXNG), falling back \
         to DuckDuckGo when none is set or the chosen one fails. Follow up with \
         web.fetch on a result url."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "max_results": { "type": "integer", "description": "Max results (default 8)" }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let query = arg_str(args, "query")?;
        let max = args.get("max_results").and_then(Value::as_u64).unwrap_or(8) as usize;
        let client = http_client()?;
        let cfg = read_search(&self.search);

        // Sticky selection: use ONLY the provider the user chose (via /search),
        // then DuckDuckGo. A chosen provider that lacks a key/URL or errors —
        // invalid/expired key, network failure — falls through to DuckDuckGo so
        // one misconfigured backend never leaves search dead. DuckDuckGo itself
        // needs no key and is the default when nothing is configured.
        let mut errors = Vec::new();
        let chosen: Option<(&'static str, Result<Value, ToolError>)> = match cfg.provider {
            SearchProvider::DuckDuckGo => None,
            SearchProvider::Tavily => match cfg.tavily_key() {
                Some(key) => Some(("tavily", tavily_search(&client, &query, max, &key).await)),
                None => {
                    errors.push("tavily: no API key configured".to_string());
                    None
                }
            },
            SearchProvider::Brave => match cfg.brave_key() {
                Some(key) => Some(("brave", brave_search(&client, &query, max, &key).await)),
                None => {
                    errors.push("brave: no API key configured".to_string());
                    None
                }
            },
            SearchProvider::Searxng => match cfg.searxng_url() {
                Some(url) => Some(("searxng", searxng_search(&client, &query, max, &url).await)),
                None => {
                    errors.push("searxng: no instance URL configured".to_string());
                    None
                }
            },
        };
        if let Some((name, result)) = chosen {
            match result {
                Ok(v) => return Ok(v),
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }

        match duckduckgo_search(&client, &query, max).await {
            Ok(v) => Ok(v),
            Err(e) => {
                errors.push(format!("duckduckgo: {e}"));
                // Chosen backend (if any) AND the DuckDuckGo fallback both
                // failed: surface every attempt so the cause is visible.
                Err(ToolError::Failed(errors.join("; ")))
            }
        }
    }
}

/// Parses a DuckDuckGo results page (`html` string, `max` cap) into result values.
type DdgParser = fn(&str, usize) -> Vec<Value>;

async fn duckduckgo_search(
    client: &reqwest::Client,
    query: &str,
    max: usize,
) -> Result<Value, ToolError> {
    // Try the scraper-friendly `lite` endpoint first, then the heavier `html` one.
    // `lite` returns simple table markup and is far less aggressively bot-blocked.
    let endpoints: [(&str, DdgParser); 2] = [
        ("https://lite.duckduckgo.com/lite/", parse_ddg_lite),
        ("https://html.duckduckgo.com/html/", parse_ddg),
    ];
    let mut last_err = String::from("no results parsed (DuckDuckGo may be anti-botting)");
    for (url, parse) in endpoints {
        let resp = client
            .post(url)
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header(reqwest::header::REFERER, "https://duckduckgo.com/")
            .form(&[("q", query)])
            .send()
            .await;
        let html = match resp {
            Ok(r) => match r.text().await {
                Ok(t) => t,
                Err(e) => {
                    last_err = e.to_string();
                    continue;
                }
            },
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };
        let results = parse(&html, max); // sync: scraper DOM never crosses an await
        if !results.is_empty() {
            let count = results.len();
            return Ok(
                json!({ "query": query, "results": results, "count": count, "backend": "duckduckgo" }),
            );
        }
    }
    Err(ToolError::Failed(last_err))
}

/// Brave Search API (free tier; reliable). Header auth, JSON results.
/// Tavily Search (https://api.tavily.com/search) — a search API built for LLM
/// agents: clean, ranked results with an NLP summary per source in `content`.
/// Bearer-authenticated; no scraping, so it's the most reliable backend.
async fn tavily_search(
    client: &reqwest::Client,
    query: &str,
    max: usize,
    key: &str,
) -> Result<Value, ToolError> {
    let body = json!({
        "query": query,
        "search_depth": "basic",
        "topic": "general",
        "max_results": max.clamp(1, 20),
    });
    let resp = client
        .post("https://api.tavily.com/search")
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let brief: String = body.chars().take(200).collect();
        return Err(ToolError::Failed(format!("tavily {status}: {brief}")));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("results").and_then(Value::as_array) {
        for r in arr.iter().take(max) {
            out.push(json!({
                "title": r.get("title").and_then(Value::as_str).unwrap_or(""),
                "url": r.get("url").and_then(Value::as_str).unwrap_or(""),
                "snippet": r.get("content").and_then(Value::as_str).unwrap_or(""),
            }));
        }
    }
    if out.is_empty() {
        return Err(ToolError::Failed("tavily: no results".into()));
    }
    let count = out.len();
    Ok(json!({ "query": query, "results": out, "count": count, "backend": "tavily" }))
}

/// Tavily Extract (https://api.tavily.com/extract) — LLM-optimized page reader.
/// Handles JS-heavy/anti-bot pages that a plain fetch can't; returns markdown.
async fn tavily_extract(
    client: &reqwest::Client,
    url: &str,
    key: &str,
) -> Result<Value, ToolError> {
    let body = json!({ "urls": url, "extract_depth": "basic", "format": "markdown" });
    let resp = client
        .post("https://api.tavily.com/extract")
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let brief: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        return Err(ToolError::Failed(format!(
            "tavily extract {status}: {brief}"
        )));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    let content = v
        .get("results")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|r| r.get("raw_content"))
        .and_then(Value::as_str);
    match content {
        Some(c) if !c.trim().is_empty() => {
            Ok(json!({ "url": url, "status": 200, "title": "", "content": c, "backend": "tavily" }))
        }
        _ => Err(ToolError::Failed(
            "tavily extract: no content returned".into(),
        )),
    }
}

/// Tavily Crawl (https://api.tavily.com/crawl) — graph-based multi-page traversal
/// from a root URL, with optional natural-language `instructions` to focus it.
async fn tavily_crawl(
    client: &reqwest::Client,
    url: &str,
    instructions: Option<&str>,
    max_depth: u64,
    limit: u64,
    key: &str,
) -> Result<Value, ToolError> {
    let mut body = json!({
        "url": url,
        "max_depth": max_depth.clamp(1, 5),
        "limit": limit.clamp(1, 100),
        "extract_depth": "basic",
        "format": "markdown",
    });
    if let Some(i) = instructions {
        if !i.trim().is_empty() {
            body["instructions"] = json!(i.trim());
        }
    }
    let resp = client
        .post("https://api.tavily.com/crawl")
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let brief: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        return Err(ToolError::Failed(format!("tavily crawl {status}: {brief}")));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    let base = v
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or(url)
        .to_string();
    let mut pages = Vec::new();
    if let Some(arr) = v.get("results").and_then(Value::as_array) {
        for r in arr {
            let purl = r.get("url").and_then(Value::as_str).unwrap_or("");
            // Cap per-page content so a big crawl can't blow the model's context.
            let content: String = r
                .get("raw_content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(4000)
                .collect();
            if !purl.is_empty() {
                pages.push(json!({ "url": purl, "content": content }));
            }
        }
    }
    if pages.is_empty() {
        return Err(ToolError::Failed(
            "no pages returned — the site is likely JavaScript-rendered or has few \
             crawlable links; use web.fetch on specific page URLs instead"
                .into(),
        ));
    }
    let count = pages.len();
    Ok(json!({ "base_url": base, "pages": pages, "count": count, "backend": "tavily" }))
}

async fn brave_search(
    client: &reqwest::Client,
    query: &str,
    max: usize,
    key: &str,
) -> Result<Value, ToolError> {
    let count = max.to_string();
    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .query(&[("q", query), ("count", count.as_str())])
        .send()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Failed(format!("brave search {status}: {body}")));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    let mut out = Vec::new();
    if let Some(arr) = v.pointer("/web/results").and_then(Value::as_array) {
        for r in arr.iter().take(max) {
            out.push(json!({
                "title": r.get("title").and_then(Value::as_str).unwrap_or(""),
                "url": r.get("url").and_then(Value::as_str).unwrap_or(""),
                "snippet": r.get("description").and_then(Value::as_str).unwrap_or(""),
            }));
        }
    }
    let count = out.len();
    Ok(json!({ "query": query, "results": out, "count": count, "backend": "brave" }))
}

/// Self-hosted SearXNG JSON API (free, no key) — the user points MEDHA_SEARXNG_URL
/// at an instance they trust.
async fn searxng_search(
    client: &reqwest::Client,
    query: &str,
    max: usize,
    base: &str,
) -> Result<Value, ToolError> {
    let url = format!("{}/search", base.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .query(&[("q", query), ("format", "json")])
        .send()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(ToolError::Failed(format!("searxng {}", resp.status())));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("results").and_then(Value::as_array) {
        for r in arr.iter().take(max) {
            out.push(json!({
                "title": r.get("title").and_then(Value::as_str).unwrap_or(""),
                "url": r.get("url").and_then(Value::as_str).unwrap_or(""),
                "snippet": r.get("content").and_then(Value::as_str).unwrap_or(""),
            }));
        }
    }
    let count = out.len();
    Ok(json!({ "query": query, "results": out, "count": count, "backend": "searxng" }))
}

/// Parse the `lite.duckduckgo.com/lite/` results table: result links carry
/// `class="result-link"`, snippets are in `td.result-snippet` (in document order).
fn parse_ddg_lite(html: &str, max: usize) -> Vec<Value> {
    let doc = Html::parse_document(html);
    let row_sel = Selector::parse("tr").unwrap();
    let link_sel = Selector::parse("a.result-link").unwrap();
    let snip_sel = Selector::parse("td.result-snippet, .result-snippet").unwrap();
    // Pair structurally, not by index (P2): a snippet belongs to the link row
    // immediately before it. Index-zipping the two node lists meant one ad or
    // snippet-less row shifted EVERY later snippet onto the wrong result.
    let mut out: Vec<Value> = Vec::new();
    let mut pending: Option<(String, String)> = None;
    let flush = |pending: &mut Option<(String, String)>, snippet: String, out: &mut Vec<Value>| {
        if let Some((title, url)) = pending.take() {
            out.push(json!({ "title": title, "url": url, "snippet": snippet }));
        }
    };
    for row in doc.select(&row_sel) {
        if out.len() >= max {
            return out;
        }
        if let Some(a) = row.select(&link_sel).next() {
            // New result row: emit any prior result that never got a snippet.
            flush(&mut pending, String::new(), &mut out);
            let title = a.text().collect::<String>().trim().to_string();
            let url = decode_ddg_url(a.value().attr("href").unwrap_or_default());
            if !title.is_empty() && !url.is_empty() {
                pending = Some((title, url));
            }
        } else if let Some(s) = row.select(&snip_sel).next() {
            let snippet = s.text().collect::<String>().trim().to_string();
            flush(&mut pending, snippet, &mut out);
        }
    }
    if out.len() < max {
        flush(&mut pending, String::new(), &mut out);
    }
    out
}

fn parse_ddg(html: &str, max: usize) -> Vec<Value> {
    let doc = Html::parse_document(html);
    // DuckDuckGo static HTML uses #links .result as container (any tag with class=result)
    let res_sel = Selector::parse("#links .result").unwrap();
    let title_sel = Selector::parse("a.result__a").unwrap();
    let snip_sel = Selector::parse(".result__snippet").unwrap();

    // Primary: structured result containers (title + snippet).
    let mut out = Vec::new();
    for el in doc.select(&res_sel) {
        if out.len() >= max {
            break;
        }
        let Some(a) = el.select(&title_sel).next() else {
            continue;
        };
        let title = a.text().collect::<String>().trim().to_string();
        let url = decode_ddg_url(a.value().attr("href").unwrap_or_default());
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let snippet = el
            .select(&snip_sel)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        out.push(json!({ "title": title, "url": url, "snippet": snippet }));
    }
    if !out.is_empty() {
        return out;
    }

    // Fallback: any anchor that is a DuckDuckGo result redirect (layout-robust).
    let any = Selector::parse("a").unwrap();
    for a in doc.select(&any) {
        if out.len() >= max {
            break;
        }
        let href = a.value().attr("href").unwrap_or_default();
        if !href.contains("uddg=") {
            continue;
        }
        let url = decode_ddg_url(href);
        let title = a.text().collect::<String>().trim().to_string();
        if !url.is_empty() && !title.is_empty() {
            out.push(json!({ "title": title, "url": url, "snippet": "" }));
        }
    }
    out
}

/// DuckDuckGo wraps result links as `//duckduckgo.com/l/?uddg=<encoded-url>`.
fn decode_ddg_url(href: &str) -> String {
    if let Some(i) = href.find("uddg=") {
        let enc = href[i + 5..].split('&').next().unwrap_or("");
        return urlencoding::decode(enc)
            .map(|c| c.into_owned())
            .unwrap_or_default();
    }
    if href.starts_with("http") {
        href.to_string()
    } else if href.starts_with("//") {
        format!("https:{href}")
    } else {
        String::new()
    }
}

struct WebFetch {
    search: SearchHandle,
}
#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web.fetch"
    }
    fn icon(&self) -> &'static str {
        "↓"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Web
    }
    fn description(&self) -> &str {
        "Fetch a web page and return its readable content as Markdown (free plain \
         fetch; falls back to Tavily Extract if TAVILY_API_KEY is set and the page \
         blocks scrapers or errors). For crawling a whole site, use web.crawl."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "url": { "type": "string", "description": "Absolute URL to fetch" } },
            "required": ["url"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let url = arg_str(args, "url")?;
        match fetch_plain(&url).await {
            Ok(v) => Ok(v),
            Err(e) => {
                // Plain fetch failed or the page blocked us — fall back to Tavily's
                // LLM-optimized extractor when a key is configured (via /search or env).
                if let Some(key) = read_search(&self.search).tavily_key() {
                    return tavily_extract(&http_client()?, &url, &key)
                        .await
                        .map_err(|te| {
                            ToolError::Failed(format!("plain fetch failed ({e}); {te}"))
                        });
                }
                Err(e)
            }
        }
    }
}

/// Free plain HTTP fetch → Markdown. Returns Err on network failure or a non-2xx
/// status (so the caller can fall back to Tavily Extract on blocked/erroring pages).
async fn fetch_plain(url: &str) -> Result<Value, ToolError> {
    let parsed =
        reqwest::Url::parse(url).map_err(|e| ToolError::Failed(format!("invalid URL: {e}")))?;

    // SSRF guard: reject internal/non-public targets before connecting. DNS is
    // blocking, so resolve off the async workers.
    validate_public_url_async(&parsed).await?;

    // Follow redirects manually (max 5) so EVERY hop is validated the same way
    // — the classic bypass is a public URL that 30x-redirects to
    // http://169.254.169.254/… — with per-hop DNS off the async workers (P2).
    let client = fetch_client()?;
    let mut current = parsed;
    let mut hops = 0u8;
    let resp = loop {
        let r = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        if !r.status().is_redirection() {
            break r;
        }
        hops += 1;
        if hops > 5 {
            return Err(ToolError::Failed("too many redirects".into()));
        }
        let loc = r
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ToolError::Failed("redirect without a Location header".into()))?;
        let next = current
            .join(loc)
            .map_err(|e| ToolError::Failed(format!("bad redirect target: {e}")))?;
        validate_public_url_async(&next)
            .await
            .map_err(|e| ToolError::Failed(format!("blocked redirect: {e}")))?;
        current = next;
    };
    let status = resp.status();
    let code = status.as_u16();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    // PDFs need the raw bytes (decoding to text would corrupt them), so
    // branch before `.text()`. arXiv & friends serve `application/pdf`; also
    // catch a `.pdf` URL in case the type header is generic.
    if status.is_success()
        && (ctype.contains("pdf") || url.split('?').next().unwrap_or(url).ends_with(".pdf"))
    {
        let bytes = read_body_capped(resp, 25 * 1024 * 1024).await?;
        // Panic-isolated on the blocking pool: a malformed PDF that makes the
        // extractor panic degrades to a message, it never takes down the run.
        let text = tokio::task::spawn_blocking(move || extract_pdf_text(&bytes))
            .await
            .unwrap_or_else(|_| "[web.fetch: PDF extraction failed]".to_string());
        return Ok(json!({ "url": url, "status": code, "title": "", "content": text }));
    }

    // A blocked/erroring page (403/404/5xx) → error so we can try Tavily Extract.
    if !status.is_success() {
        return Err(ToolError::Failed(format!("HTTP {code}")));
    }

    let raw = read_body_capped(resp, 16 * 1024 * 1024).await?;
    let body = String::from_utf8_lossy(&raw).into_owned();
    let title = extract_title(&body);

    // Only HTML/text is converted. Binary content (PDF, images, …) must NOT
    // be fed to the recursive HTML parser: on non-HTML bytes it can recurse
    // until the worker stack overflows, which aborts the whole process
    // (uncatchable). Report the type plainly instead.
    let texty = ctype.contains("html")
        || ctype.contains("xml")
        || ctype.starts_with("text/")
        || (ctype.is_empty() && body.trim_start().starts_with('<'));
    if !texty {
        let kind = if ctype.is_empty() {
            "binary content".to_string()
        } else {
            ctype
        };
        return Ok(json!({
            "url": url, "status": code, "title": title,
            "content": format!(
                "[web.fetch: {kind} ({} bytes) is not an HTML/text page — cannot convert to Markdown]",
                body.len()
            )
        }));
    }

    // Conversion is CPU-bound and recursive → off the async workers and onto
    // a large-stack thread (inside `html_to_markdown`) so it can't abort us.
    let markdown = tokio::task::spawn_blocking(move || html_to_markdown(&body))
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    Ok(json!({ "url": url, "status": code, "title": title, "content": markdown }))
}

struct WebCrawl {
    search: SearchHandle,
}
#[async_trait]
impl Tool for WebCrawl {
    fn name(&self) -> &str {
        "web.crawl"
    }
    fn icon(&self) -> &'static str {
        "⇊"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Web
    }
    fn description(&self) -> &str {
        "Fetch content from MANY pages under ONE site/root in a single call (Tavily; \
         requires a Tavily key configured via /search or TAVILY_API_KEY). Use this — \
         instead of calling web.fetch page by \
         page — when the task needs multiple pages of the SAME site, e.g. 'read all \
         the docs under this URL', 'every posting on this careers page', 'summarize \
         this whole section'. Give natural-language `instructions` to focus it (e.g. \
         'pages about pricing'). NOT for: a single known page (use web.fetch) or \
         finding pages across different sites (use web.search)."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn timeout(&self) -> Option<std::time::Duration> {
        // A multi-page crawl (`limit` up to 100) is one long request and can't
        // finish inside 60s; give it a wider ceiling.
        Some(std::time::Duration::from_secs(300)) // 5 min
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Root URL to start crawling" },
                "instructions": { "type": "string", "description": "Optional: what to look for, in natural language" },
                "max_depth": { "type": "integer", "description": "How far from the root to follow links (1-5, default 1)" },
                "limit": { "type": "integer", "description": "Max pages to process (default 20)" }
            },
            "required": ["url"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let url = arg_str(args, "url")?;
        let instructions = args.get("instructions").and_then(Value::as_str);
        let max_depth = args.get("max_depth").and_then(Value::as_u64).unwrap_or(1);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20);
        let key = read_search(&self.search).tavily_key().ok_or_else(|| {
            ToolError::Failed(
                "web.crawl needs a Tavily API key — set one with /search (choose Tavily) \
                 or TAVILY_API_KEY"
                    .into(),
            )
        })?;
        tavily_crawl(&http_client()?, &url, instructions, max_depth, limit, &key).await
    }
}

// ── clarify (human-in-the-loop questions) ────────────────────────────────────

struct Clarify {
    asker: ClarifyHandle,
}

impl Clarify {
    /// Parse + validate the `questions` argument into kernel `Question`s.
    /// Bounds mirror a structured-question UI (AskUserQuestion-style): 1–4
    /// questions, 2–5 options each — enough to be useful, few enough to render.
    fn parse_questions(args: &Value) -> Result<Vec<kernel::Question>, ToolError> {
        // Some models double-encode nested JSON, sending `questions` as a string
        // that contains the array. Accept either the array or a JSON-string of it.
        let raw: Vec<Value> = match args.get("questions") {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                .ok()
                .and_then(|v| match v {
                    Value::Array(a) => Some(a),
                    _ => None,
                })
                .ok_or_else(|| ToolError::Args("expected an array 'questions'".into()))?,
            _ => return Err(ToolError::Args("expected an array 'questions'".into())),
        };
        if raw.is_empty() || raw.len() > 4 {
            return Err(ToolError::Args("provide 1–4 questions".into()));
        }
        let mut out = Vec::with_capacity(raw.len());
        for q in &raw {
            let prompt = q
                .get("question")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    ToolError::Args("each question needs a non-empty 'question'".into())
                })?
                .to_string();
            let header = q
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let multi_select = q
                .get("multi_select")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let opts = q
                .get("options")
                .and_then(Value::as_array)
                .ok_or_else(|| ToolError::Args("each question needs an 'options' array".into()))?;
            if opts.len() < 2 {
                return Err(ToolError::Args(
                    "each question needs at least 2 options".into(),
                ));
            }
            // Models sometimes overshoot the 5-option UI cap; keep the first 5
            // rather than failing the whole call.
            let opts = &opts[..opts.len().min(5)];
            let options = opts
                .iter()
                .map(|o| {
                    let label = o
                        .get("label")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .ok_or_else(|| ToolError::Args("each option needs a 'label'".into()))?
                        .to_string();
                    Ok(kernel::QOption {
                        label,
                        description: o
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        recommended: o
                            .get("recommended")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect::<Result<Vec<_>, ToolError>>()?;
            out.push(kernel::Question {
                prompt,
                header,
                options,
                multi_select,
            });
        }
        Ok(out)
    }
}

#[async_trait]
impl Tool for Clarify {
    fn name(&self) -> &str {
        "clarify"
    }
    fn icon(&self) -> &'static str {
        "?"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Plan
    }
    fn description(&self) -> &str {
        "Ask the user one or more multiple-choice questions BEFORE proceeding, when \
         the task is materially ambiguous (missing target, several valid \
         interpretations, a consequential fork). HARD LIMITS: 1–4 questions, and \
         each question MUST have between 2 and 5 options — never more than 5 (pick \
         the 5 most useful and let the user type their own for the rest). Mark one \
         option `recommended`; set `multi_select` for checkboxes. The user can also \
         type their own answer. Returns their choices. Don't use it for trivial \
         decisions you can make yourself — only when guessing wrong would waste \
         real work."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn timeout(&self) -> Option<std::time::Duration> {
        // A human question has NO deadline — the agent must wait for the user, not
        // give up after 60s and proceed on its own. The user can always Esc to
        // dismiss the form (→ skipped) or cancel the turn.
        None
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "1 to 4 questions (maximum 4).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string", "description": "The full question" },
                            "header": { "type": "string", "description": "Short label/chip, e.g. 'Auth method'" },
                            "multi_select": { "type": "boolean", "description": "true = checkboxes (any number); false = pick one" },
                            "options": {
                                "type": "array",
                                "description": "Between 2 and 5 options — never more than 5.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string" },
                                        "description": { "type": "string" },
                                        "recommended": { "type": "boolean" }
                                    },
                                    "required": ["label"]
                                }
                            }
                        },
                        "required": ["question", "options"]
                    }
                }
            },
            "required": ["questions"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let questions = Self::parse_questions(args)?;
        let prompts: Vec<String> = questions.iter().map(|q| q.prompt.clone()).collect();

        let asker = self.asker.lock().ok().and_then(|g| g.clone());
        let Some(asker) = asker else {
            // No interactive surface (headless/CI). Never block — tell the model
            // to proceed on its own best judgment.
            return Ok(json!({
                "skipped": true,
                "note": "no interactive user is available — proceed using your best judgment"
            }));
        };

        match asker.ask(questions).await {
            Some(answers) => {
                let out: Vec<Value> = prompts
                    .iter()
                    .zip(answers.iter())
                    .map(
                        |(q, a)| json!({ "question": q, "selected": a.selected, "other": a.other }),
                    )
                    .collect();
                Ok(json!({ "answers": out }))
            }
            None => Ok(json!({
                "skipped": true,
                "note": "the user dismissed the question — proceed using your best judgment"
            })),
        }
    }
}

fn extract_title(html: &str) -> String {
    Regex::new(r"(?is)<title[^>]*>(.*?)</title>")
        .ok()
        .and_then(|re| re.captures(html))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .unwrap_or_default()
}

/// Strip script/style noise, then convert to Markdown (free, no headless
/// browser). `html2md` is a recursive descent over the DOM, so two guards keep
/// it from overflowing the stack and aborting the process: the input is capped,
/// and the parse runs on a thread with a large stack (deeply nested but valid
/// HTML would otherwise blow the default 2 MB worker stack).
fn html_to_markdown(html: &str) -> String {
    const MAX_INPUT: usize = 4 * 1024 * 1024;
    let truncated = if html.len() > MAX_INPUT {
        let mut end = MAX_INPUT;
        while !html.is_char_boundary(end) {
            end -= 1;
        }
        &html[..end]
    } else {
        html
    };
    let no_script = Regex::new(r"(?is)<script.*?</script>")
        .unwrap()
        .replace_all(truncated, "");
    let cleaned = Regex::new(r"(?is)<style.*?</style>")
        .unwrap()
        .replace_all(&no_script, "")
        .into_owned();

    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || html2md::parse_html(&cleaned))
        .ok()
        .and_then(|h| h.join().ok())
        .unwrap_or_else(|| "[web.fetch: page too complex to convert to Markdown]".to_string())
}

/// Extract the text of a PDF (e.g. an arXiv paper) so `web.fetch` returns
/// something useful instead of refusing binary. Size-capped; empty output means
/// a scanned/image PDF with no embedded text layer (we don't OCR).
fn extract_pdf_text(bytes: &[u8]) -> String {
    const MAX_PDF: usize = 25 * 1024 * 1024;
    const MAX_OUT: usize = 400 * 1024;
    if bytes.len() > MAX_PDF {
        return format!(
            "[web.fetch: PDF too large to extract ({} bytes)]",
            bytes.len()
        );
    }
    match pdf_extract::extract_text_from_mem(bytes) {
        Ok(text) => {
            let text = text.trim();
            if text.is_empty() {
                "[web.fetch: PDF has no extractable text layer (likely scanned images)]".to_string()
            } else if text.len() > MAX_OUT {
                let mut end = MAX_OUT;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                format!(
                    "{}\n\n[…truncated; {} total chars]",
                    &text[..end],
                    text.len()
                )
            } else {
                text.to_string()
            }
        }
        Err(e) => format!("[web.fetch: could not extract PDF text: {e}]"),
    }
}

// ── shell family ─────────────────────────────────────────────────────────────

/// The environment handed to `shell.exec` children: the current process env
/// filtered down to a non-secret allowlist. Everything else (provider/API keys
/// like TAVILY_API_KEY/BRAVE_API_KEY and any other injected secrets) is dropped,
/// so an arbitrary shell command can't read them out of the environment.
fn shell_env() -> Vec<(String, String)> {
    const ALLOW: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "PWD",
        "OLDPWD",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LANGUAGE",
        "TERM",
        "TERMINFO",
        "TZ",
        "COLUMNS",
        "LINES",
        // Toolchain locators (paths, not secrets) so builds/tests still work.
        "CARGO_HOME",
        "RUSTUP_HOME",
        "GOPATH",
        "GOROOT",
        "GOCACHE",
        "JAVA_HOME",
        "NODE_PATH",
        "NVM_DIR",
        "PYENV_ROOT",
        "VIRTUAL_ENV",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
    ];
    std::env::vars()
        .filter(|(k, _)| ALLOW.contains(&k.as_str()) || k.starts_with("LC_"))
        .collect()
}

/// True if `program` resolves on the current PATH (or is an existing absolute
/// path). Used to report "not installed" before invoking a checker, since under
/// a sandbox wrapper a missing target no longer surfaces as a spawn error.
fn program_on_path(program: &str) -> bool {
    let p = std::path::Path::new(program);
    if p.is_absolute() {
        return p.exists();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).exists()))
        .unwrap_or(false)
}

struct ShellExec {
    sbx: Arc<WorkspaceSandbox>,
    tasks: Arc<TaskTable>,
}

/// Seconds a `shell.exec` runs in the foreground before it's promoted to a
/// background task (returns partial output + a task id, keeps running). Chosen
/// below the default tool ceiling so promotion — not a kill — is what happens to
/// a slow command.
const PROMOTE_AFTER_SECS: u64 = 50;

/// Shared registry of promoted background commands (§2). One per session,
/// created in [`ToolRegistry::with_workspace`] and shared by `shell.exec` and
/// the `task.*` tools.
#[derive(Default)]
struct TaskTable {
    inner: std::sync::Mutex<HashMap<String, TaskEntry>>,
    seq: std::sync::atomic::AtomicU64,
}

struct TaskEntry {
    proc: sandbox::exec::BgProc,
    command: String,
}

impl TaskTable {
    fn register(&self, command: String, proc: sandbox::exec::BgProc) -> String {
        let n = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let id = format!("t{n}");
        if let Ok(mut m) = self.inner.lock() {
            m.insert(id.clone(), TaskEntry { proc, command });
        }
        id
    }
    /// A JSON snapshot of one task (status + current output), or `None` if the id
    /// is unknown.
    fn snapshot(&self, id: &str) -> Option<Value> {
        let m = self.inner.lock().ok()?;
        let t = m.get(id)?;
        let (stdout, stderr) = t.proc.snapshot();
        let running = t.proc.is_running();
        Some(json!({
            "task_id": id,
            "command": t.command,
            "status": if running { "running" } else { "exited" },
            "exit_code": t.proc.exit_code(),
            "stdout": stdout,
            "stderr": stderr,
        }))
    }
    /// Kill a task's process group; returns false if the id is unknown.
    fn kill(&self, id: &str) -> bool {
        let Ok(m) = self.inner.lock() else {
            return false;
        };
        match m.get(id) {
            Some(t) => {
                t.proc.kill();
                true
            }
            None => false,
        }
    }
    /// One-line summaries of every task (for `task.list` / status).
    fn list(&self) -> Vec<Value> {
        let Ok(m) = self.inner.lock() else {
            return Vec::new();
        };
        let mut out: Vec<Value> = m
            .iter()
            .map(|(id, t)| {
                json!({
                    "task_id": id,
                    "command": t.command,
                    "status": if t.proc.is_running() { "running" } else { "exited" },
                    "exit_code": t.proc.exit_code(),
                })
            })
            .collect();
        out.sort_by(|a, b| a["task_id"].as_str().cmp(&b["task_id"].as_str()));
        out
    }
    /// Await a registered task up to `dur` (without holding the table lock across
    /// the await — clone the completion receiver out first). Returns true if it
    /// finished in time. Unknown id → false.
    async fn wait_until(&self, id: &str, dur: std::time::Duration) -> bool {
        let rx = {
            let Ok(m) = self.inner.lock() else {
                return false;
            };
            match m.get(id) {
                Some(t) => t.proc.done_receiver(),
                None => return false,
            }
        };
        sandbox::exec::wait_done(rx, dur).await
    }

    /// Remove a finished task and return its `(exit_code, stdout, stderr)` — used
    /// when a promoted command actually completed inside the foreground window,
    /// so it doesn't linger in the table.
    fn take_finished(&self, id: &str) -> Option<(Option<i32>, String, String)> {
        let mut m = self.inner.lock().ok()?;
        let entry = m.remove(id)?;
        let (o, e) = entry.proc.snapshot();
        Some((entry.proc.exit_code(), o, e))
    }

    /// `(pid, stdout_so_far, stderr_so_far)` for a still-running task.
    fn running_view(&self, id: &str) -> Option<(Option<u32>, String, String)> {
        let m = self.inner.lock().ok()?;
        let t = m.get(id)?;
        let (o, e) = t.proc.snapshot();
        Some((t.proc.pid, o, e))
    }

    /// Structured task list for surfaces (the TUI status line / `/tasks`).
    fn info(&self) -> Vec<kernel::BackgroundTask> {
        let Ok(m) = self.inner.lock() else {
            return Vec::new();
        };
        let mut out: Vec<kernel::BackgroundTask> = m
            .iter()
            .map(|(id, t)| kernel::BackgroundTask {
                id: id.clone(),
                command: t.command.clone(),
                running: t.proc.is_running(),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Kill every still-running task — called at session end so nothing is left
    /// running detached.
    fn kill_all(&self) {
        if let Ok(m) = self.inner.lock() {
            for t in m.values() {
                if t.proc.is_running() {
                    t.proc.kill();
                }
            }
        }
    }
}
#[async_trait]
impl Tool for ShellExec {
    fn name(&self) -> &str {
        "shell.exec"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Shell
    }
    fn description(&self) -> &str {
        "Run any shell command in the workspace root and capture stdout/stderr/exit \
         code — sed, awk, cat, diff, git, curl, build/test commands, pipelines, \
         anything the shell provides. Prefer fs.edit for exact string-replace edits \
         (it produces a reviewable diff) and glob/grep for finding files — use this \
         for everything else, including multi-step shell pipelines."
    }
    fn blast_radius(&self) -> BlastRadius {
        // Phase 0 runs locally in the workspace; the container backend and the
        // pre-execution scanner (§4.6) harden this in Phase 1.
        BlastRadius::IrreversibleLocal
    }
    fn timeout(&self) -> Option<std::time::Duration> {
        // Self-managed: `execute` runs the command in the foreground only up to
        // `promote_after` (default 50s, or the caller's `timeout_s`), then
        // promotes it to a background task — so the call always returns promptly
        // and never needs the registry's wall-clock kill. `None` = no outer cap.
        None
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Command line to run via the shell" },
                "background": { "type": "boolean", "description": "Start detached immediately and return a task_id (for servers/watchers). Poll with task.output, stop with task.kill." },
                "timeout_s": { "type": "integer", "description": "Seconds to wait before promoting a slow command to a background task (default 50)." }
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let command = arg_str(args, "command")?;
        let background = args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let promote_after = std::time::Duration::from_secs(
            args.get("timeout_s")
                .and_then(Value::as_u64)
                .unwrap_or(PROMOTE_AFTER_SECS),
        );
        // Spawn through the sandbox's execution backend (host or OS-native jail),
        // rooted at the workspace, as a background task from the start. `clear_env`
        // + the allowlist keep injected API keys out of the child so a command
        // can't exfiltrate them via `printenv`. The process is its own group
        // leader, so `task.kill` (or session-end cleanup) tears down the whole
        // tree — no orphans.
        let bg = self
            .sbx
            .exec_background(
                "sh",
                &["-c".to_string(), command.clone()],
                shell_env(),
                true,
            )
            .map_err(|e| ToolError::Failed(e.to_string()))?;

        // Register in the table BEFORE waiting. If the caller cancels (Esc) during
        // the foreground window, this future is dropped mid-wait — and because the
        // task is already tracked, it stays pollable/listable and is reaped by the
        // table's session-end `kill_all`, instead of leaking an untracked process
        // group (the timeout path was fixed by GroupReaper; this closes the cancel
        // path).
        let task_id = self.tasks.register(command.clone(), bg);

        // Foreground window (skipped for explicit background): if it finishes in
        // time, unregister and return the full output like an ordinary command.
        if !background && self.tasks.wait_until(&task_id, promote_after).await {
            if let Some((exit_code, stdout, stderr)) = self.tasks.take_finished(&task_id) {
                return Ok(json!({
                    "command": command,
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                }));
            }
        }

        // Still running past the deadline (or explicitly backgrounded): leave it
        // tracked and hand back the task id + partial output.
        let (pid, stdout, stderr) =
            self.tasks
                .running_view(&task_id)
                .unwrap_or((None, String::new(), String::new()));
        Ok(json!({
            "command": command,
            "status": "running",
            "task_id": task_id,
            "pid": pid,
            "stdout_so_far": stdout,
            "stderr_so_far": stderr,
            "hint": "still running — poll with task.output {task_id}, stop with task.kill {task_id}",
        }))
    }

    async fn preview(&self, args: &Value) -> Option<String> {
        args.get("command")
            .and_then(|v| v.as_str())
            .map(|c| format!("$ {c}"))
    }
}

// ── background tasks: task.output / task.kill / task.list ────────────────────

/// Kill every still-running task when the table (and thus the session's tool
/// registry) is dropped — no detached command outlives the session.
impl Drop for TaskTable {
    fn drop(&mut self) {
        self.kill_all();
    }
}

struct TaskOutput {
    tasks: Arc<TaskTable>,
}

#[async_trait]
impl Tool for TaskOutput {
    fn name(&self) -> &str {
        "task.output"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Shell
    }
    fn description(&self) -> &str {
        "Check a background shell task — one that a slow `shell.exec` was promoted \
         into, or that you started with `background: true`. Returns its current \
         stdout/stderr, whether it's still running, and the exit code once it \
         finishes. Omit `task_id` to list every task."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "task_id": { "type": "string", "description": "Task id from shell.exec (omit to list all tasks)" } }
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        match args.get("task_id").and_then(Value::as_str) {
            Some(id) => self
                .tasks
                .snapshot(id)
                .ok_or_else(|| ToolError::Failed(format!("no such task '{id}'"))),
            None => Ok(json!({ "tasks": self.tasks.list() })),
        }
    }
}

struct TaskKill {
    tasks: Arc<TaskTable>,
}

#[async_trait]
impl Tool for TaskKill {
    fn name(&self) -> &str {
        "task.kill"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Shell
    }
    fn description(&self) -> &str {
        "Stop a background shell task — SIGKILLs its whole process group (so any \
         child processes, e.g. a dev server's workers, die too)."
    }
    fn blast_radius(&self) -> BlastRadius {
        // A local, expected action on a task this agent started — no approval nag.
        BlastRadius::ReversibleLocal
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "task_id": { "type": "string", "description": "Task id to kill" } },
            "required": ["task_id"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let id = arg_str(args, "task_id")?;
        if self.tasks.kill(&id) {
            Ok(json!({ "task_id": id, "killed": true }))
        } else {
            Err(ToolError::Failed(format!("no such task '{id}'")))
        }
    }
}

struct TaskList {
    tasks: Arc<TaskTable>,
}

#[async_trait]
impl Tool for TaskList {
    fn name(&self) -> &str {
        "task.list"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Shell
    }
    fn description(&self) -> &str {
        "List all background shell tasks with their status and exit codes."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &Value) -> Result<Value, ToolError> {
        Ok(json!({ "tasks": self.tasks.list() }))
    }
}

// ── multi_edit ─────────────────────────────────────────────────────────────

struct MultiEdit {
    sbx: Arc<WorkspaceSandbox>,
    pins: PreviewPins,
    lsp: LspHandle,
    artifacts: Arc<dyn kernel::ArtifactStore>,
}

/// Apply a sequence of exact-substring replacements to `content`, in order (each
/// edit sees the previous one's result). All-or-nothing: if any edit's
/// `old_string` is missing — or ambiguous without `replace_all` — the whole
/// batch fails and nothing is returned, so a partial edit can never land.
/// Reject `old_string` values that corrupt files rather than edit them: an
/// empty needle matches *between every character* (so `replace` splices
/// `new_string` throughout), and `old == new` is a no-op that still writes and
/// burns a snapshot. Both must fail loudly, not "succeed".
fn validate_edit(old: &str, new: &str) -> Result<(), ToolError> {
    if old.is_empty() {
        return Err(ToolError::Failed(
            "old_string must not be empty (an empty match would insert new_string between every character)".into(),
        ));
    }
    if old == new {
        return Err(ToolError::Failed(
            "old_string and new_string are identical — nothing to change".into(),
        ));
    }
    Ok(())
}

/// Resolve the (old, new) pair to use for a byte-exact edit, tolerating a
/// line-ending mismatch. On a CRLF file `new` is always normalized to CRLF so a
/// replacement never leaves mixed endings — this matters even when `old` matched
/// exactly (the model can supply a CRLF `old` but an LF-only `new`). If `old`
/// isn't found verbatim, retries with `old`'s endings normalized to CRLF (the
/// model may hold LF-only text from an earlier read). `None` if neither form is
/// present. `to_crlf` is idempotent, so an already-CRLF `new` is unchanged.
fn resolve_edit(content: &str, old: &str, new: &str) -> Option<(String, String)> {
    let file_is_crlf = content.contains("\r\n");
    let to_crlf = |s: &str| s.replace("\r\n", "\n").replace('\n', "\r\n");
    if content.contains(old) {
        let new = if file_is_crlf {
            to_crlf(new)
        } else {
            new.to_string()
        };
        return Some((old.to_string(), new));
    }
    if file_is_crlf {
        let crlf_old = to_crlf(old);
        if content.contains(&crlf_old) {
            return Some((crlf_old, to_crlf(new)));
        }
    }
    None
}

fn apply_edits(content: &str, edits: &[Value]) -> Result<String, ToolError> {
    let mut cur = content.to_string();
    for (i, e) in edits.iter().enumerate() {
        let n = i + 1;
        let old = e
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Args(format!("edit #{n}: missing 'old_string'")))?;
        let new = e
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Args(format!("edit #{n}: missing 'new_string'")))?;
        validate_edit(old, new).map_err(|err| ToolError::Failed(format!("edit #{n}: {err}")))?;
        let replace_all = e
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (old, new) = resolve_edit(&cur, old, new)
            .ok_or_else(|| ToolError::Failed(format!("edit #{n}: old_string not found")))?;
        let count = cur.matches(&old).count();
        if count > 1 && !replace_all {
            return Err(ToolError::Failed(format!(
                "edit #{n}: old_string appears {count} times; pass replace_all or use a more specific string"
            )));
        }
        cur = if replace_all {
            cur.replace(&old, &new)
        } else {
            cur.replacen(&old, &new, 1)
        };
    }
    Ok(cur)
}

#[async_trait]
impl Tool for MultiEdit {
    fn name(&self) -> &str {
        "multi_edit"
    }
    fn icon(&self) -> &'static str {
        "❏"
    }
    fn description(&self) -> &str {
        "Apply several exact-substring edits to ONE file atomically, in order — one \
         snapshot, one write, one combined diff. All-or-nothing: if any edit's \
         `old_string` isn't found (or is ambiguous without `replace_all`), NOTHING is \
         written. Prefer this over multiple `fs.edit` calls when changing several places \
         in the same file — it's cheaper and can't leave the file half-edited. Each edit \
         sees the result of the previous one."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::ReversibleLocal
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative or absolute path" },
                "edits": {
                    "type": "array",
                    "description": "Edits applied in order; each sees the previous edit's result",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string", "description": "Exact text to replace" },
                            "new_string": { "type": "string", "description": "Replacement text" },
                            "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)" }
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let path = arg_str(args, "path")?;
        let edits = args
            .get("edits")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::Args("expected array 'edits'".into()))?;
        if edits.is_empty() {
            return Err(ToolError::Args("'edits' is empty".into()));
        }
        // Serialize same-file edits within a turn (P0-4).
        let _guard = self.sbx.path_guard(&path).await;
        let content = self
            .sbx
            .read(&path)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        // Refuse if the file changed since the approved preview (P1-1).
        self.pins.check(args, &path, &content)?;
        let updated = apply_edits(&content, edits)?;
        let baseline = pre_edit_lsp(&self.lsp, &self.sbx, &path, &content).await;
        let snapshot = self
            .sbx
            .write(&path, &updated)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let diff = make_diff(&path, &content, &updated);
        let mut out = json!({
            "path": path,
            "edits_applied": edits.len(),
            "diff": diff,
            "old": content,
            "new": updated,
            "snapshot": snapshot
        });
        if let Some(report) = post_edit_lsp(
            &self.lsp,
            &self.sbx,
            &self.artifacts,
            &path,
            &updated,
            baseline,
        )
        .await
        {
            out["lsp"] = report;
        }
        Ok(out)
    }

    async fn preview(&self, args: &Value) -> Option<String> {
        let path = args.get("path")?.as_str()?;
        let edits = args.get("edits")?.as_array()?;
        let content = self.sbx.read(path).await.ok()?;
        // Pin what the approval card will show (P1-1).
        self.pins.pin(args, &content);
        match apply_edits(&content, edits) {
            Ok(updated) => Some(cap_preview(&make_diff(path, &content, &updated))),
            Err(e) => Some(format!("({e} — this multi_edit would fail)")),
        }
    }
}

// ── git (read-only) ──────────────────────────────────────────────────────────

struct Git {
    sbx: Arc<WorkspaceSandbox>,
}

/// Reject an argument that could be mistaken for a git flag (leading `-`).
/// User-supplied revs/paths are passed as plain argv (no shell), and paths go
/// after `--`, so this closes the only remaining flag-injection gap.
fn safe_git_arg(s: &str) -> Result<&str, ToolError> {
    if s.starts_with('-') {
        return Err(ToolError::Args(format!(
            "invalid git argument '{s}' (must not start with '-')"
        )));
    }
    Ok(s)
}

#[async_trait]
impl Tool for Git {
    fn name(&self) -> &str {
        "git"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Vcs
    }
    fn description(&self) -> &str {
        "Git as a structured tool. Read-only inspection — `status`, `diff`, `log`, \
         `blame`, `show` — plus `add` and `commit` (which are approval-gated). Use this \
         instead of `shell.exec` for git: reads never mutate the repo, and writes go \
         through the human gate. Branches/pushes/rebases are intentionally out of scope \
         — use `shell.exec` for those."
    }
    fn blast_radius(&self) -> BlastRadius {
        // Mixed: reads are READ, commit/add stage/record changes. Report the max
        // (a commit is reversible via git). Policy gates the mutating subcommands.
        BlastRadius::ReversibleLocal
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subcommand": { "type": "string", "enum": ["status","diff","log","blame","show","add","commit"], "description": "Which git command (add/commit are approval-gated)" },
                "path": { "type": "string", "description": "Limit to this file/dir (diff/log/blame); what to stage (add, default '.')" },
                "rev": { "type": "string", "description": "Revision/ref for diff/show/log (e.g. HEAD, a branch, a SHA)" },
                "staged": { "type": "boolean", "description": "diff: show staged changes (--staged)" },
                "max_count": { "type": "integer", "description": "log: number of commits (default 20)" },
                "message": { "type": "string", "description": "commit: the commit message (required for commit)" },
                "all": { "type": "boolean", "description": "commit: stage all tracked changes first (-a)" }
            },
            "required": ["subcommand"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let sub = arg_str(args, "subcommand")?;
        let path = args.get("path").and_then(Value::as_str);
        let rev = args.get("rev").and_then(Value::as_str);
        let mut ga: Vec<String> = Vec::new();
        match sub.as_str() {
            "status" => ga.extend(["status".into(), "--short".into(), "--branch".into()]),
            "diff" => {
                ga.push("diff".into());
                if args.get("staged").and_then(Value::as_bool).unwrap_or(false) {
                    ga.push("--staged".into());
                }
                if let Some(r) = rev {
                    ga.push(safe_git_arg(r)?.into());
                }
            }
            "log" => {
                let n = args.get("max_count").and_then(Value::as_u64).unwrap_or(20);
                ga.extend(["log".into(), "--oneline".into(), "-n".into(), n.to_string()]);
                if let Some(r) = rev {
                    ga.push(safe_git_arg(r)?.into());
                }
            }
            "blame" => {
                path.ok_or_else(|| ToolError::Args("blame requires 'path'".into()))?;
                ga.push("blame".into());
            }
            "show" => {
                ga.extend([
                    "show".into(),
                    "--stat".into(),
                    safe_git_arg(rev.unwrap_or("HEAD"))?.into(),
                ]);
            }
            "add" => ga.push("add".into()),
            "commit" => {
                let msg = arg_str(args, "message")
                    .map_err(|_| ToolError::Args("commit requires 'message'".into()))?;
                ga.push("commit".into());
                if args.get("all").and_then(Value::as_bool).unwrap_or(false) {
                    ga.push("-a".into());
                }
                // `-m <msg>` — msg is the value consumed by -m, so a leading '-' is
                // harmless; passed as its own argv element (no shell).
                ga.push("-m".into());
                ga.push(msg);
            }
            other => return Err(ToolError::Args(format!("unknown git subcommand '{other}'"))),
        }
        // User paths go after `--` so they can never be read as flags.
        if matches!(sub.as_str(), "diff" | "log" | "blame") {
            if let Some(p) = path {
                ga.push("--".into());
                ga.push(safe_git_arg(p)?.into());
            }
        }
        if sub == "add" {
            ga.push("--".into());
            ga.push(safe_git_arg(path.unwrap_or("."))?.into());
        }
        // Fixed git subcommand, run through the sandbox backend (inherits env —
        // git needs $HOME/$PATH; it's not an arbitrary-command surface).
        let output = self
            .sbx
            .exec("git", &ga, vec![], false)
            .await
            .map_err(|e| ToolError::Failed(format!("failed to run git: {e}")))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Cap large output (a long log/diff) so it doesn't flood context — with
        // an explicit marker so a capped diff is never mistaken for a complete one.
        let total_lines = stdout.lines().count();
        let mut capped: String = stdout.lines().take(400).collect::<Vec<_>>().join("\n");
        if total_lines > 400 {
            capped.push_str(&format!(
                "\n[truncated: showing first 400 of {total_lines} lines — scope with `path` or use shell.exec git for full output]"
            ));
        }
        Ok(json!({
            "subcommand": sub,
            "exit_code": output.status,
            "stdout": capped,
            "truncated": total_lines > 400,
            "stderr": String::from_utf8_lossy(&output.stderr),
        }))
    }

    async fn preview(&self, args: &Value) -> Option<String> {
        // Only the mutating subcommands reach the gate; show what will run.
        match args.get("subcommand").and_then(Value::as_str)? {
            "commit" => {
                let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
                let all = if args.get("all").and_then(Value::as_bool).unwrap_or(false) {
                    " -a"
                } else {
                    ""
                };
                Some(format!("git commit{all} -m {msg:?}"))
            }
            "add" => Some(format!(
                "git add -- {}",
                args.get("path").and_then(Value::as_str).unwrap_or(".")
            )),
            _ => None,
        }
    }
}

// ── diagnostics ──────────────────────────────────────────────────────────────

struct Diagnostics {
    sbx: Arc<WorkspaceSandbox>,
}

/// A parser turning a checker's `(stdout, stderr)` into structured diagnostics.
/// Uniform signature so every toolchain plugs into the same runner.
type DiagParse = fn(&str, &str) -> Vec<Value>;

/// Parse `cargo check`/`cargo clippy --message-format=json` stdout into
/// structured diagnostics. Each stdout line is one JSON object; we keep
/// `compiler-message` errors and warnings, pulling the primary span for
/// file/line/column. Clippy reuses this — it emits the identical JSON shape.
fn cargo_diagnostics(stdout: &str, _stderr: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        let level = msg.get("level").and_then(Value::as_str).unwrap_or("");
        if !matches!(level, "error" | "warning") {
            continue;
        }
        let text = msg
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let code = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let (file, line_no, col) = msg
            .get("spans")
            .and_then(Value::as_array)
            .and_then(|spans| {
                spans
                    .iter()
                    .find(|s| s.get("is_primary").and_then(Value::as_bool) == Some(true))
                    .or_else(|| spans.first())
            })
            .map(|s| {
                (
                    s.get("file_name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    s.get("line_start").and_then(Value::as_u64).unwrap_or(0),
                    s.get("column_start").and_then(Value::as_u64).unwrap_or(0),
                )
            })
            .unwrap_or_default();
        out.push(json!({
            "severity": level, "file": file, "line": line_no, "column": col,
            "code": code, "message": text
        }));
    }
    out
}

/// The toolchains `diagnostics` knows how to check. Each maps to a FIXED set of
/// argument-free commands (see [`plan_checkers`]) — no user-supplied command
/// string ever runs (that would be an ungated `shell.exec` bypass).
#[derive(Clone, Copy, PartialEq)]
enum DiagLang {
    Rust,
    Go,
    Python,
    TypeScript,
    JavaScript,
    Cpp,
    JavaMaven,
    JavaGradle,
}

impl DiagLang {
    /// Stable id reported back to the model.
    fn id(self) -> &'static str {
        match self {
            DiagLang::Rust => "rust",
            DiagLang::Go => "go",
            DiagLang::Python => "python",
            DiagLang::TypeScript => "typescript",
            DiagLang::JavaScript => "javascript",
            DiagLang::Cpp => "cpp",
            DiagLang::JavaMaven | DiagLang::JavaGradle => "java",
        }
    }
}

/// True if any file directly under `root` has one of `exts` (shallow, no walk).
fn has_source_ext(root: &std::path::Path, exts: &[&str]) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.path()
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| exts.contains(&x))
            .unwrap_or(false)
    })
}

/// Detect the toolchain from marker files at the workspace root. First match
/// wins, most-specific first (tsconfig before package.json; build files before a
/// bare source-extension scan).
fn detect_lang(root: &std::path::Path) -> Option<DiagLang> {
    let has = |f: &str| root.join(f).exists();
    if has("Cargo.toml") {
        Some(DiagLang::Rust)
    } else if has("go.mod") {
        Some(DiagLang::Go)
    } else if has("tsconfig.json") {
        Some(DiagLang::TypeScript)
    } else if has("package.json") {
        Some(DiagLang::JavaScript)
    } else if has("pom.xml") {
        Some(DiagLang::JavaMaven)
    } else if has("build.gradle") || has("build.gradle.kts") {
        Some(DiagLang::JavaGradle)
    } else if has("pyproject.toml")
        || has("ruff.toml")
        || has(".ruff.toml")
        || has("setup.py")
        || has("requirements.txt")
    {
        Some(DiagLang::Python)
    } else if has("CMakeLists.txt")
        || has_source_ext(root, &["cpp", "cc", "cxx", "hpp", "hh", "c", "h"])
    {
        Some(DiagLang::Cpp)
    } else {
        None
    }
}

/// One fixed checker invocation: a program, fixed args, a label, and a parser.
struct Checker {
    program: &'static str,
    args: &'static [&'static str],
    label: &'static str,
    parse: DiagParse,
}

/// The fixed command(s) to run for a language. Multiple entries run in sequence
/// and their diagnostics merge — e.g. Rust prefers `cargo clippy` (a superset of
/// `cargo check`) and falls back to `cargo check` when the clippy component is
/// absent; Go runs `go build` (compile errors) then `go vet` (lint warnings).
fn plan_checkers(lang: DiagLang) -> Vec<Checker> {
    match lang {
        DiagLang::Rust => {
            if program_on_path("cargo-clippy") {
                vec![Checker {
                    program: "cargo",
                    args: &["clippy", "--message-format=json", "--quiet"],
                    label: "cargo clippy",
                    parse: cargo_diagnostics,
                }]
            } else {
                vec![Checker {
                    program: "cargo",
                    args: &["check", "--message-format=json", "--quiet"],
                    label: "cargo check",
                    parse: cargo_diagnostics,
                }]
            }
        }
        DiagLang::Go => vec![
            Checker {
                program: "go",
                args: &["build", "./..."],
                label: "go build",
                parse: gobuild_diagnostics,
            },
            Checker {
                program: "go",
                args: &["vet", "./..."],
                label: "go vet",
                parse: govet_diagnostics,
            },
        ],
        DiagLang::Python => vec![Checker {
            program: "ruff",
            args: &["check", "--output-format=json", "."],
            label: "ruff",
            parse: ruff_diagnostics,
        }],
        DiagLang::TypeScript => vec![Checker {
            program: "npx",
            args: &["--no-install", "tsc", "--noEmit", "--pretty", "false"],
            label: "tsc",
            parse: tsc_diagnostics,
        }],
        DiagLang::JavaScript => vec![Checker {
            program: "npx",
            args: &["--no-install", "eslint", "--format", "json", "."],
            label: "eslint",
            parse: eslint_diagnostics,
        }],
        DiagLang::Cpp => vec![Checker {
            program: "cppcheck",
            args: &[
                "--enable=warning,style,performance,portability",
                "--quiet",
                "--template={file}:{line}:{column}:{severity}:{id}:{message}",
                ".",
            ],
            label: "cppcheck",
            parse: cppcheck_diagnostics,
        }],
        DiagLang::JavaMaven => vec![Checker {
            program: "mvn",
            args: &["-q", "-DskipTests", "compile"],
            label: "mvn compile",
            parse: java_diagnostics,
        }],
        DiagLang::JavaGradle => vec![Checker {
            program: "gradle",
            args: &["-q", "compileJava"],
            label: "gradle compileJava",
            parse: java_diagnostics,
        }],
    }
}

/// Shared Go parser: `file.go:line[:col]: message`, skipping `# package` headers.
/// `severity` is fixed per checker (build → error, vet → warning).
fn parse_go_stream(text: &str, severity: &str) -> Vec<Value> {
    let re = Regex::new(r"^(.+?\.go):(\d+):(?:(\d+):)?\s*(.*)$").unwrap();
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with('#') {
                return None;
            }
            let c = re.captures(l)?;
            Some(json!({
                "severity": severity,
                "file": &c[1],
                "line": c[2].parse::<u64>().unwrap_or(0),
                "column": c.get(3).and_then(|m| m.as_str().parse::<u64>().ok()).unwrap_or(0),
                "code": Value::Null,
                "message": &c[4],
            }))
        })
        .collect()
}

/// `go build ./...` compile errors (to stderr) → severity `error`.
fn gobuild_diagnostics(_stdout: &str, stderr: &str) -> Vec<Value> {
    parse_go_stream(stderr, "error")
}

/// `go vet ./...` findings (to stderr) → severity `warning`.
fn govet_diagnostics(_stdout: &str, stderr: &str) -> Vec<Value> {
    parse_go_stream(stderr, "warning")
}

/// Parse `eslint --format json`: `[{filePath, messages:[{line,column,severity,
/// ruleId,message}]}]`. ESLint severity 2 = error, 1 = warning.
fn eslint_diagnostics(stdout: &str, _stderr: &str) -> Vec<Value> {
    let Ok(files) = serde_json::from_str::<Vec<Value>>(stdout) else {
        return vec![];
    };
    let mut out = Vec::new();
    for f in &files {
        let path = f.get("filePath").and_then(Value::as_str).unwrap_or("");
        let Some(msgs) = f.get("messages").and_then(Value::as_array) else {
            continue;
        };
        for m in msgs {
            let severity = if m.get("severity").and_then(Value::as_u64) == Some(2) {
                "error"
            } else {
                "warning"
            };
            out.push(json!({
                "severity": severity,
                "file": path,
                "line": m.get("line").and_then(Value::as_u64).unwrap_or(0),
                "column": m.get("column").and_then(Value::as_u64).unwrap_or(0),
                "code": m.get("ruleId").and_then(Value::as_str),
                "message": m.get("message").and_then(Value::as_str).unwrap_or(""),
            }));
        }
    }
    out
}

/// Parse cppcheck's `{file}:{line}:{column}:{severity}:{id}:{message}` template
/// (emitted to stderr). cppcheck `error` → error; everything else → warning.
fn cppcheck_diagnostics(_stdout: &str, stderr: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| {
            let parts: Vec<&str> = l.splitn(6, ':').collect();
            if parts.len() < 6 {
                return None;
            }
            let severity = if parts[3] == "error" {
                "error"
            } else {
                "warning"
            };
            Some(json!({
                "severity": severity,
                "file": parts[0],
                "line": parts[1].parse::<u64>().unwrap_or(0),
                "column": parts[2].parse::<u64>().unwrap_or(0),
                "code": parts[4],
                "message": parts[5].trim(),
            }))
        })
        .collect()
}

/// Parse javac-style diagnostics from Maven and Gradle output (either stream):
/// Maven `[ERROR] /path/File.java:[12,5] msg` and javac `/path/File.java:12:
/// error: msg`.
fn java_diagnostics(stdout: &str, stderr: &str) -> Vec<Value> {
    let mvn = Regex::new(r"^\[ERROR\]\s+(.+?\.java):\[(\d+),(\d+)\]\s+(.*)$").unwrap();
    let javac = Regex::new(r"^(.+?\.java):(\d+):\s*(error|warning):\s*(.*)$").unwrap();
    let mut out = Vec::new();
    for l in stdout.lines().chain(stderr.lines()) {
        let l = l.trim();
        if let Some(c) = mvn.captures(l) {
            out.push(json!({
                "severity": "error",
                "file": &c[1],
                "line": c[2].parse::<u64>().unwrap_or(0),
                "column": c[3].parse::<u64>().unwrap_or(0),
                "code": Value::Null,
                "message": &c[4],
            }));
        } else if let Some(c) = javac.captures(l) {
            out.push(json!({
                "severity": &c[3],
                "file": &c[1],
                "line": c[2].parse::<u64>().unwrap_or(0),
                "column": 0,
                "code": Value::Null,
                "message": &c[4],
            }));
        }
    }
    out
}

/// Parse `tsc --noEmit --pretty false` output lines:
/// `src/x.ts(12,5): error TS2322: Type '...' is not assignable ...`
fn tsc_diagnostics(stdout: &str, _stderr: &str) -> Vec<Value> {
    let re = Regex::new(r"^(.+?)\((\d+),(\d+)\):\s*(error|warning)\s+(TS\d+):\s*(.*)$").unwrap();
    stdout
        .lines()
        .filter_map(|l| {
            let c = re.captures(l.trim())?;
            Some(json!({
                "severity": &c[4],
                "file": &c[1],
                "line": c[2].parse::<u64>().unwrap_or(0),
                "column": c[3].parse::<u64>().unwrap_or(0),
                "code": &c[5],
                "message": &c[6],
            }))
        })
        .collect()
}

/// Parse `ruff check --output-format=json`: an array of
/// `{filename, location:{row,column}, code, message}`. Ruff emits lint findings
/// (no per-item severity), so all are reported as `warning`.
fn ruff_diagnostics(stdout: &str, _stderr: &str) -> Vec<Value> {
    let Ok(items) = serde_json::from_str::<Vec<Value>>(stdout) else {
        return vec![];
    };
    items
        .iter()
        .map(|it| {
            let loc = it.get("location");
            json!({
                "severity": "warning",
                "file": it.get("filename").and_then(Value::as_str).unwrap_or(""),
                "line": loc.and_then(|l| l.get("row")).and_then(Value::as_u64).unwrap_or(0),
                "column": loc.and_then(|l| l.get("column")).and_then(Value::as_u64).unwrap_or(0),
                "code": it.get("code").and_then(Value::as_str),
                "message": it.get("message").and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect()
}

#[async_trait]
impl Tool for Diagnostics {
    fn name(&self) -> &str {
        "diagnostics"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Diagnostic
    }
    fn description(&self) -> &str {
        "Get STRUCTURED compiler/linter diagnostics — {file, line, column, severity, \
         code, message} — for the workspace, instead of scraping raw build output. \
         Auto-detects the toolchain from project markers and runs the right checker(s): \
         Rust→`cargo clippy` (or `cargo check`), Go→`go build`+`go vet`, \
         Python→`ruff`, TypeScript→`tsc`, JavaScript→`eslint`, C/C++→`cppcheck`, \
         Java→`mvn`/`gradle` compile. Pass `language` to force one. Checkers not \
         installed are reported as skipped, never as a clean run. Build tools may \
         execute project-defined build scripts/plugins, so this action requires \
         approval. Use after edits to find exactly what broke and where."
    }
    fn blast_radius(&self) -> BlastRadius {
        // Cargo/npm/Maven/Gradle and language plugins execute repository-owned
        // code and write caches/artifacts. Treating this as Read would bypass the
        // human gate and let an untrusted checkout exfiltrate secrets.
        BlastRadius::IrreversibleLocal
    }
    fn timeout(&self) -> Option<std::time::Duration> {
        // A cold `cargo check`/`tsc`/`mvn` on a large workspace easily exceeds 60s.
        Some(std::time::Duration::from_secs(600)) // 10 min
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "language": {
                    "type": "string",
                    "enum": ["rust","go","python","typescript","javascript","cpp","java"],
                    "description": "Force a toolchain; omit to auto-detect from the workspace"
                }
            }
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let root = self.sbx.root();
        // Language is an ENUM, never a free command string — no shell.exec bypass.
        // Each variant maps to a FIXED command set (see `plan_checkers`).
        let lang = match args.get("language").and_then(Value::as_str) {
            Some("rust") | Some("rs") => Some(DiagLang::Rust),
            Some("go") | Some("golang") => Some(DiagLang::Go),
            Some("python") | Some("py") => Some(DiagLang::Python),
            Some("typescript") | Some("ts") => Some(DiagLang::TypeScript),
            Some("javascript") | Some("js") | Some("node") => Some(DiagLang::JavaScript),
            Some("cpp") | Some("c++") | Some("c") | Some("cc") => Some(DiagLang::Cpp),
            // Forced Java needs a build tool — pick whichever marker is present.
            Some("java") => {
                if root.join("pom.xml").exists() {
                    Some(DiagLang::JavaMaven)
                } else if root.join("build.gradle").exists()
                    || root.join("build.gradle.kts").exists()
                {
                    Some(DiagLang::JavaGradle)
                } else {
                    return Err(ToolError::Args(
                        "java forced but no pom.xml or build.gradle found".into(),
                    ));
                }
            }
            Some(other) => {
                return Err(ToolError::Args(format!(
                    "unknown language '{other}' (rust|go|python|typescript|javascript|cpp|java)"
                )));
            }
            None => detect_lang(root),
        };
        let Some(lang) = lang else {
            return Ok(json!({
                "supported": false,
                "note": "no supported project detected (looked for Cargo.toml, go.mod, tsconfig.json, package.json, pom.xml, build.gradle, pyproject.toml/ruff.toml/setup.py/requirements.txt, CMakeLists.txt or C/C++ sources). Pass `language`, or use shell.exec for another checker."
            }));
        };

        let mut diags: Vec<Value> = Vec::new();
        let mut ran: Vec<&str> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        for c in plan_checkers(lang) {
            // Under a sandbox wrapper a missing checker surfaces as a wrapper
            // exec failure, not our spawn error — so check PATH up front to keep
            // the honest "not installed → skipped" report.
            if !program_on_path(c.program) {
                skipped.push(format!("{} ('{}' not on PATH)", c.label, c.program));
                continue;
            }
            let owned: Vec<String> = c.args.iter().map(|s| s.to_string()).collect();
            let output = match self.sbx.exec(c.program, &owned, shell_env(), true).await {
                Ok(o) => o,
                Err(e) => {
                    skipped.push(format!("{} ({e})", c.label));
                    continue;
                }
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut found = (c.parse)(&stdout, &stderr);
            // A non-zero exit with nothing parsed usually means the checker itself
            // failed (bad config, a missing sub-tool via npx) — surface stderr
            // rather than silently reporting a clean run.
            if found.is_empty() && output.status != Some(0) {
                let tail: Vec<&str> = stderr.lines().rev().take(6).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                notes.push(format!(
                    "{} exited non-zero with no parsed diagnostics — stderr: {}",
                    c.label,
                    tail.join(" | ")
                ));
            }
            ran.push(c.label);
            diags.append(&mut found);
        }

        // Merge across checkers (clippy + check, build + vet) — drop exact dupes.
        let mut seen = std::collections::HashSet::new();
        diags.retain(|d| {
            let key = format!(
                "{}|{}|{}|{}",
                d["file"], d["line"], d["column"], d["message"]
            );
            seen.insert(key)
        });

        if ran.is_empty() {
            return Ok(json!({
                "supported": false,
                "language": lang.id(),
                "note": format!(
                    "detected {} but no checker could run. Skipped: {}",
                    lang.id(),
                    if skipped.is_empty() { "(none)".into() } else { skipped.join("; ") }
                ),
            }));
        }

        let errors = diags.iter().filter(|d| d["severity"] == "error").count();
        let warnings = diags.iter().filter(|d| d["severity"] == "warning").count();
        let mut result = json!({
            "supported": true,
            "language": lang.id(),
            "checkers_ran": ran,
            "errors": errors,
            "warnings": warnings,
            "diagnostics": diags,
        });
        if !skipped.is_empty() {
            result["checkers_skipped"] = json!(skipped);
        }
        if !notes.is_empty() {
            result["notes"] = json!(notes);
        }
        Ok(result)
    }
}

// ── planning ─────────────────────────────────────────────────────────────────

/// A cognitive tool (no side effects): the model maintains a live checklist for
/// a multi-step task. It passes the *full* list each call, so the latest call
/// is the current plan; the surface renders it and the event log captures the
/// agent's intent + progress over time (P3). This is what lets the agent lay
/// out a plan for complex work and tick it off as it goes.
struct UpdatePlan;

#[async_trait]
impl Tool for UpdatePlan {
    fn name(&self) -> &str {
        "update_plan"
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Plan
    }
    fn description(&self) -> &str {
        "Create and maintain your TODO list for the current task — this is your planning \
         tool and the user's live progress view. Call it PROACTIVELY: the moment a task \
         needs 3 or more steps, before doing anything else, call it with the full ordered \
         list of steps (first step `in_progress`, the rest `pending`). Then call it again \
         after each step to mark that step `completed` and set the next to `in_progress`. \
         ALWAYS pass the COMPLETE list every time (it replaces the previous one). Exactly \
         ONE step is `in_progress`. Skipping this on a multi-step task is a mistake — the \
         user relies on it to see what you're doing. Optional `explanation`: a one-line \
         note about the update (shown above the list)."
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "explanation": { "type": "string", "description": "Optional one-line note about this plan update (what changed / why)." },
                "steps": {
                    "type": "array",
                    "description": "The full ordered list of steps.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string", "description": "Short imperative step description." },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                        },
                        "required": ["title", "status"]
                    }
                }
            },
            "required": ["steps"]
        })
    }
    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let steps = args
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::Args("expected array 'steps'".into()))?;
        let mut out = Vec::with_capacity(steps.len());
        // Enforce the invariant the model is asked to keep: at most ONE step
        // `in_progress`. If it slips and marks several, keep the first and demote the
        // rest to `pending` — so the rendered plan is always unambiguous.
        let mut seen_active = false;
        for s in steps {
            let title = s
                .get("title")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::Args("each step needs a 'title'".into()))?;
            // compatible enum is `completed`; accept the legacy `done` too and
            // normalize, so a model that emits either works.
            let mut status = match s.get("status").and_then(Value::as_str) {
                Some("in_progress") => "in_progress",
                Some("completed" | "done") => "completed",
                _ => "pending",
            };
            if status == "in_progress" {
                if seen_active {
                    status = "pending";
                } else {
                    seen_active = true;
                }
            }
            out.push(json!({ "title": title, "status": status }));
        }
        let done = out.iter().filter(|s| s["status"] == "completed").count();
        let mut result = json!({ "steps": out, "total": out.len(), "done": done });
        if let Some(exp) = args.get("explanation").and_then(Value::as_str) {
            if !exp.trim().is_empty() {
                result["explanation"] = json!(exp.trim());
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scan source with the outline rules the same way the tool does.
    fn scan_outline(path: &str, src: &str) -> Vec<(String, String, usize)> {
        let rules = outline_rules(path);
        let mut out = Vec::new();
        for (i, line) in src.lines().enumerate() {
            for (kind, re) in &rules {
                if let Some(c) = re.captures(line) {
                    if let Some(n) = c.name("name") {
                        out.push((kind.to_string(), n.as_str().to_string(), i + 1));
                        break;
                    }
                }
            }
        }
        out
    }

    #[test]
    fn code_outline_rust() {
        let src =
            "pub fn foo() {}\nstruct Bar;\n    async fn baz() {}\nimpl Bar {}\npub trait T {}\n";
        let s = scan_outline("x.rs", src);
        assert!(s.iter().any(|(k, n, _)| k == "fn" && n == "foo"));
        assert!(s.iter().any(|(k, n, _)| k == "struct" && n == "Bar"));
        assert!(s.iter().any(|(k, n, _)| k == "fn" && n == "baz"));
        assert!(s.iter().any(|(k, n, _)| k == "impl" && n == "Bar"));
        assert!(s.iter().any(|(k, n, _)| k == "trait" && n == "T"));
    }

    #[test]
    fn code_outline_python() {
        let src = "class A:\n    def m(self):\n        pass\nasync def h():\n    pass\n";
        let s = scan_outline("x.py", src);
        assert!(
            s.iter()
                .any(|(k, n, l)| k == "class" && n == "A" && *l == 1)
        );
        assert!(s.iter().any(|(k, n, _)| k == "def" && n == "m"));
        assert!(s.iter().any(|(k, n, _)| k == "def" && n == "h"));
    }

    #[test]
    fn code_outline_unknown_ext_is_empty() {
        assert!(outline_rules("notes.txt").is_empty());
    }

    #[test]
    fn ssrf_guard_blocks_internal_and_bad_schemes() {
        // IP literals and a non-http scheme need no DNS, so this is hermetic.
        for bad in [
            "http://127.0.0.1/x",
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://[::1]/",
            "http://0.0.0.0/",
            "http://100.64.0.1/",       // CGNAT
            "ftp://example.com/passwd", // disallowed scheme
            "file:///etc/passwd",       // disallowed scheme
        ] {
            let u = reqwest::Url::parse(bad).unwrap();
            assert!(validate_public_url(&u).is_err(), "should block {bad}");
        }
        // A public IP literal passes the guard.
        let ok = reqwest::Url::parse("http://1.1.1.1/").unwrap();
        assert!(
            validate_public_url(&ok).is_ok(),
            "public address should pass"
        );
    }

    #[tokio::test]
    async fn references_whole_word_and_definition_flag() {
        let dir = std::env::temp_dir().join(format!("medha-refs-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        // `helper` is defined once and called once; `helperx` must NOT match.
        std::fs::write(
            dir.join("a.rs"),
            "fn helper() {}\nfn main() {\n    helper();\n    let helperx = 1;\n}\n",
        )
        .unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let tool = References {
            sbx,
            lsp: Arc::new(Mutex::new(None)),
        };
        let out = tool.execute(&json!({ "symbol": "helper" })).await.unwrap();
        // Whole-word: matches the def line and the call, but NOT `helperx`.
        assert_eq!(
            out["count"].as_u64().unwrap(),
            2,
            "should match the def line and the call, not helperx"
        );
        // No language server wired here, so the text scan is the answer and says so.
        assert_eq!(out["backend"], "text");
        // The recorded column is what a language server would be handed to
        // resolve the symbol. It must index the RAW line — the call site is
        // indented four spaces, so a column derived from the trimmed display
        // text would land before the identifier and resolve nothing.
        let call = out["references"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["line"] == 3)
            .expect("the indented call site");
        assert_eq!(call["col"], 4, "column must index the untrimmed line");
        let refs = out["references"].as_array().unwrap();
        assert!(
            refs.iter()
                .all(|r| r["text"].as_str().unwrap().contains("helper"))
        );
    }

    #[tokio::test]
    async fn tree_lists_nested_structure() {
        let dir = std::env::temp_dir().join(format!("medha-tree-{}", ulid_like()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("README.md"), "# hi").unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let tool = Tree { sbx };
        let out = tool.execute(&json!({ "depth": 2 })).await.unwrap();
        let tree = out["tree"].as_str().unwrap();
        assert!(tree.contains("src/"), "tree: {tree}");
        assert!(tree.contains("main.rs"));
        assert!(tree.contains("README.md"));
    }

    #[tokio::test]
    async fn registry_executes_fs_write_then_read() {
        let dir = std::env::temp_dir().join(format!("medha-tools-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let reg = ToolRegistry::with_workspace(sbx, mem_artifacts());

        // specs are exposed and sorted
        let names: Vec<String> = reg.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"fs.write".to_string()));

        let write = ToolIntent {
            id: "1".into(),
            tool: "fs.write".into(),
            args: json!({ "path": "x.txt", "content": "hi" }),
        };
        let obs = reg.execute(&write).await;
        assert_eq!(obs.status, kernel::ObsStatus::Ok);

        let read = ToolIntent {
            id: "2".into(),
            tool: "fs.read".into(),
            args: json!({ "path": "x.txt" }),
        };
        let obs = reg.execute(&read).await;
        assert_eq!(obs.payload["content"], "hi");
    }

    #[tokio::test]
    async fn lsp_registration_exposes_tools_and_annotates_rust_writes() {
        let dir = std::env::temp_dir().join(format!("medha-lsp-tools-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let mut reg = ToolRegistry::with_workspace(sbx, mem_artifacts());
        reg.register_lsp(Arc::new(lsp::LspManager::new(
            dir,
            lsp::Config {
                enabled: false,
                ..lsp::Config::default()
            },
        )));

        let names: Vec<String> = reg.specs().into_iter().map(|spec| spec.name).collect();
        assert!(names.contains(&"lsp.status".to_string()));
        assert!(names.contains(&"lsp.start".to_string()));
        assert!(names.contains(&"lsp.diagnostics".to_string()));
        assert!(names.contains(&"lsp.definition".to_string()));
        assert!(names.contains(&"lsp.references".to_string()));
        assert!(names.contains(&"lsp.hover".to_string()));
        assert!(names.contains(&"lsp.symbols".to_string()));

        let observation = reg
            .execute(&ToolIntent {
                id: "lsp-write".into(),
                tool: "fs.write".into(),
                args: json!({ "path": "main.rs", "content": "fn main() {}" }),
            })
            .await;
        assert_eq!(observation.status, kernel::ObsStatus::Ok);
        assert_eq!(observation.payload["lsp"]["status"], "unavailable");
    }

    /// End-to-end proof that `shell.exec`, routed through a native-backed
    /// workspace, is actually confined: an in-workspace write succeeds, a write
    /// to $HOME is blocked by the OS jail.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn shell_exec_is_jailed_under_native_backend() {
        let dir = std::env::temp_dir().join(format!("medha-shelljail-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let backend = sandbox::select_backend(
            &sandbox::SandboxConfig {
                backend: sandbox::BackendKind::Native,
                net: sandbox::NetPolicy::Allow,
                ..Default::default()
            },
            vec![],
        );
        let sbx = Arc::new(
            WorkspaceSandbox::new_jailed(&dir)
                .unwrap()
                .with_exec_backend(backend),
        );
        let reg = ToolRegistry::with_workspace(sbx, mem_artifacts());

        let inside = reg
            .execute(&ToolIntent {
                id: "1".into(),
                tool: "shell.exec".into(),
                args: json!({ "command": "touch inside.txt" }),
            })
            .await;
        assert_eq!(
            inside.payload["exit_code"].as_i64(),
            Some(0),
            "in-workspace write should succeed"
        );
        assert!(dir.join("inside.txt").exists());

        let marker = format!(".medha-shelljail-escape-{}", ulid_like());
        let outside = reg
            .execute(&ToolIntent {
                id: "2".into(),
                tool: "shell.exec".into(),
                args: json!({ "command": format!("touch \"$HOME/{marker}\"") }),
            })
            .await;
        assert_ne!(
            outside.payload["exit_code"].as_i64(),
            Some(0),
            "shell write to HOME must be blocked"
        );
        let home = std::env::var("HOME").unwrap();
        assert!(
            !std::path::Path::new(&home).join(&marker).exists(),
            "escape file must not exist"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn update_plan_echoes_steps_and_counts_done() {
        let dir = std::env::temp_dir().join(format!("medha-tools-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let reg = ToolRegistry::with_workspace(sbx, mem_artifacts());
        let obs = reg
            .execute(&ToolIntent {
                id: "p".into(),
                tool: "update_plan".into(),
                args: json!({ "steps": [
                    { "title": "read files", "status": "done" },
                    { "title": "write page", "status": "in_progress" },
                    { "title": "verify", "status": "pending" }
                ] }),
            })
            .await;
        assert_eq!(obs.status, kernel::ObsStatus::Ok);
        assert_eq!(obs.payload["total"], 3);
        assert_eq!(obs.payload["done"], 1);
        assert_eq!(obs.payload["steps"][1]["status"], "in_progress");
    }

    #[tokio::test]
    async fn update_plan_enforces_single_active_and_keeps_explanation() {
        let dir = std::env::temp_dir().join(format!("medha-plan-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let reg = ToolRegistry::with_workspace(sbx, mem_artifacts());
        let obs = reg
            .execute(&ToolIntent {
                id: "p".into(),
                tool: "update_plan".into(),
                args: json!({ "explanation": "starting the write", "steps": [
                    { "title": "a", "status": "in_progress" },
                    { "title": "b", "status": "in_progress" },
                    { "title": "c", "status": "pending" }
                ] }),
            })
            .await;
        assert_eq!(obs.payload["steps"][0]["status"], "in_progress");
        assert_eq!(
            obs.payload["steps"][1]["status"], "pending",
            "a second active step is demoted"
        );
        assert_eq!(obs.payload["explanation"], "starting the write");
    }

    #[test]
    fn extract_pdf_text_smoke() {
        // Set MEDHA_TEST_PDF=/path/to.pdf to exercise against a real file.
        let Ok(path) = std::env::var("MEDHA_TEST_PDF") else {
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        let out = extract_pdf_text(&bytes);
        assert!(!out.is_empty());
        assert!(
            !out.starts_with("[web.fetch: could not"),
            "extraction errored: {out}"
        );
        eprintln!(
            "extracted {} chars; head: {:?}",
            out.len(),
            &out[..out.len().min(120)]
        );
    }

    #[test]
    fn html_to_markdown_converts_and_caps_large_input() {
        let md = html_to_markdown("<h1>Title</h1><p>Hello <b>world</b></p>");
        assert!(md.contains("Title"), "converts basic HTML");
        // Oversized input is truncated before the recursive parser sees it.
        let big = format!("<p>{}</p>", "x".repeat(6 * 1024 * 1024));
        let out = html_to_markdown(&big);
        assert!(out.len() <= 5 * 1024 * 1024, "large input is capped");
    }

    #[tokio::test]
    async fn unknown_tool_is_denied() {
        let dir = std::env::temp_dir().join(format!("medha-tools-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let reg = ToolRegistry::with_workspace(sbx, mem_artifacts());
        let obs = reg
            .execute(&ToolIntent {
                id: "9".into(),
                tool: "nope".into(),
                args: json!({}),
            })
            .await;
        assert_eq!(obs.status, kernel::ObsStatus::Denied);
    }

    #[test]
    fn ddg_lite_snippets_pair_structurally_not_by_index() {
        // Result 1 has NO snippet row — its absence must not shift result 2's
        // snippet onto it (the old index-zip did exactly that).
        let html = r#"<table>
            <tr><td><a class="result-link" href="https://one.example/">One</a></td></tr>
            <tr><td><a class="result-link" href="https://two.example/">Two</a></td></tr>
            <tr><td class="result-snippet">snippet for two</td></tr>
        </table>"#;
        let out = parse_ddg_lite(html, 10);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["title"], "One");
        assert_eq!(out[0]["snippet"], "", "missing snippet must stay missing");
        assert_eq!(out[1]["title"], "Two");
        assert_eq!(
            out[1]["snippet"], "snippet for two",
            "snippet stays with ITS result"
        );
    }

    #[tokio::test]
    async fn read_artifact_pages_snap_to_char_boundaries() {
        let dir = std::env::temp_dir().join(format!("medha-tools-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let store = mem_artifacts();
        let reg = ToolRegistry::with_workspace(sbx, store.clone());
        // "héllo wörld" repeated — page length 5 cuts mid-'é' (2 bytes).
        let text = "héllo wörld ".repeat(10);
        let hash = store.put(text.as_bytes()).unwrap();

        let page1 = reg
            .execute(&ToolIntent {
                id: "1".into(),
                tool: "read_artifact".into(),
                args: json!({ "hash": hash, "offset": 0, "length": 2 }),
            })
            .await;
        let c1 = page1.payload["content"].as_str().unwrap();
        assert!(
            !c1.contains('\u{FFFD}'),
            "no replacement char at a cut page edge: {c1:?}"
        );
        assert_eq!(c1, "h", "the split 'é' is dropped, not mangled");
        // Continue from next_offset: the dropped bytes re-appear.
        let next = page1.payload["next_offset"].as_u64().unwrap();
        assert_eq!(next, 1, "resume where the complete chars ended");
        let page2 = reg
            .execute(&ToolIntent {
                id: "2".into(),
                tool: "read_artifact".into(),
                args: json!({ "hash": hash, "offset": next, "length": 4 }),
            })
            .await;
        let c2 = page2.payload["content"].as_str().unwrap();
        assert!(
            !c2.contains('\u{FFFD}'),
            "leading continuation bytes snapped: {c2:?}"
        );
        assert_eq!(c2, "éll");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn fs_read_refuses_whole_read_of_a_huge_file_but_allows_ranges() {
        let dir = std::env::temp_dir().join(format!("medha-tools-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let reg = ToolRegistry::with_workspace(sbx, mem_artifacts());
        // 3MB file, over the 2MB whole-read cap.
        std::fs::write(dir.join("big.txt"), "line\n".repeat(600_000)).unwrap();

        let whole = reg
            .execute(&ToolIntent {
                id: "1".into(),
                tool: "fs.read".into(),
                args: json!({ "path": "big.txt" }),
            })
            .await;
        assert_eq!(
            whole.status,
            kernel::ObsStatus::Error,
            "whole read must refuse"
        );
        assert!(
            whole.payload.to_string().contains("offset"),
            "error points at ranged reads"
        );

        let ranged = reg
            .execute(&ToolIntent {
                id: "2".into(),
                tool: "fs.read".into(),
                args: json!({ "path": "big.txt", "offset": 1, "limit": 3 }),
            })
            .await;
        assert_eq!(
            ranged.status,
            kernel::ObsStatus::Ok,
            "ranged read still works"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn edit_refuses_when_file_changed_after_preview() {
        let dir = std::env::temp_dir().join(format!("medha-tools-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let reg = ToolRegistry::with_workspace(sbx.clone(), mem_artifacts());

        sbx.write("p.txt", "alpha\nbeta\n").await.unwrap();
        let intent = ToolIntent {
            id: "1".into(),
            tool: "fs.edit".into(),
            args: json!({ "path": "p.txt", "old_string": "beta", "new_string": "BETA" }),
        };
        // Gate flow: preview pins the content, then the file changes underneath.
        assert!(reg.preview(&intent).await.is_some());
        sbx.write("p.txt", "alpha\nbeta\nnew line sneaked in\n")
            .await
            .unwrap();
        let obs = reg.execute(&intent).await;
        assert_eq!(
            obs.status,
            kernel::ObsStatus::Error,
            "stale preview must refuse"
        );
        assert!(
            obs.payload
                .to_string()
                .contains("changed after the approved preview")
        );

        // Re-running (fresh preview against current content) succeeds.
        let intent2 = ToolIntent {
            id: "2".into(),
            ..intent.clone()
        };
        assert!(reg.preview(&intent2).await.is_some());
        let obs2 = reg.execute(&intent2).await;
        assert_eq!(obs2.status, kernel::ObsStatus::Ok);

        // multi_edit gets the same guard.
        let mintent = ToolIntent {
            id: "3".into(),
            tool: "multi_edit".into(),
            args: json!({ "path": "p.txt", "edits": [{ "old_string": "alpha", "new_string": "ALPHA" }] }),
        };
        assert!(reg.preview(&mintent).await.is_some());
        sbx.write("p.txt", "changed\nalpha\n").await.unwrap();
        let mobs = reg.execute(&mintent).await;
        assert_eq!(
            mobs.status,
            kernel::ObsStatus::Error,
            "stale multi_edit preview must refuse"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn fs_edit_diffs_and_grep_finds() {
        let dir = std::env::temp_dir().join(format!("medha-tools-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let reg = ToolRegistry::with_workspace(sbx, mem_artifacts());

        reg.execute(&ToolIntent {
            id: "1".into(),
            tool: "fs.write".into(),
            args: json!({ "path": "f.txt", "content": "alpha\nbeta\ngamma\n" }),
        })
        .await;

        // edit: replace a unique line, expect a diff with -/+ lines
        let edit = reg
            .execute(&ToolIntent {
                id: "2".into(),
                tool: "fs.edit".into(),
                args: json!({ "path": "f.txt", "old_string": "beta", "new_string": "BETA" }),
            })
            .await;
        assert_eq!(edit.status, kernel::ObsStatus::Ok);
        let diff = edit.payload["diff"].as_str().unwrap();
        assert!(diff.contains("-beta"), "diff shows removal");
        assert!(diff.contains("+BETA"), "diff shows insertion");

        // grep finds the edited content
        let g = reg
            .execute(&ToolIntent {
                id: "3".into(),
                tool: "grep".into(),
                args: json!({ "pattern": "BETA", "path": "." }),
            })
            .await;
        assert_eq!(g.status, kernel::ObsStatus::Ok);
        assert!(g.payload["count"].as_u64().unwrap() >= 1);

        // edit with a non-unique match is rejected (no replace_all)
        let bad = reg
            .execute(&ToolIntent {
                id: "4".into(),
                tool: "fs.edit".into(),
                args: json!({ "path": "f.txt", "old_string": "a", "new_string": "x" }),
            })
            .await;
        assert_eq!(bad.status, kernel::ObsStatus::Error);
    }

    #[test]
    fn multi_edit_applies_in_order_and_is_atomic() {
        let src = "one two three\n";
        // Sequential edits, each seeing the previous result.
        let ok = apply_edits(
            src,
            &[
                json!({ "old_string": "one", "new_string": "1" }),
                json!({ "old_string": "1 two", "new_string": "1-2" }),
            ],
        )
        .unwrap();
        assert_eq!(ok, "1-2 three\n");
        // A missing old_string aborts the WHOLE batch (nothing applied).
        let err = apply_edits(
            src,
            &[
                json!({ "old_string": "one", "new_string": "1" }),
                json!({ "old_string": "nope", "new_string": "x" }),
            ],
        );
        assert!(
            err.is_err(),
            "batch must fail atomically on a missing match"
        );
        // Ambiguous match without replace_all is rejected.
        assert!(apply_edits("a a a", &[json!({ "old_string": "a", "new_string": "b" })]).is_err());
        assert_eq!(
            apply_edits(
                "a a a",
                &[json!({ "old_string": "a", "new_string": "b", "replace_all": true })]
            )
            .unwrap(),
            "b b b"
        );
    }

    #[test]
    fn safe_git_arg_rejects_flag_like_values() {
        assert!(safe_git_arg("HEAD~3").is_ok());
        assert!(safe_git_arg("src/main.rs").is_ok());
        assert!(safe_git_arg("--upload-pack=evil").is_err());
        assert!(safe_git_arg("-x").is_err());
    }

    #[test]
    fn cargo_diagnostics_parses_compiler_messages() {
        // One compiler-message line + one unrelated line (build-script/artifact).
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"x"}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[{"is_primary":true,"file_name":"src/lib.rs","line_start":42,"column_start":9}]}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"note","message":"just a note","spans":[]}}"#,
        );
        let diags = cargo_diagnostics(stdout, "");
        assert_eq!(
            diags.len(),
            1,
            "keeps errors/warnings, drops notes and artifacts"
        );
        assert_eq!(diags[0]["severity"], "error");
        assert_eq!(diags[0]["file"], "src/lib.rs");
        assert_eq!(diags[0]["line"], 42);
        assert_eq!(diags[0]["code"], "E0308");
    }

    #[test]
    fn tsc_and_ruff_parsers_extract_structured_diagnostics() {
        // tsc --pretty false
        let tsc = "src/app.ts(12,5): error TS2322: Type 'string' is not assignable to type 'number'.\nnot a diagnostic line";
        let d = tsc_diagnostics(tsc, "");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0]["file"], "src/app.ts");
        assert_eq!(d[0]["line"], 12);
        assert_eq!(d[0]["column"], 5);
        assert_eq!(d[0]["code"], "TS2322");
        assert_eq!(d[0]["severity"], "error");

        // ruff --output-format=json
        let ruff = r#"[{"filename":"m.py","location":{"row":3,"column":1},"code":"F401","message":"unused import"}]"#;
        let r = ruff_diagnostics(ruff, "");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["file"], "m.py");
        assert_eq!(r[0]["line"], 3);
        assert_eq!(r[0]["code"], "F401");
        assert_eq!(r[0]["severity"], "warning");
        // Non-JSON (e.g. ruff not installed / crashed) → no diagnostics, no panic.
        assert!(ruff_diagnostics("error: ruff not found", "").is_empty());
    }

    #[test]
    fn go_parsers_split_build_errors_from_vet_warnings() {
        // go build → stderr, severity error; skips `# package` header lines.
        let build = "# example.com/m\n./main.go:10:6: undefined: Foo\n";
        let b = gobuild_diagnostics("", build);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0]["severity"], "error");
        assert_eq!(b[0]["file"], "./main.go");
        assert_eq!(b[0]["line"], 10);
        assert_eq!(b[0]["column"], 6);

        // go vet → stderr, severity warning; column optional.
        let vet = "./main.go:7: Printf format %d has arg s of wrong type string\n";
        let v = govet_diagnostics("", vet);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["severity"], "warning");
        assert_eq!(v[0]["line"], 7);
    }

    #[test]
    fn eslint_and_cppcheck_and_java_parsers() {
        // eslint --format json: severity 2=error, 1=warning.
        let eslint = r#"[{"filePath":"/a/app.js","messages":[{"line":3,"column":5,"severity":2,"ruleId":"no-undef","message":"'x' is not defined"},{"line":9,"column":1,"severity":1,"ruleId":"semi","message":"Missing semicolon"}]}]"#;
        let e = eslint_diagnostics(eslint, "");
        assert_eq!(e.len(), 2);
        assert_eq!(e[0]["severity"], "error");
        assert_eq!(e[0]["file"], "/a/app.js");
        assert_eq!(e[0]["code"], "no-undef");
        assert_eq!(e[1]["severity"], "warning");

        // cppcheck template on stderr: file:line:col:severity:id:message
        let cpp = "src/x.cpp:12:3:error:nullPointer:Null pointer dereference\nsrc/x.cpp:20:1:style:unusedVariable:Unused variable 'k'\n";
        let c = cppcheck_diagnostics("", cpp);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["severity"], "error");
        assert_eq!(c[0]["line"], 12);
        assert_eq!(c[0]["code"], "nullPointer");
        assert_eq!(c[1]["severity"], "warning"); // style → warning

        // java: Maven [ERROR] path:[l,c] and javac path:l: error:
        let mvn = "[ERROR] /s/App.java:[8,17] cannot find symbol";
        let javac = "/s/App.java:15: error: ';' expected";
        let j = java_diagnostics(&format!("{mvn}\n{javac}"), "");
        assert_eq!(j.len(), 2);
        assert_eq!(j[0]["file"], "/s/App.java");
        assert_eq!(j[0]["line"], 8);
        assert_eq!(j[0]["column"], 17);
        assert_eq!(j[1]["line"], 15);
        assert_eq!(j[1]["severity"], "error");
    }

    #[tokio::test]
    async fn multi_edit_writes_once_or_not_at_all() {
        let dir = std::env::temp_dir().join(format!("medha-tools-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let reg = ToolRegistry::with_workspace(sbx, mem_artifacts());
        reg.execute(&ToolIntent {
            id: "1".into(),
            tool: "fs.write".into(),
            args: json!({ "path": "f.txt", "content": "alpha\nbeta\n" }),
        })
        .await;
        // A batch where the second edit can't match must leave the file untouched.
        let bad = reg
            .execute(&ToolIntent {
                id: "2".into(),
                tool: "multi_edit".into(),
                args: json!({ "path": "f.txt", "edits": [
                    { "old_string": "alpha", "new_string": "ALPHA" },
                    { "old_string": "nonexistent", "new_string": "x" }
                ] }),
            })
            .await;
        assert_eq!(bad.status, kernel::ObsStatus::Error);
        let after = reg
            .execute(&ToolIntent {
                id: "3".into(),
                tool: "fs.read".into(),
                args: json!({ "path": "f.txt" }),
            })
            .await;
        assert!(
            after.payload["content"].as_str().unwrap().contains("alpha"),
            "must not be partially edited"
        );
    }

    #[tokio::test]
    async fn preview_renders_a_real_diff_without_writing() {
        let dir = std::env::temp_dir().join(format!("medha-tools-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let reg = ToolRegistry::with_workspace(sbx, mem_artifacts());

        reg.execute(&ToolIntent {
            id: "1".into(),
            tool: "fs.write".into(),
            args: json!({ "path": "f.txt", "content": "alpha\nbeta\ngamma\n" }),
        })
        .await;

        // fs.edit preview shows the change and does NOT modify the file.
        let prev = reg
            .preview(&ToolIntent {
                id: "2".into(),
                tool: "fs.edit".into(),
                args: json!({ "path": "f.txt", "old_string": "beta", "new_string": "BETA" }),
            })
            .await
            .expect("edit preview should render a diff");
        assert!(
            prev.contains("-beta") && prev.contains("+BETA"),
            "preview is a real diff: {prev}"
        );
        // The file is untouched (preview is side-effect-free).
        let after = reg
            .execute(&ToolIntent {
                id: "3".into(),
                tool: "fs.read".into(),
                args: json!({ "path": "f.txt" }),
            })
            .await;
        assert!(after.payload["content"].as_str().unwrap().contains("beta"));

        // A brand-new file previews as all-additions.
        let np = reg
            .preview(&ToolIntent {
                id: "4".into(),
                tool: "fs.write".into(),
                args: json!({ "path": "new.txt", "content": "hello\n" }),
            })
            .await
            .expect("write preview should render additions");
        assert!(
            np.contains("+hello"),
            "new-file preview shows additions: {np}"
        );

        // An edit whose old_string is absent previews the failure, not a diff.
        let miss = reg
            .preview(&ToolIntent {
                id: "5".into(),
                tool: "fs.edit".into(),
                args: json!({ "path": "f.txt", "old_string": "nope", "new_string": "x" }),
            })
            .await
            .expect("should explain the miss");
        assert!(miss.contains("not found"), "preview flags the miss: {miss}");
    }

    #[tokio::test]
    async fn word_count_counts_lines_words_chars() {
        let dir = std::env::temp_dir().join(format!("medha-tools-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        let reg = ToolRegistry::with_workspace(sbx, mem_artifacts());

        // Write a test file
        reg.execute(&ToolIntent {
            id: "1".into(),
            tool: "fs.write".into(),
            args: json!({ "path": "test.txt", "content": "hello world\nthis is a test\nthree lines here" }),
        })
        .await;

        // word_count should return correct counts
        let wc = reg
            .execute(&ToolIntent {
                id: "2".into(),
                tool: "word_count".into(),
                args: json!({ "path": "test.txt" }),
            })
            .await;
        assert_eq!(wc.status, kernel::ObsStatus::Ok);
        assert_eq!(wc.payload["lines"], 3);
        assert_eq!(wc.payload["words"], 9); // "hello", "world", "this", "is", "a", "test", "three", "lines", "here"
        assert_eq!(wc.payload["chars"], 43); // exact char count including newlines
        assert_eq!(wc.payload["path"], "test.txt");
    }

    // tiny unique-ish suffix without pulling ulid into dev-deps
    fn ulid_like() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    // Minimal in-memory artifact store for tests (read_artifact isn't exercised).
    #[derive(Default)]
    struct MemArtifacts(std::sync::Mutex<HashMap<String, Vec<u8>>>);
    impl kernel::ArtifactStore for MemArtifacts {
        fn put(&self, bytes: &[u8]) -> Result<String, String> {
            let key = format!("{}", bytes.len());
            self.0.lock().unwrap().insert(key.clone(), bytes.to_vec());
            Ok(key)
        }
        fn get(&self, hash: &str, offset: usize, len: Option<usize>) -> Result<Vec<u8>, String> {
            let map = self.0.lock().unwrap();
            let data = map.get(hash).ok_or("not found")?;
            let start = offset.min(data.len());
            let end = len
                .map(|l| (start + l).min(data.len()))
                .unwrap_or(data.len());
            Ok(data[start..end].to_vec())
        }
        fn size(&self, hash: &str) -> Result<usize, String> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(hash)
                .map(|d| d.len())
                .unwrap_or(0))
        }
    }
    fn mem_artifacts() -> Arc<dyn kernel::ArtifactStore> {
        Arc::new(MemArtifacts::default())
    }

    #[test]
    fn truncated_lsp_results_spill_losslessly_to_artifacts() {
        let artifacts = mem_artifacts();
        let location = |line| lsp::Location {
            path: std::path::PathBuf::from("src/lib.rs"),
            range: lsp::Range {
                start: lsp::Position { line, character: 0 },
                end: lsp::Position { line, character: 1 },
            },
        };
        let value = lsp_query_value(
            lsp::QueryReport::Ready {
                server: "fake".into(),
                root: std::path::PathBuf::from("."),
                sources: vec!["fake".into()],
                warnings: Vec::new(),
                items: vec![location(0)],
                overflow: vec![location(1)],
                total: 2,
                truncated: true,
            },
            &artifacts,
        )
        .unwrap();
        let hash = value["artifact_hash"].as_str().expect("artifact hash");
        assert!(artifacts.size(hash).unwrap() > 0);
        assert_eq!(value["items"].as_array().unwrap().len(), 1);
        assert!(value.get("overflow").is_none());
    }

    /// A registry over a fresh jailed workspace, for exercising tools end-to-end.
    fn reg_in(dir: &std::path::Path) -> ToolRegistry {
        std::fs::create_dir_all(dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(dir).unwrap());
        ToolRegistry::with_workspace(sbx, mem_artifacts())
    }
    async fn run(reg: &ToolRegistry, tool: &str, args: Value) -> kernel::Observation {
        reg.execute(&ToolIntent {
            id: "t".into(),
            tool: tool.into(),
            args,
        })
        .await
    }

    // ── P0-1: ranged reads keep raw bytes (CRLF + trailing newline) so an edit
    //         built from a ranged read matches byte-for-byte ───────────────────
    #[tokio::test]
    async fn ranged_read_preserves_crlf_then_edit_roundtrips() {
        let dir = std::env::temp_dir().join(format!("medha-crlf-{}", ulid_like()));
        let reg = reg_in(&dir);
        run(
            &reg,
            "fs.write",
            json!({ "path": "f.txt", "content": "one\r\ntwo\r\nthree\r\n" }),
        )
        .await;

        // A ranged read returns the slice with its CRLF terminator intact.
        let r = run(
            &reg,
            "fs.read",
            json!({ "path": "f.txt", "offset": 2, "limit": 1 }),
        )
        .await;
        assert_eq!(r.status, kernel::ObsStatus::Ok);
        assert_eq!(
            r.payload["content"], "two\r\n",
            "CRLF preserved (was LF-normalized)"
        );

        // The model copies that exact slice into an edit — it must match.
        let e = run(
            &reg,
            "fs.edit",
            json!({
                "path": "f.txt", "old_string": "two\r\n", "new_string": "TWO\r\n"
            }),
        )
        .await;
        assert_eq!(
            e.status,
            kernel::ObsStatus::Ok,
            "edit failed: {:?}",
            e.payload
        );
        assert_eq!(
            run(&reg, "fs.read", json!({ "path": "f.txt" }))
                .await
                .payload["content"],
            "one\r\nTWO\r\nthree\r\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn edit_keeps_crlf_when_new_string_is_lf_only() {
        // Exact-match path: old_string matches the CRLF file verbatim, but the
        // model supplied an LF-only new_string. The write must not leave mixed
        // endings — new is normalized to the file's CRLF.
        let dir = std::env::temp_dir().join(format!("medha-crlf3-{}", ulid_like()));
        let reg = reg_in(&dir);
        run(
            &reg,
            "fs.write",
            json!({ "path": "f.txt", "content": "a\r\nb\r\nc\r\n" }),
        )
        .await;
        let e = run(
            &reg,
            "fs.edit",
            json!({
                "path": "f.txt", "old_string": "b\r\n", "new_string": "X\nY\n"
            }),
        )
        .await;
        assert_eq!(e.status, kernel::ObsStatus::Ok, "{:?}", e.payload);
        let out = run(&reg, "fs.read", json!({ "path": "f.txt" }))
            .await
            .payload["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            !out.contains("X\nY") || out.contains("X\r\nY"),
            "no bare LF introduced: {out:?}"
        );
        assert_eq!(out, "a\r\nX\r\nY\r\nc\r\n", "new_string normalized to CRLF");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn edit_is_crlf_tolerant_for_lf_only_needle() {
        let dir = std::env::temp_dir().join(format!("medha-crlf2-{}", ulid_like()));
        let reg = reg_in(&dir);
        run(
            &reg,
            "fs.write",
            json!({ "path": "f.txt", "content": "one\r\ntwo\r\n" }),
        )
        .await;
        // Needle uses LF though the file is CRLF — resolve_edit should still match.
        let e = run(
            &reg,
            "fs.edit",
            json!({
                "path": "f.txt", "old_string": "one\ntwo", "new_string": "1\n2"
            }),
        )
        .await;
        assert_eq!(
            e.status,
            kernel::ObsStatus::Ok,
            "CRLF-tolerant match failed: {:?}",
            e.payload
        );
        assert_eq!(
            run(&reg, "fs.read", json!({ "path": "f.txt" }))
                .await
                .payload["content"],
            "1\r\n2\r\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── P2: a huge limit must not overflow into an empty slice ────────────────
    #[tokio::test]
    async fn ranged_read_huge_limit_does_not_overflow() {
        let dir = std::env::temp_dir().join(format!("medha-ovf-{}", ulid_like()));
        let reg = reg_in(&dir);
        run(
            &reg,
            "fs.write",
            json!({ "path": "f.txt", "content": "a\nb\nc\n" }),
        )
        .await;
        let r = run(
            &reg,
            "fs.read",
            json!({ "path": "f.txt", "offset": 2, "limit": u64::MAX }),
        )
        .await;
        assert_eq!(
            r.payload["content"], "b\nc\n",
            "saturating end, not wrapped-empty"
        );
        // Offset past EOF is flagged, not silently empty.
        let past = run(
            &reg,
            "fs.read",
            json!({ "path": "f.txt", "offset": 99, "limit": 1 }),
        )
        .await;
        assert_eq!(past.payload["content"], "");
        assert!(
            past.payload["note"]
                .as_str()
                .unwrap_or("")
                .contains("past end")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── P0-5: empty / no-op old_string must fail, not corrupt the file ────────
    #[tokio::test]
    async fn edit_rejects_empty_and_noop_old_string() {
        let dir = std::env::temp_dir().join(format!("medha-empty-{}", ulid_like()));
        let reg = reg_in(&dir);
        run(
            &reg,
            "fs.write",
            json!({ "path": "f.txt", "content": "abc" }),
        )
        .await;

        let empty = run(
            &reg,
            "fs.edit",
            json!({
                "path": "f.txt", "old_string": "", "new_string": "X", "replace_all": true
            }),
        )
        .await;
        assert_eq!(
            empty.status,
            kernel::ObsStatus::Error,
            "empty old_string must be rejected"
        );
        // File untouched.
        assert_eq!(
            run(&reg, "fs.read", json!({ "path": "f.txt" }))
                .await
                .payload["content"],
            "abc"
        );

        let noop = run(
            &reg,
            "fs.edit",
            json!({
                "path": "f.txt", "old_string": "abc", "new_string": "abc"
            }),
        )
        .await;
        assert_eq!(
            noop.status,
            kernel::ObsStatus::Error,
            "old==new must be rejected"
        );

        // multi_edit guards the same way.
        let me = run(
            &reg,
            "multi_edit",
            json!({
                "path": "f.txt", "edits": [{ "old_string": "", "new_string": "Y" }]
            }),
        )
        .await;
        assert_eq!(me.status, kernel::ObsStatus::Error);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── P0-3 / §2: background-task facility ───────────────────────────────────
    #[tokio::test]
    async fn shell_fast_command_completes_inline() {
        let dir = std::env::temp_dir().join(format!("medha-sh-fast-{}", ulid_like()));
        let reg = reg_in(&dir);
        let obs = run(&reg, "shell.exec", json!({ "command": "printf hi" })).await;
        assert_eq!(obs.status, kernel::ObsStatus::Ok);
        assert_eq!(obs.payload["exit_code"].as_i64(), Some(0));
        assert_eq!(obs.payload["stdout"], "hi");
        assert!(
            obs.payload.get("task_id").is_none(),
            "fast command should not promote"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn shell_background_promotes_and_is_pollable_and_killable() {
        let dir = std::env::temp_dir().join(format!("medha-sh-bg-{}", ulid_like()));
        let reg = reg_in(&dir);
        // background:true → returns a task id immediately, keeps running.
        let started = run(
            &reg,
            "shell.exec",
            json!({
                "command": "sleep 30; echo done", "background": true
            }),
        )
        .await;
        assert_eq!(started.payload["status"], "running");
        let id = started.payload["task_id"]
            .as_str()
            .expect("task id")
            .to_string();

        // task.output reports it running.
        let poll = run(&reg, "task.output", json!({ "task_id": id })).await;
        assert_eq!(poll.payload["status"], "running", "{:?}", poll.payload);

        // task.list shows it.
        let list = run(&reg, "task.list", json!({})).await;
        assert!(
            list.payload["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["task_id"] == id.as_str())
        );

        // The executor exposes it to surfaces (what the TUI status line polls).
        {
            use kernel::Executor;
            let live = reg.background_tasks();
            assert!(
                live.iter().any(|t| t.id == id && t.running),
                "executor should report the running task"
            );
        }

        // task.kill stops it.
        let killed = run(&reg, "task.kill", json!({ "task_id": id })).await;
        assert_eq!(killed.payload["killed"], true);

        // Give the reaper a moment; the task is no longer running.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let after = run(&reg, "task.output", json!({ "task_id": id })).await;
        assert_eq!(
            after.payload["status"], "exited",
            "killed task should no longer run"
        );

        // Unknown task ids are errors, not silent success.
        assert_eq!(
            run(&reg, "task.kill", json!({ "task_id": "nope" }))
                .await
                .status,
            kernel::ObsStatus::Error
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // (The wait-based promote path — `wait_until` timing out then registering —
    // is the same registration code as the deterministic `background: true` case
    // above; a real-clock variant flakes under heavy parallel test load, so it's
    // intentionally not a separate test.)

    #[tokio::test]
    async fn cancelling_shell_exec_mid_wait_still_tracks_the_task() {
        // If the caller cancels (Esc) during the foreground window, the shell.exec
        // future is dropped mid-wait. Because the task is registered BEFORE the
        // wait, it must remain tracked (pollable/killable/reaped) — not leak an
        // invisible process group.
        let dir = std::env::temp_dir().join(format!("medha-sh-cancel-{}", ulid_like()));
        let reg = Arc::new(reg_in(&dir));
        // Long foreground window so the call is definitely mid-wait when dropped.
        let fut = run(
            &reg,
            "shell.exec",
            json!({ "command": "sleep 30", "timeout_s": 30 }),
        );
        // Drop the future partway through the wait — this is what a cancel does.
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), fut).await;

        // The task is still tracked (register-before-wait), not leaked.
        let list = run(&reg, "task.list", json!({})).await;
        let tasks = list.payload["tasks"].as_array().unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "cancelled shell.exec must stay tracked: {:?}",
            list.payload
        );
        let id = tasks[0]["task_id"].as_str().unwrap().to_string();
        // And it's reapable via the table (also what session-end kill_all does).
        assert_eq!(
            run(&reg, "task.kill", json!({ "task_id": id }))
                .await
                .payload["killed"],
            true
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── K7 / P1-10: long-running tools get a wider ceiling than the 60s default ──
    #[test]
    fn per_tool_timeouts_exceed_the_default_for_long_runners() {
        let default = TOOL_TIMEOUT;
        // A quick local tool keeps the default 60s.
        assert_eq!(WordCount { sbx: mk_sbx() }.timeout(), Some(default));
        // shell.exec self-manages (promotes to background) → no outer cap.
        assert_eq!(
            ShellExec {
                sbx: mk_sbx(),
                tasks: Arc::new(TaskTable::default())
            }
            .timeout(),
            None
        );
        // A long-running fixed tool widens its ceiling past the default.
        assert!(
            Diagnostics { sbx: mk_sbx() }.timeout().unwrap() > default,
            "diagnostics must exceed 60s"
        );
        assert_eq!(
            Diagnostics { sbx: mk_sbx() }.blast_radius(),
            BlastRadius::IrreversibleLocal,
            "project build scripts must never bypass the human gate as a read"
        );
    }

    fn mk_sbx() -> Arc<WorkspaceSandbox> {
        let dir = std::env::temp_dir().join(format!("medha-to-{}", ulid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap())
    }

    // ── P0-4: concurrent same-file edits must not clobber each other ──────────
    #[tokio::test]
    async fn concurrent_edits_to_one_file_both_apply() {
        let dir = std::env::temp_dir().join(format!("medha-p04-{}", ulid_like()));
        let reg = Arc::new(reg_in(&dir));
        run(
            &reg,
            "fs.write",
            json!({ "path": "f.txt", "content": "alpha beta" }),
        )
        .await;

        // Fire two edits to the SAME file at once. Without per-path serialization
        // both read "alpha beta" and last-write-wins drops one; the lock forces
        // the second to see the first's result, so both land.
        let (a, b) = (reg.clone(), reg.clone());
        let e1 = tokio::spawn(async move {
            run(
                &a,
                "fs.edit",
                json!({ "path": "f.txt", "old_string": "alpha", "new_string": "A" }),
            )
            .await
        });
        let e2 = tokio::spawn(async move {
            run(
                &b,
                "fs.edit",
                json!({ "path": "f.txt", "old_string": "beta", "new_string": "B" }),
            )
            .await
        });
        let (r1, r2) = (e1.await.unwrap(), e2.await.unwrap());
        assert_eq!(r1.status, kernel::ObsStatus::Ok);
        assert_eq!(r2.status, kernel::ObsStatus::Ok);

        // Final file reflects BOTH edits (order-independent).
        let out = run(&reg, "fs.read", json!({ "path": "f.txt" }))
            .await
            .payload["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(out, "A B", "both edits applied, neither lost: {out:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── P1-7: `*` must not cross `/` ──────────────────────────────────────────
    #[tokio::test]
    async fn glob_star_does_not_cross_slash() {
        let dir = std::env::temp_dir().join(format!("medha-glob-{}", ulid_like()));
        let reg = reg_in(&dir);
        run(&reg, "fs.write", json!({ "path": "top.rs", "content": "" })).await;
        run(
            &reg,
            "fs.write",
            json!({ "path": "src/main.rs", "content": "" }),
        )
        .await;

        let shallow = run(&reg, "glob", json!({ "pattern": "*.rs" })).await;
        let m = shallow.payload["matches"].as_array().unwrap();
        assert!(m.iter().any(|v| v == "top.rs"), "matches top-level");
        assert!(
            !m.iter().any(|v| v == "src/main.rs"),
            "* must not descend into src/"
        );

        let deep = run(&reg, "glob", json!({ "pattern": "**/*.rs" })).await;
        let m = deep.payload["matches"].as_array().unwrap();
        assert!(m.iter().any(|v| v == "src/main.rs"), "** spans directories");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn skill_registry_exposes_a_consistent_skill_capability_set() {
        use kernel::Executor;

        let dir = std::env::temp_dir().join(format!("medha-skills-reg-{}", ulid_like()));
        let project = dir.join("project");
        std::fs::create_dir_all(project.join("nested-loader")).unwrap();
        std::fs::write(
            project.join("nested-loader").join("SKILL.md"),
            "---\nname = \"nested-loader\"\ndescription = \"Loads another skill\"\nrequired_tools = [\"skill.load\"]\n---\nbody",
        )
        .unwrap();

        let store = Arc::new(SkillStore::new(project, Some(dir.join("user"))));
        let mut reg = ToolRegistry::new();
        reg.register_skills(store);
        let names = reg.tool_names();
        assert!(
            names.contains("skill.load")
                && names.contains("skill.save")
                && names.contains("skill.list")
        );

        let obs = reg
            .execute(&ToolIntent {
                id: "load".into(),
                tool: "skill.load".into(),
                args: json!({ "name": "nested-loader" }),
            })
            .await;
        assert_eq!(obs.status, kernel::ObsStatus::Ok);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── clarify tool ─────────────────────────────────────────────────────────
    #[test]
    fn clarify_parse_validates_bounds() {
        // Good input parses (2 questions, options within 2–5, recommended flag).
        let ok = json!({ "questions": [
            { "question": "Which DB?", "header": "DB", "options": [
                { "label": "Postgres", "recommended": true }, { "label": "SQLite" } ] },
            { "question": "Extras?", "multi_select": true, "options": [
                { "label": "Auth" }, { "label": "Cache" }, { "label": "Queue" } ] }
        ]});
        let qs = Clarify::parse_questions(&ok).expect("valid");
        assert_eq!(qs.len(), 2);
        assert!(qs[0].options[0].recommended, "recommended flag parsed");
        assert!(!qs[0].multi_select, "default single-select");
        assert!(qs[1].multi_select, "multi_select parsed");
        // Too many questions → rejected.
        let many = json!({ "questions": (0..5).map(|i| json!({
            "question": format!("q{i}"), "options": [{"label":"a"},{"label":"b"}] })).collect::<Vec<_>>() });
        assert!(Clarify::parse_questions(&many).is_err());
        // A question with <2 options → rejected (a non-choice).
        let thin = json!({ "questions": [ { "question": "x", "options": [ {"label":"only"} ] } ] });
        assert!(Clarify::parse_questions(&thin).is_err());
    }

    #[test]
    fn clarify_truncates_more_than_five_options() {
        // A model overshooting to 6 options must not fail the call — keep 5.
        let args = json!({ "questions": [ { "question": "Which?", "options": [
            {"label":"a"},{"label":"b"},{"label":"c"},{"label":"d"},{"label":"e"},{"label":"f"} ] } ]});
        let qs = Clarify::parse_questions(&args).expect("6 options truncates, not errors");
        assert_eq!(qs[0].options.len(), 5);
    }

    #[test]
    fn clarify_accepts_double_encoded_questions() {
        // Some models send `questions` as a JSON *string* of the array. Accept it.
        let inner =
            r#"[{"question":"Which DB?","options":[{"label":"Postgres"},{"label":"SQLite"}]}]"#;
        let args = json!({ "questions": inner });
        let qs = Clarify::parse_questions(&args).expect("stringified questions parse");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].options.len(), 2);
    }

    #[test]
    fn clarify_never_times_out() {
        // Regression: a human question must have NO deadline — with the default
        // 60s tool timeout it fired and the agent proceeded without an answer.
        let tool = Clarify {
            asker: Arc::new(Mutex::new(None)),
        };
        assert!(
            tool.timeout().is_none(),
            "clarify must wait indefinitely for the user"
        );
    }

    #[tokio::test]
    async fn clarify_without_an_asker_reports_skipped_never_blocks() {
        // Headless/no-surface: the tool must return promptly with skipped=true,
        // never hang waiting for an answer that can't come.
        let tool = Clarify {
            asker: Arc::new(Mutex::new(None)),
        };
        let out = tool
            .execute(&json!({ "questions": [
                { "question": "pick", "options": [{"label":"a"},{"label":"b"}] } ] }))
            .await
            .expect("ok");
        assert_eq!(out.get("skipped").and_then(Value::as_bool), Some(true));
    }

    /// A runner that reports what the child was actually given, so the wiring
    /// from tool arguments through to a narrowed executor is covered end to end.
    struct EchoRunner;

    #[async_trait]
    impl orchestrator::ChildRunner for EchoRunner {
        async fn run(
            &self,
            run: orchestrator::ChildRun,
        ) -> Result<orchestrator::ChildOutcome, String> {
            let tools: Vec<String> = run.executor.specs().into_iter().map(|s| s.name).collect();
            Ok(orchestrator::ChildOutcome {
                status: orchestrator::AgentStatus::Completed,
                summary: format!("saw {} tool(s): {}", tools.len(), tools.join(",")),
                turns: 1,
                tool_calls: 0,
                trust: kernel::TrustLabel::Tool,
            })
        }
    }

    fn registry_with_agents() -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::default();
        let control = Arc::new(orchestrator::AgentControl::new(
            Arc::new(EchoRunner),
            tokio_util::sync::CancellationToken::new(),
        ));
        registry.register_agents(control, 8);
        let parent = registry.agent_parent_handle();
        let registry = Arc::new(registry);
        // What `main` does once the kernel exists: children narrow from the
        // finished registry.
        *parent.lock().unwrap() = Some(registry.clone() as Arc<dyn kernel::Executor>);
        registry
    }

    #[tokio::test]
    async fn agent_spawn_is_exposed_and_delegates_read_only() {
        let registry = registry_with_agents();
        assert!(
            registry
                .specs()
                .iter()
                .any(|spec| spec.name == "agent.spawn"),
            "agent.spawn must reach the model"
        );

        let observation = registry
            .execute(&kernel::ToolIntent {
                id: "i1".into(),
                tool: "agent.spawn".into(),
                args: json!({ "objective": "survey the crate" }),
            })
            .await;
        assert_eq!(observation.status, kernel::ObsStatus::Ok);
        let summary = observation.payload["summary"].as_str().unwrap_or_default();
        // The child inherited the parent's registry but only its read-only half,
        // so a mutating tool must not appear in what it saw.
        assert!(
            !summary.contains("fs.write"),
            "child got a write tool: {summary}"
        );
        assert_eq!(observation.payload["status"], "completed");
    }

    #[tokio::test]
    async fn agent_spawn_without_an_objective_is_rejected() {
        let registry = registry_with_agents();
        let observation = registry
            .execute(&kernel::ToolIntent {
                id: "i1".into(),
                tool: "agent.spawn".into(),
                args: json!({ "objective": "  " }),
            })
            .await;
        assert_ne!(observation.status, kernel::ObsStatus::Ok);
    }
}
