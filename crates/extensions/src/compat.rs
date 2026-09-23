//! The one adapter for existing third-party hook and plugin formats.
//!
//! Names in this file are file-format constants of those formats. Everything is
//! translated to Medha's own types here, so no other module depends on them.

use crate::package::{Package, PackageSource, read_text, real_dir};
use crate::{Error, ExtensionComponent, HookDecision, HookPoint, HookResult, Manifest};
use medha_extension_api::{
    HookFailureMode, HookProtocol, HookWorkdir, ProcessEntrypoint, RequestedPermissions,
};
use serde_json::{Value, json};
use std::path::Path;

#[path = "compat_plugin.rs"]
mod plugin;
pub(crate) use plugin::{apply_entry, entry_for, is_plugin, load_plugin, read_marketplace, slug};

/// Variables exit-status hooks in the existing format read. The config and data
/// folders point at the plugin's private writable folder, not `~/.claude`.
pub(crate) fn environment(workspace: &Path, root: &Path, data: &Path) -> Vec<(String, String)> {
    let path = |path: &Path| path.to_string_lossy().into_owned();
    vec![
        (PROJECT_DIR_ENV.into(), path(workspace)),
        ("CLAUDE_PLUGIN_ROOT".into(), path(root)),
        ("CLAUDE_PLUGIN_DATA".into(), path(data)),
        ("CLAUDE_CONFIG_DIR".into(), path(data)),
    ]
}

/// Public catalogs in the existing marketplace format that Discover starts with.
const DEFAULT_MARKETPLACES: &[&str] = &[
    "anthropics/claude-plugins-official",
    "dietrichgebert/ponytail",
];

pub(crate) fn default_marketplaces() -> Vec<crate::sources::GitSource> {
    DEFAULT_MARKETPLACES
        .iter()
        .filter_map(|spec| match crate::sources::parse(spec) {
            crate::sources::Spec::Git(source) => Some(source),
            _ => None,
        })
        .collect()
}

/// Settings folders whose `hooks` sections Medha can run: `<workspace>/.claude`
/// and `~/.claude`.
pub const SETTINGS_DIR: &str = ".claude";
pub const PROJECT_SETTINGS_HOOKS_ID: &str = "project.claude-hooks";
pub const USER_SETTINGS_HOOKS_ID: &str = "user.claude-hooks";
const SETTINGS_FILES: &[&str] = &["settings.json", "settings.local.json"];
const SCRIPTS_DIR: &str = "hooks";
const MAX_TIMEOUT_MS: u64 = 60_000;

/// Hooks from settings files become exit-status shell hooks that run in the
/// workspace. Only the `hooks` sections and the scripts folder are approved, so
/// unrelated settings edits do not revoke them. `None` when no hooks exist.
pub(crate) fn load_settings(
    dir: &Path,
    id: &str,
    medha_version: &str,
) -> Result<Option<Package>, Error> {
    let dir = real_dir(dir)?;
    let mut sections = Vec::new();
    for name in SETTINGS_FILES {
        let path = dir.join(name);
        if !path.is_file() {
            continue;
        }
        let settings: Value = serde_json::from_str(&read_text(&path)?)
            .map_err(|error| Error::Manifest(format!("{}: {error}", path.display())))?;
        if let Some(hooks) = settings.get("hooks").filter(|hooks| hooks.is_object()) {
            sections.push((*name, hooks.clone()));
        }
    }
    if sections.is_empty() {
        return Ok(None);
    }
    let mut components = Vec::new();
    let mut skipped = Vec::new();
    for (_, hooks) in &sections {
        convert_section(hooks, &mut components, &mut skipped);
    }
    let mut description = format!("hooks from {} settings", dir.display());
    if !skipped.is_empty() {
        skipped.sort();
        skipped.dedup();
        description.push_str(&format!("; not run by Medha: {}", skipped.join(", ")));
    }
    let manifest = Manifest {
        schema_version: medha_extension_api::MANIFEST_SCHEMA_VERSION,
        id: id.into(),
        name: "Hooks from settings".into(),
        version: "0.0.0".into(),
        medha: "*".into(),
        description: Some(description),
        permissions: RequestedPermissions {
            read_paths: vec![".".into()],
            write_paths: vec![".".into()],
            ..RequestedPermissions::default()
        },
        components,
    };
    let seed = serde_json::to_vec(&sections).unwrap_or_default();
    let scripts = dir.join(SCRIPTS_DIR);
    let tree = scripts.is_dir().then_some(scripts);
    Package::assemble(
        dir,
        tree.as_deref(),
        manifest,
        medha_version,
        PackageSource::Settings,
        &seed,
    )
    .map(Some)
}

