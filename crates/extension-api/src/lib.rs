//! Stable, serializable contracts shared by Medha and out-of-process extensions.
//!
//! This crate deliberately contains no kernel, tool-registry, UI, or process
//! implementation. An extension can describe requested capability; only the host
//! can grant and enforce it.

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Component as PathComponent, Path};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const HOST_API_VERSION: u32 = 1;
pub const MAX_HOOK_RESULT_FIELD_BYTES: usize = 4 * 1024;
/// Added context is model-visible rule text, so it gets more room than a reason.
pub const MAX_HOOK_CONTEXT_BYTES: usize = 16 * 1024;
/// Replaced with the installed package directory in MCP commands.
pub const PLUGIN_ROOT_PLACEHOLDER: &str = "${PLUGIN_ROOT}";

pub fn is_version(value: &str) -> bool {
    Version::parse(value).is_ok()
}

pub fn url_host(value: &str) -> Option<String> {
    url::Url::parse(value).ok()?.host_str().map(str::to_string)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub medha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub permissions: RequestedPermissions,
    #[serde(default)]
    pub components: Vec<ExtensionComponent>,
}

impl Manifest {
    pub fn validate(&self, medha_version: &str) -> Result<(), ManifestError> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema(self.schema_version));
        }
        validate_qualified_id(&self.id).map_err(ManifestError::Id)?;
        if self.name.trim().is_empty() {
            return Err(ManifestError::EmptyName);
        }
        Version::parse(&self.version).map_err(|error| ManifestError::Version(error.to_string()))?;
        let requirement = VersionReq::parse(&self.medha)
            .map_err(|error| ManifestError::MedhaRequirement(error.to_string()))?;
        let host = Version::parse(medha_version)
            .map_err(|error| ManifestError::HostVersion(error.to_string()))?;
        if !requirement.matches(&host) {
            return Err(ManifestError::Incompatible {
                required: self.medha.clone(),
                actual: medha_version.to_string(),
            });
        }
        if self.components.is_empty() {
            return Err(ManifestError::NoComponents);
        }
        self.permissions.validate()?;
        let mut ids = HashSet::new();
        for component in &self.components {
            component.validate()?;
            if !ids.insert(component.id()) {
                return Err(ManifestError::DuplicateComponent(
                    component.id().to_string(),
                ));
            }
        }
        Ok(())
    }

    pub fn namespaced(&self, component: &ExtensionComponent) -> String {
        format!("{}/{}", self.id, component.id())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestedPermissions {
    /// Host names the component may ask the host to contact. This is a request,
    /// never a grant and never a place for URLs containing credentials.
    pub network_hosts: Vec<String>,
    /// Package-relative or host-defined scope expressions. Resolution and approval
    /// belong to the host because a manifest is untrusted input.
    pub read_paths: Vec<String>,
    pub write_paths: Vec<String>,
    /// Logical secret names. Values are always brokered as opaque handles.
    pub secrets: Vec<String>,
}

impl RequestedPermissions {
    fn validate(&self) -> Result<(), ManifestError> {
        validate_unique_nonempty("network host", &self.network_hosts)
            .map_err(ManifestError::Permission)?;
        validate_unique_nonempty("read path", &self.read_paths)
            .map_err(ManifestError::Permission)?;
        validate_unique_nonempty("write path", &self.write_paths)
            .map_err(ManifestError::Permission)?;
        validate_unique_nonempty("secret", &self.secrets).map_err(ManifestError::Permission)?;
        for host in &self.network_hosts {
            validate_network_host(host).map_err(ManifestError::Permission)?;
        }
        for (kind, paths) in [
            ("read path", &self.read_paths),
            ("write path", &self.write_paths),
        ] {
            if let Some(path) = paths.iter().find(|path| path.contains('\0')) {
                return Err(ManifestError::Permission(format!(
                    "{kind} contains NUL: {path:?}"
                )));
            }
        }
        for secret in &self.secrets {
            validate_component_id(secret).map_err(|error| {
                ManifestError::Permission(format!("secret '{secret}': {error}"))
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionComponent {
    Skill {
        id: String,
        path: String,
    },
    /// An operator-facing action. It is not a model tool and does not add a tool
    /// schema to provider requests.
    Action {
        id: String,
        title: String,
        description: String,
        prompt: String,
    },
    Mcp {
        id: String,
        #[serde(default)]
        command: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default)]
        allow_tools: Vec<String>,
        #[serde(default)]
        deny_tools: Vec<String>,
    },
    Hook {
        id: String,
        points: Vec<HookPoint>,
        entrypoint: ProcessEntrypoint,
        #[serde(default)]
        failure: HookFailureMode,
        #[serde(default = "default_hook_timeout_ms")]
        timeout_ms: u64,
        /// Canonical tool-name globs for tool points; empty matches every tool.
        /// A non-matching call never starts the process.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        matcher: Vec<String>,
        #[serde(default)]
        protocol: HookProtocol,
        #[serde(default)]
        workdir: HookWorkdir,
    },
}

/// How a hook process reports its decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookProtocol {
    /// A versioned [`HookEnvelope`] in, one [`HookResult`] out.
    #[default]
    Envelope,
    /// Exit status 0 continues and 2 blocks with stderr as the reason; stdout
    /// may carry optional JSON. The host adapts input and output at the edge.
    ExitStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookWorkdir {
    #[default]
    Package,
    /// The workspace root; the sandbox still admits only granted paths.
    Workspace,
}

impl HookPoint {
    /// Points whose payload names a tool, so a matcher can filter them.
    pub const fn is_tool_point(self) -> bool {
        matches!(self, Self::PreTool | Self::PostTool | Self::ToolFailure)
    }
}

impl ExtensionComponent {
    pub fn id(&self) -> &str {
        match self {
            Self::Skill { id, .. }
            | Self::Action { id, .. }
            | Self::Mcp { id, .. }
            | Self::Hook { id, .. } => id,
        }
    }

    fn validate(&self) -> Result<(), ManifestError> {
        validate_component_id(self.id()).map_err(|error| {
            ManifestError::Component(format!("component '{}': {error}", self.id()))
        })?;
        match self {
            Self::Skill { path, .. } => validate_relative_path(path),
            Self::Action {
                title,
                description,
                prompt,
                ..
            } => {
                for (field, value) in [
                    ("title", title.as_str()),
                    ("description", description.as_str()),
                    ("prompt", prompt.as_str()),
                ] {
                    if value.trim().is_empty() {
                        return Err(ManifestError::Component(format!(
                            "action '{}' has an empty {field}",
                            self.id()
                        )));
                    }
                }
                Ok(())
            }
            Self::Mcp {
                command,
                url,
                allow_tools,
                deny_tools,
                ..
            } => {
                let has_command = !command.is_empty();
                let has_url = url.as_deref().is_some_and(|value| !value.trim().is_empty());
                if has_command == has_url {
                    return Err(ManifestError::Component(format!(
                        "MCP component '{}' must declare exactly one of command or url",
                        self.id()
                    )));
                }
                if command
                    .iter()
                    .any(|part| part.is_empty() || part.contains('\0'))
                {
                    return Err(ManifestError::Component(format!(
                        "MCP component '{}' contains an empty command argument",
                        self.id()
                    )));
                }
                if let Some(program) = command.first() {
                    let packaged = program
                        .strip_prefix(PLUGIN_ROOT_PLACEHOLDER)
                        .and_then(|rest| rest.strip_prefix('/'));
                    let valid = match packaged {
                        Some(relative) => validate_relative_path(relative).is_ok(),
                        None => !program.contains(['/', '\\']),
                    };
                    if !valid {
                        return Err(ManifestError::Component(format!(
                            "MCP component '{}' program must be a command name found on PATH \
                             or {PLUGIN_ROOT_PLACEHOLDER}/<file in the package>",
                            self.id()
                        )));
                    }
                }
                if let Some(url) = url {
                    let parsed = url::Url::parse(url).map_err(|error| {
                        ManifestError::Component(format!(
                            "MCP component '{}' has an invalid URL: {error}",
                            self.id()
                        ))
                    })?;
                    if parsed.scheme() != "https"
                        || parsed.host_str().is_none()
                        || !parsed.username().is_empty()
                        || parsed.password().is_some()
                        || parsed.fragment().is_some()
                    {
                        return Err(ManifestError::Component(format!(
                            "MCP component '{}' URL must be credential-free HTTPS without a fragment",
                            self.id()
                        )));
                    }
                }
                validate_unique_nonempty("allowed MCP tool", allow_tools)
                    .map_err(ManifestError::Component)?;
                validate_unique_nonempty("denied MCP tool", deny_tools)
                    .map_err(ManifestError::Component)?;
                if let Some(tool) = allow_tools.iter().find(|tool| deny_tools.contains(tool)) {
                    return Err(ManifestError::Component(format!(
                        "MCP component '{}' both allows and denies tool '{tool}'",
                        self.id()
                    )));
                }
                Ok(())
            }
            Self::Hook {
                points,
                entrypoint,
                timeout_ms,
                matcher,
                ..
            } => {
                validate_unique_nonempty("hook matcher", matcher)
                    .map_err(ManifestError::Component)?;
                if !matcher.is_empty() && !points.iter().all(|point| point.is_tool_point()) {
                    return Err(ManifestError::Component(format!(
                        "hook '{}' has a matcher, which applies only to pre_tool, post_tool, \
                         and tool_failure",
                        self.id()
                    )));
                }
                if points.is_empty() {
                    return Err(ManifestError::Component(format!(
                        "hook '{}' has no hook points",
                        self.id()
                    )));
                }
                if let Some(point) = points.iter().find(|point| !point.is_available()) {
                    return Err(ManifestError::Component(format!(
                        "hook '{}' subscribes to '{}', which this Medha version does not run",
                        self.id(),
                        point.as_str()
                    )));
                }
                if points.iter().collect::<HashSet<_>>().len() != points.len() {
                    return Err(ManifestError::Component(format!(
                        "hook '{}' contains a duplicate hook point",
                        self.id()
                    )));
                }
                if !(1..=60_000).contains(timeout_ms) {
                    return Err(ManifestError::Component(format!(
                        "hook '{}' timeout must be between 1 and 60000 ms",
                        self.id()
                    )));
                }
                entrypoint.validate()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessEntrypoint {
    /// Package-relative executable, or a command line when `shell` is set.
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Run `program` as a command line through the platform shell. The sandbox,
    /// not command parsing, is what confines it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shell: bool,
}

impl ProcessEntrypoint {
    fn validate(&self) -> Result<(), ManifestError> {
        if self.shell {
            if self.program.trim().is_empty()
                || self.program.contains('\0')
                || !self.args.is_empty()
            {
                return Err(ManifestError::Component(
                    "a shell entrypoint is one non-empty command line without args".into(),
                ));
            }
            return Ok(());
        }
        validate_relative_path(&self.program)?;
        if self.args.iter().any(|arg| arg.contains('\0')) {
            return Err(ManifestError::Component(
                "entrypoint arguments may not contain NUL".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    SessionStart,
    SessionEnd,
    PromptSubmit,
    PreModel,
    PostModel,
    PreTool,
    PostTool,
    ToolFailure,
    ApprovalDecision,
    FileChange,
    PreCompaction,
    PostCompaction,
    AgentStart,
    AgentStop,
    JobStateChange,
    TaskCompletion,
}

impl HookPoint {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::PromptSubmit => "prompt_submit",
            Self::PreModel => "pre_model",
            Self::PostModel => "post_model",
            Self::PreTool => "pre_tool",
            Self::PostTool => "post_tool",
            Self::ToolFailure => "tool_failure",
            Self::ApprovalDecision => "approval_decision",
            Self::FileChange => "file_change",
            Self::PreCompaction => "pre_compaction",
            Self::PostCompaction => "post_compaction",
            Self::AgentStart => "agent_start",
            Self::AgentStop => "agent_stop",
            Self::JobStateChange => "job_state_change",
            Self::TaskCompletion => "task_completion",
        }
    }

    /// The decisions this host honors at the point. An empty list means the
    /// point has no call site yet, so a manifest may not subscribe to it.
    pub const fn decisions(self) -> &'static [HookDecision] {
        use HookDecision::{AddContext, Annotate, Continue, Deny, RequestApproval};
        match self {
            Self::PreTool => &[Continue, Deny, RequestApproval, Annotate],
            Self::PromptSubmit => &[Continue, Deny, AddContext],
            Self::PostTool | Self::ToolFailure => &[Continue, Annotate, AddContext],
            Self::SessionStart | Self::TaskCompletion => &[Continue, AddContext],
            Self::AgentStart => &[Continue, Annotate, AddContext],
            Self::PostCompaction | Self::AgentStop => &[Continue, Annotate],
            Self::SessionEnd
            | Self::PreModel
            | Self::PostModel
            | Self::ApprovalDecision
            | Self::FileChange
            | Self::PreCompaction
            | Self::JobStateChange => &[],
        }
    }

    pub const fn is_available(self) -> bool {
        !self.decisions().is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailureMode {
    FailClosed,
    #[default]
    Warn,
    Ignore,
}

const fn default_hook_timeout_ms() -> u64 {
    5_000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookEnvelope {
    pub api_version: u32,
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    #[serde(default)]
    pub depth: u8,
    pub plugin_id: String,
    pub component_id: String,
    pub point: HookPoint,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub deadline_unix_ms: u64,
    pub trust: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookResult {
    pub decision: HookDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

impl HookResult {
    pub fn validate_for(&self, point: HookPoint) -> Result<(), HookProtocolError> {
        let expected = match self.decision {
            HookDecision::Continue => None,
            HookDecision::Deny | HookDecision::RequestApproval => Some(("reason", &self.reason)),
            HookDecision::AddContext => Some(("context", &self.context)),
            HookDecision::Annotate => Some(("annotation", &self.annotation)),
            HookDecision::EnqueueAction => Some(("action", &self.action)),
        };
        for (name, value, limit) in [
            ("reason", &self.reason, MAX_HOOK_RESULT_FIELD_BYTES),
            ("context", &self.context, MAX_HOOK_CONTEXT_BYTES),
            ("annotation", &self.annotation, MAX_HOOK_RESULT_FIELD_BYTES),
            ("action", &self.action, MAX_HOOK_RESULT_FIELD_BYTES),
        ] {
            if value
                .as_ref()
                .is_some_and(|value| value.trim().is_empty() || value.len() > limit)
            {
                return Err(HookProtocolError::InvalidField(name));
            }
        }
        if let Some((name, value)) = expected
            && value.is_none()
        {
            return Err(HookProtocolError::MissingField(name));
        }
        let extra = match self.decision {
            HookDecision::Continue => {
                self.reason.is_some()
                    || self.context.is_some()
                    || self.annotation.is_some()
                    || self.action.is_some()
            }
            HookDecision::Deny | HookDecision::RequestApproval => {
                self.context.is_some() || self.annotation.is_some() || self.action.is_some()
            }
            HookDecision::AddContext => {
                self.reason.is_some() || self.annotation.is_some() || self.action.is_some()
            }
            HookDecision::Annotate => {
                self.reason.is_some() || self.context.is_some() || self.action.is_some()
            }
            HookDecision::EnqueueAction => {
                self.reason.is_some() || self.context.is_some() || self.annotation.is_some()
            }
        };
        if extra {
            return Err(HookProtocolError::UnexpectedField);
        }
        if !point.decisions().contains(&self.decision) {
            return Err(HookProtocolError::UnsupportedDecision {
                point,
                decision: self.decision,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookDecision {
    Continue,
    Deny,
    RequestApproval,
    AddContext,
    Annotate,
    EnqueueAction,
}

impl HookDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Deny => "deny",
            Self::RequestApproval => "request_approval",
            Self::AddContext => "add_context",
            Self::Annotate => "annotate",
            Self::EnqueueAction => "enqueue_action",
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HookProtocolError {
    #[error("hook result is missing required field '{0}'")]
    MissingField(&'static str),
    #[error("hook result field '{0}' is empty or exceeds the size limit")]
    InvalidField(&'static str),
    #[error("hook result contains a field that does not belong to its decision")]
    UnexpectedField,
    #[error("hook decision {decision:?} is not valid for {point:?}")]
    UnsupportedDecision {
        point: HookPoint,
        decision: HookDecision,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error(
        "unsupported manifest schema {0}; this Medha supports schema {MANIFEST_SCHEMA_VERSION}"
    )]
    UnsupportedSchema(u32),
    #[error("invalid plugin id: {0}")]
    Id(String),
    #[error("plugin name cannot be empty")]
    EmptyName,
    #[error("invalid plugin version: {0}")]
    Version(String),
    #[error("invalid Medha version requirement: {0}")]
    MedhaRequirement(String),
    #[error("invalid host Medha version: {0}")]
    HostVersion(String),
    #[error("plugin requires Medha {required}, but this host is {actual}")]
    Incompatible { required: String, actual: String },
    #[error("plugin must declare at least one component")]
    NoComponents,
    #[error("duplicate component id '{0}'")]
    DuplicateComponent(String),
    #[error("invalid component: {0}")]
    Component(String),
    #[error("invalid permission request: {0}")]
    Permission(String),
}

fn validate_qualified_id(value: &str) -> Result<(), String> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() < 2 {
        return Err("use a reverse-domain-style id such as 'org.example.plugin'".into());
    }
    for part in parts {
        validate_component_id(part)?;
    }
    Ok(())
}

fn validate_component_id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 64 {
        return Err("must contain 1 to 64 characters".into());
    }
    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err("must start with a lowercase letter or digit".into());
    }
    if !bytes[bytes.len() - 1].is_ascii_lowercase() && !bytes[bytes.len() - 1].is_ascii_digit() {
        return Err("must end with a lowercase letter or digit".into());
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return Err("may contain only lowercase letters, digits, and hyphens".into());
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<(), ManifestError> {
    if value.is_empty()
        || value
            .chars()
            .any(|character| matches!(character, '\0' | '\\'))
    {
        return Err(ManifestError::Component(
            "package path cannot be empty or contain NUL or backslashes".into(),
        ));
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, PathComponent::Normal(_) | PathComponent::CurDir))
    {
        return Err(ManifestError::Component(format!(
            "package path '{value}' must stay relative and cannot traverse parents"
        )));
    }
    Ok(())
}

fn validate_unique_nonempty(kind: &str, values: &[String]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for value in values {
        if value.trim().is_empty() {
            return Err(format!("{kind} cannot be empty"));
        }
        if !seen.insert(value) {
            return Err(format!("duplicate {kind} '{value}'"));
        }
    }
    Ok(())
}

fn validate_network_host(value: &str) -> Result<(), String> {
    let host = value.strip_prefix("*.").unwrap_or(value);
    if host.is_empty()
        || ["//", "/", "@"].iter().any(|part| host.contains(part))
        || host.chars().any(char::is_whitespace)
    {
        return Err(format!(
            "network host '{value}' must be a hostname, not a URL or credential"
        ));
    }
    let parsed = url::Url::parse(&format!("https://{host}"))
        .map_err(|error| format!("invalid network host '{value}': {error}"))?;
    if parsed.host_str().is_none()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(format!("invalid network host '{value}'"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            id: "dev.medha.example".into(),
            name: "Example".into(),
            version: "1.2.3".into(),
            medha: ">=0.1.0, <0.2.0".into(),
            description: None,
            permissions: RequestedPermissions::default(),
            components: vec![ExtensionComponent::Action {
                id: "review".into(),
                title: "Review".into(),
                description: "Review a change".into(),
                prompt: "Review the selected change.".into(),
            }],
        }
    }

    #[test]
    fn a_valid_manifest_namespaces_components_without_making_them_tools() {
        let manifest = manifest();
        manifest.validate("0.1.8").unwrap();
        assert_eq!(
            manifest.namespaced(&manifest.components[0]),
            "dev.medha.example/review"
        );
    }

    #[test]
    fn compatibility_and_identity_fail_closed() {
        let mut value = manifest();
        value.medha = ">=1.0.0".into();
        assert!(matches!(
            value.validate("0.1.8"),
            Err(ManifestError::Incompatible { .. })
        ));
        value = manifest();
        value.id = "Example".into();
        assert!(matches!(value.validate("0.1.8"), Err(ManifestError::Id(_))));
    }

    #[test]
    fn duplicate_components_and_traversing_paths_are_rejected() {
        let mut value = manifest();
        value.components.push(value.components[0].clone());
        assert_eq!(
            value.validate("0.1.8"),
            Err(ManifestError::DuplicateComponent("review".into()))
        );
        value = manifest();
        value.components = vec![ExtensionComponent::Skill {
            id: "bad".into(),
            path: "../outside".into(),
        }];
        assert!(matches!(
            value.validate("0.1.8"),
            Err(ManifestError::Component(_))
        ));
    }

    #[test]
    fn hook_entrypoints_are_argv_not_shell_strings() {
        let mut value = manifest();
        value.components = vec![ExtensionComponent::Hook {
            id: "audit".into(),
            points: vec![HookPoint::PreTool, HookPoint::PostTool],
            entrypoint: ProcessEntrypoint {
                program: "bin/audit".into(),
                args: vec!["--json".into()],
                shell: false,
            },
            failure: HookFailureMode::FailClosed,
            timeout_ms: 2_000,
            matcher: vec!["shell.*".into()],
            protocol: HookProtocol::Envelope,
            workdir: HookWorkdir::Package,
        }];
        value.validate("0.1.8").unwrap();
        if let ExtensionComponent::Hook { entrypoint, .. } = &value.components[0] {
            assert_eq!(entrypoint.program, "bin/audit");
        }
    }

    #[test]
    fn each_hook_point_accepts_only_the_decisions_it_runs() {
        let mut value = manifest();
        value.components = vec![ExtensionComponent::Hook {
            id: "future".into(),
            points: vec![HookPoint::FileChange],
            entrypoint: ProcessEntrypoint {
                program: "bin/hook".into(),
                args: Vec::new(),
                shell: false,
            },
            failure: HookFailureMode::Warn,
            timeout_ms: 1_000,
            matcher: Vec::new(),
            protocol: HookProtocol::Envelope,
            workdir: HookWorkdir::Package,
        }];
        let error = value.validate("0.1.8").unwrap_err().to_string();
        assert!(error.contains("does not run"), "{error}");

        let context = HookResult {
            decision: HookDecision::AddContext,
            reason: None,
            context: Some("tests are failing".into()),
            annotation: None,
            action: None,
        };
        context.validate_for(HookPoint::PromptSubmit).unwrap();
        context.validate_for(HookPoint::TaskCompletion).unwrap();
        assert!(context.validate_for(HookPoint::PreTool).is_err());
        let enqueue = HookResult {
            decision: HookDecision::EnqueueAction,
            reason: None,
            context: None,
            annotation: None,
            action: Some("later".into()),
        };
        assert!(
            [
                HookPoint::PromptSubmit,
                HookPoint::PostTool,
                HookPoint::AgentStop
            ]
            .iter()
            .all(|point| enqueue.validate_for(*point).is_err())
        );
    }

    #[test]
    fn mcp_programs_are_path_commands_or_package_files() {
        let with_command = |command: &[&str]| {
            let mut value = manifest();
            value.components = vec![ExtensionComponent::Mcp {
                id: "server".into(),
                command: command.iter().map(|part| part.to_string()).collect(),
                url: None,
                allow_tools: Vec::new(),
                deny_tools: Vec::new(),
            }];
            value.validate("0.1.8")
        };
        with_command(&["npx", "-y", "server"]).unwrap();
        with_command(&["${PLUGIN_ROOT}/bin/server", "--stdio"]).unwrap();
        for bad in [
            &["/bin/sh", "-c", "curl x | sh"][..],
            &["./server"],
            &["${PLUGIN_ROOT}/../outside"],
        ] {
            assert!(with_command(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn manifests_cannot_smuggle_urls_credentials_or_portable_path_traversal() {
        let mut value = manifest();
        value.permissions.network_hosts = vec!["https://user:secret@example.com".into()];
        assert!(matches!(
            value.validate("0.1.8"),
            Err(ManifestError::Permission(_))
        ));

        value = manifest();
        value.components = vec![ExtensionComponent::Mcp {
            id: "remote".into(),
            command: Vec::new(),
            url: Some("http://example.com/mcp".into()),
            allow_tools: Vec::new(),
            deny_tools: Vec::new(),
        }];
        assert!(matches!(
            value.validate("0.1.8"),
            Err(ManifestError::Component(_))
        ));

        value = manifest();
        value.components = vec![ExtensionComponent::Skill {
            id: "bad".into(),
            path: r"..\outside".into(),
        }];
        assert!(matches!(
            value.validate("0.1.8"),
            Err(ManifestError::Component(_))
        ));

        value = manifest();
        value.components = vec![ExtensionComponent::Mcp {
            id: "ambiguous".into(),
            command: vec!["server".into()],
            url: None,
            allow_tools: vec!["read".into()],
            deny_tools: vec!["read".into()],
        }];
        assert!(matches!(
            value.validate("0.1.8"),
            Err(ManifestError::Component(_))
        ));
    }

    #[test]
    fn hook_results_are_typed_and_point_specific() {
        let deny = HookResult {
            decision: HookDecision::Deny,
            reason: Some("blocked by policy".into()),
            context: None,
            annotation: None,
            action: None,
        };
        deny.validate_for(HookPoint::PreTool).unwrap();
        assert!(matches!(
            deny.validate_for(HookPoint::PostTool),
            Err(HookProtocolError::UnsupportedDecision { .. })
        ));

        let malformed = HookResult {
            decision: HookDecision::Continue,
            reason: Some("smuggled text".into()),
            context: None,
            annotation: None,
            action: None,
        };
        assert_eq!(
            malformed.validate_for(HookPoint::PreTool),
            Err(HookProtocolError::UnexpectedField)
        );
    }
}