fn convert_section(
    hooks: &Value,
    components: &mut Vec<ExtensionComponent>,
    skipped: &mut Vec<String>,
) {
    let Some(events) = hooks.as_object() else {
        return;
    };
    for (event, groups) in events {
        let Some(point) = point_for_event(event).filter(|point| point.is_available()) else {
            skipped.push(event.clone());
            continue;
        };
        for group in groups.as_array().into_iter().flatten() {
            let matcher = if point.is_tool_point() {
                matcher_patterns(group.get("matcher").and_then(Value::as_str).unwrap_or(""))
            } else {
                Vec::new()
            };
            for hook in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let command = hook.get("command").and_then(Value::as_str);
                match (hook.get("type").and_then(Value::as_str), command) {
                    (Some("command"), Some(command)) if !command.trim().is_empty() => {
                        let timeout_ms = hook
                            .get("timeout")
                            .and_then(Value::as_u64)
                            .map_or(MAX_TIMEOUT_MS, |seconds| {
                                seconds.saturating_mul(1_000).clamp(1, MAX_TIMEOUT_MS)
                            });
                        components.push(ExtensionComponent::Hook {
                            id: hook_id(point, command, components),
                            points: vec![point],
                            entrypoint: ProcessEntrypoint {
                                program: command.to_string(),
                                args: Vec::new(),
                                shell: true,
                            },
                            failure: HookFailureMode::Warn,
                            timeout_ms,
                            matcher: matcher.clone(),
                            protocol: HookProtocol::ExitStatus,
                            workdir: HookWorkdir::Workspace,
                        });
                    }
                    (kind, _) => {
                        skipped.push(format!("{event} ({} hook)", kind.unwrap_or("untyped")))
                    }
                }
            }
        }
    }
}

/// Named after the script the command runs, e.g. `session-start` for
/// `python3 hooks/session-start.py`; a number is added only on a clash.
fn hook_id(point: HookPoint, command: &str, components: &[ExtensionComponent]) -> String {
    let event = point.as_str().replace('_', "-");
    let script = command
        .split_whitespace()
        .rev()
        .map(|word| word.trim_matches(['"', '\'']))
        .find(|word| word.contains('/') || word.contains('.'))
        .and_then(|word| Path::new(word).file_stem())
        .map(|stem| plugin::slug(&stem.to_string_lossy()))
        .filter(|stem| !stem.is_empty());
    let base = match script {
        Some(script) if script.contains(&event) => script,
        Some(script) => format!("{event}-{script}"),
        None => event,
    };
    let taken = |id: &str| components.iter().any(|component| component.id() == id);
    let mut id = base.clone();
    let mut n = 2;
    while taken(&id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

fn point_for_event(event: &str) -> Option<HookPoint> {
    [
        HookPoint::PreTool,
        HookPoint::PostTool,
        HookPoint::ToolFailure,
        HookPoint::PromptSubmit,
        HookPoint::SessionStart,
        HookPoint::SessionEnd,
        HookPoint::TaskCompletion,
        HookPoint::PreCompaction,
        HookPoint::PostCompaction,
        HookPoint::AgentStart,
        HookPoint::AgentStop,
    ]
    .into_iter()
    .find(|point| event_name(*point) == event)
}

/// `Bash|Edit` alternatives and `.*` wildcards become Medha globs; empty or `*`
/// matches every tool.
fn matcher_patterns(matcher: &str) -> Vec<String> {
    let matcher = matcher.trim();
    if matcher.is_empty() || matcher == "*" || matcher == ".*" {
        return Vec::new();
    }
    let mut patterns: Vec<String> = matcher
        .split('|')
        .map(|part| part.trim().replace(".*", "*"))
        .filter(|part| !part.is_empty())
        .collect();
    patterns.sort();
    patterns.dedup();
    patterns
}

/// Environment variable that exit-status hooks written for the existing format
/// use to locate the project.
pub(crate) const PROJECT_DIR_ENV: &str = "CLAUDE_PROJECT_DIR";

/// Canonical Medha tool name ↔ the tool name those hooks match on.
const TOOL_NAMES: &[(&str, &str)] = &[
    ("shell.exec", "Bash"),
    ("read", "Read"),
    ("edit", "Edit"),
    ("edit", "Write"),
    ("edit", "MultiEdit"),
    ("glob", "Glob"),
    ("grep", "Grep"),
    ("ls", "LS"),
    ("web", "WebFetch"),
    ("update_plan", "TodoWrite"),
    ("agent_spawn", "Task"),
];

pub(crate) fn event_name(point: HookPoint) -> &'static str {
    match point {
        HookPoint::PreTool => "PreToolUse",
        HookPoint::PostTool => "PostToolUse",
        HookPoint::ToolFailure => "PostToolUseFailure",
        HookPoint::PromptSubmit => "UserPromptSubmit",
        HookPoint::SessionStart => "SessionStart",
        HookPoint::SessionEnd => "SessionEnd",
        HookPoint::TaskCompletion => "Stop",
        HookPoint::PreCompaction => "PreCompact",
        HookPoint::PostCompaction => "PostCompact",
        HookPoint::AgentStart => "SubagentStart",
        HookPoint::AgentStop => "SubagentStop",
        HookPoint::PreModel
        | HookPoint::PostModel
        | HookPoint::ApprovalDecision
        | HookPoint::FileChange
        | HookPoint::JobStateChange => point.as_str(),
    }
}

pub(crate) fn external_tool_name(canonical: &str) -> &str {
    TOOL_NAMES
        .iter()
        .find(|(medha, _)| *medha == canonical)
        .map_or(canonical, |(_, external)| external)
}

/// The existing format's tool names map back to canonical ones; unknown names
/// (MCP tools, globs) pass through unchanged.
pub(crate) fn canonical_tool_name(external: &str) -> &str {
    TOOL_NAMES
        .iter()
        .find(|(_, name)| *name == external)
        .map_or(external, |(medha, _)| medha)
}

pub(crate) fn encode_input(
    point: HookPoint,
    session_id: &str,
    workspace: &Path,
    payload: &Value,
) -> Value {
    let mut input = json!({
        "session_id": session_id,
        "hook_event_name": event_name(point),
        "cwd": workspace.display().to_string(),
    });
    if let Some(tool) = payload.get("tool").and_then(Value::as_str) {
        input["tool_name"] = json!(external_tool_name(tool));
        input["tool_input"] = payload.get("args").cloned().unwrap_or(json!({}));
        if let Some(result) = payload.get("result") {
            input["tool_response"] = result.clone();
        }
    }
    if let Some(prompt) = payload.get("prompt") {
        input["prompt"] = prompt.clone();
    }
    if let Some(source) = payload.get("source") {
        input["source"] = source.clone();
    }
    if point == HookPoint::TaskCompletion {
        input["stop_hook_active"] = json!(
            payload
                .get("continuations")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
        );
    }
    input
}

/// Exit 0 continues (stdout may hold JSON), 2 blocks with stderr as the reason,
/// anything else is a hook failure. `allow` can never widen Medha's policy.
pub(crate) fn decode_output(
    point: HookPoint,
    status: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<HookResult, String> {
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    match status {
        Some(2) => Ok(blocking(
            point,
            (!stderr.is_empty()).then_some(stderr),
            None,
        )),
        Some(0) => {
            let stdout = String::from_utf8_lossy(stdout);
            let stdout = stdout.trim();
            if stdout.is_empty() {
                return Ok(result(HookDecision::Continue));
            }
            match serde_json::from_str::<Value>(stdout) {
                Ok(output) if output.is_object() => decode_json(point, &output),
                _ => Ok(plain_text(point, stdout.to_string())),
            }
        }
        Some(code) => Err(format!("hook exited with status {code}: {stderr}")),
        None => Err("hook was terminated by a signal".into()),
    }
}

fn decode_json(point: HookPoint, output: &Value) -> Result<HookResult, String> {
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let specific = output.get("hookSpecificOutput");
    let reason = text(output.get("reason"))
        .or_else(|| text(specific.and_then(|value| value.get("permissionDecisionReason"))));
    let context = text(specific.and_then(|value| value.get("additionalContext")))
        .or_else(|| text(output.get("additionalContext")));
    let message = text(output.get("systemMessage"));
    if output.get("continue").and_then(Value::as_bool) == Some(false)
        || output.get("decision").and_then(Value::as_str) == Some("block")
    {
        let reason = reason.or_else(|| text(output.get("stopReason")));
        return Ok(blocking(point, reason, context));
    }
    match specific
        .and_then(|value| value.get("permissionDecision"))
        .and_then(Value::as_str)
    {
        Some("deny") => return Ok(blocking(point, reason, None)),
        Some("ask") if point == HookPoint::PreTool => {
            let mut result = result(HookDecision::RequestApproval);
            result.reason = Some(reason.unwrap_or_else(|| "a hook asked for review".into()));
            return Ok(result);
        }
        _ => {}
    }
    if let Some(context) = context
        && point.decisions().contains(&HookDecision::AddContext)
    {
        let mut result = result(HookDecision::AddContext);
        result.context = Some(context);
        return Ok(result);
    }
    Ok(match message {
        Some(message) => annotate(point, message),
        None => result(HookDecision::Continue),
    })
}

/// Plain stdout is model context for the start and prompt events, and an
/// operator note everywhere else.
fn plain_text(point: HookPoint, text: String) -> HookResult {
    if matches!(
        point,
        HookPoint::SessionStart | HookPoint::PromptSubmit | HookPoint::AgentStart
    ) {
        let mut result = result(HookDecision::AddContext);
        result.context = Some(text);
        result
    } else {
        annotate(point, text)
    }
}

/// Blocking means refusal before an action, and "keep working" feedback after one.
fn blocking(point: HookPoint, reason: Option<String>, context: Option<String>) -> HookResult {
    let text = reason
        .or(context)
        .unwrap_or_else(|| format!("blocked by a {} hook", point.as_str()));
    if point.decisions().contains(&HookDecision::Deny) {
        let mut result = result(HookDecision::Deny);
        result.reason = Some(text);
        result
    } else if point.decisions().contains(&HookDecision::AddContext) {
        let mut result = result(HookDecision::AddContext);
        result.context = Some(text);
        result
    } else {
        annotate(point, text)
    }
}

fn annotate(point: HookPoint, text: String) -> HookResult {
    if point.decisions().contains(&HookDecision::Annotate) {
        let mut result = result(HookDecision::Annotate);
        result.annotation = Some(text);
        result
    } else {
        result(HookDecision::Continue)
    }
}

fn result(decision: HookDecision) -> HookResult {
    HookResult {
        decision,
        reason: None,
        context: None,
        annotation: None,
        action: None,
    }
}

/// Test fixture: a one-skill plugin repository in the existing layout, listed
/// by its own marketplace index; `with_mcp` adds a server that needs network.
#[cfg(test)]
pub(crate) fn write_community_plugin(repo: &Path, version: &str, with_mcp: bool) {
    let write = |path: &str, text: String| {
        let path = repo.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        ".claude-plugin/plugin.json",
        format!(r#"{{"name": "tidy", "version": "{version}", "description": "Keeps code tidy"}}"#),
    );
    write(
        ".claude-plugin/marketplace.json",
        r#"{"name": "tidy-market", "plugins": [{"name": "tidy", "source": "./", "description": "Keeps code tidy"}]}"#.into(),
    );
    write(
        "skills/tidy/SKILL.md",
        format!("---\nname: tidy\ndescription: tidy up v{version}\n---\nTidy.\n"),
    );
    if with_mcp {
        write(
            ".mcp.json",
            r#"{"mcpServers": {"docs": {"command": "npx", "args": ["-y", "docs-server"]}}}"#.into(),
        );
    }
}

#[cfg(test)]
#[path = "compat_tests.rs"]
mod tests;
