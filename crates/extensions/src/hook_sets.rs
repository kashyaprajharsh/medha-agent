//! Project and user hooks without writing a plugin.
//!
//! Drop a script into a folder named after its event, e.g.
//! `.medha/hooks/pre-tool/check.py`; a `hooks.toml` beside the folders is
//! optional and only needed for advanced settings. The whole folder is packaged
//! under a fixed id, so it is pinned, granted, and drift-checked like a plugin,
//! and starts disabled.

use crate::package::{Package, PackageSource, parse_toml, real_dir};
use crate::{Error, ExtensionComponent, HookPoint, Manifest, RequestedPermissions};
use medha_extension_api::{HookFailureMode, HookProtocol, HookWorkdir, ProcessEntrypoint};
use serde::Deserialize;
use std::path::Path;

pub const HOOKS_FILE: &str = "hooks.toml";
pub const PROJECT_HOOKS_ID: &str = "project.hooks";
pub const USER_HOOKS_ID: &str = "user.hooks";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const HEADER_LINES: usize = 5;
const HEADER_TAG: &str = "medha:";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSetFile {
    #[serde(default)]
    permissions: RequestedPermissions,
    #[serde(default)]
    hooks: Vec<toml::Table>,
}

/// The folder name for each event a hook script can subscribe to.
pub fn event_folders() -> Vec<(&'static str, HookPoint)> {
    [
        ("pre-tool", HookPoint::PreTool),
        ("post-tool", HookPoint::PostTool),
        ("tool-failure", HookPoint::ToolFailure),
        ("prompt-submit", HookPoint::PromptSubmit),
        ("session-start", HookPoint::SessionStart),
        ("task-completion", HookPoint::TaskCompletion),
        ("post-compaction", HookPoint::PostCompaction),
        ("agent-start", HookPoint::AgentStart),
        ("agent-stop", HookPoint::AgentStop),
    ]
    .into_iter()
    .filter(|(_, point)| point.is_available())
    .collect()
}

pub fn load(dir: &Path, id: &str, medha_version: &str) -> Result<Package, Error> {
    let dir = real_dir(dir)?;
    let mut permissions = RequestedPermissions::default();
    let mut components = Vec::new();
    let toml_path = dir.join(HOOKS_FILE);
    if toml_path.is_file() {
        let file: HookSetFile = parse_toml(&toml_path)?;
        permissions = file.permissions;
        for mut table in file.hooks {
            table.insert("kind".into(), toml::Value::String("hook".into()));
            components.push(
                toml::Value::Table(table)
                    .try_into::<ExtensionComponent>()
                    .map_err(|error| {
                        Error::Manifest(format!("{}: {error}", toml_path.display()))
                    })?,
            );
        }
    }
    let scripts = folder_hooks(&dir, &mut components)?;
    for host in scripts.network_hosts {
        if !permissions.network_hosts.contains(&host) {
            permissions.network_hosts.push(host);
        }
    }
    if scripts.any {
        for paths in [&mut permissions.read_paths, &mut permissions.write_paths] {
            if !paths.iter().any(|path| path == ".") {
                paths.push(".".into());
            }
        }
    }
    if components.is_empty() {
        return Err(Error::NotFound(id.to_string()));
    }
    let manifest = Manifest {
        schema_version: medha_extension_api::MANIFEST_SCHEMA_VERSION,
        id: id.into(),
        name: if id == USER_HOOKS_ID {
            "Your hooks".into()
        } else {
            "Project hooks".into()
        },
        version: "0.0.0".into(),
        medha: "*".into(),
        description: Some(format!("hooks in {}", dir.display())),
        permissions,
        components,
    };
    let tree = dir.clone();
    Package::assemble(
        dir,
        Some(&tree),
        manifest,
        medha_version,
        PackageSource::HookSet,
        &[],
    )
}

#[derive(Default)]
struct FolderScan {
    any: bool,
    network_hosts: Vec<String>,
}

fn folder_hooks(dir: &Path, components: &mut Vec<ExtensionComponent>) -> Result<FolderScan, Error> {
    let events = event_folders();
    let mut scan = FolderScan::default();
    let mut folders: Vec<_> = std::fs::read_dir(dir)
        .map_err(|error| crate::io_error("reading hooks folder", dir, error))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .collect();
    folders.sort_by_key(|entry| entry.file_name());
    for folder in folders {
        let name = folder.file_name().to_string_lossy().replace('_', "-");
        let Some((_, point)) = events.iter().find(|(event, _)| *event == name) else {
            let known: Vec<&str> = events.iter().map(|(event, _)| *event).collect();
            return Err(Error::Manifest(format!(
                "hooks folder '{name}' is not an event Medha runs; use one of: {}",
                known.join(", ")
            )));
        };
        let mut files: Vec<_> = std::fs::read_dir(folder.path())
            .map_err(|error| crate::io_error("reading hooks folder", &folder.path(), error))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && is_script(path))
            .collect();
        files.sort();
        for file in files {
            let relative = format!(
                "{name}/{}",
                file.file_name().unwrap_or_default().to_string_lossy()
            );
            let header = Header::read(&file)?;
            scan.any = true;
            scan.network_hosts.extend(header.network_hosts);
            components.push(ExtensionComponent::Hook {
                id: component_id(&relative),
                points: vec![*point],
                entrypoint: entrypoint(&file, &relative)?,
                failure: header.failure,
                timeout_ms: header.timeout_ms,
                matcher: if point.is_tool_point() {
                    header.matcher
                } else {
                    Vec::new()
                },
                protocol: HookProtocol::ExitStatus,
                workdir: HookWorkdir::Workspace,
            });
        }
    }
    Ok(scan)
}

fn is_script(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let lower = name.to_ascii_lowercase();
    !name.starts_with('.')
        && !lower.starts_with("readme")
        && !lower.ends_with(".md")
        && !lower.ends_with(".txt")
}

/// Executable files run directly; common script types run through their
/// interpreter so `chmod +x` is optional.
fn entrypoint(file: &Path, relative: &str) -> Result<ProcessEntrypoint, Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let executable = std::fs::metadata(file)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if executable {
            return Ok(ProcessEntrypoint {
                program: relative.into(),
                args: Vec::new(),
                shell: false,
            });
        }
    }
    let extension = file
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let interpreter = match extension.as_str() {
        "py" => "python3",
        "sh" => "sh",
        "bash" => "bash",
        "js" | "mjs" | "cjs" => "node",
        "rb" => "ruby",
        _ => {
            return Err(Error::Manifest(format!(
                "hook {relative} is neither executable nor a .py, .sh, .js, or .rb script"
            )));
        }
    };
    Ok(ProcessEntrypoint {
        program: format!("{interpreter} \"$MEDHA_PLUGIN_ROOT/{relative}\""),
        args: Vec::new(),
        shell: true,
    })
}

fn component_id(relative: &str) -> String {
    let stem = relative.rsplit_once('.').map_or(relative, |(stem, _)| stem);
    let mut id = String::new();
    for c in stem.chars() {
        let c = if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        if !(c == '-' && (id.is_empty() || id.ends_with('-'))) {
            id.push(c);
        }
    }
    id.trim_end_matches('-').to_string()
}

/// `# medha: matcher=shell.exec,edit timeout=10 failure=fail_closed network=api.github.com`
struct Header {
    matcher: Vec<String>,
    timeout_ms: u64,
    failure: HookFailureMode,
    network_hosts: Vec<String>,
}

impl Header {
    fn read(file: &Path) -> Result<Self, Error> {
        let mut header = Self {
            matcher: Vec::new(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            failure: HookFailureMode::Warn,
            network_hosts: Vec::new(),
        };
        let text = std::fs::read(file)
            .map_err(|error| crate::io_error("reading hook script", file, error))?;
        let text = String::from_utf8_lossy(&text);
        let Some(line) = text.lines().take(HEADER_LINES).find_map(|line| {
            let line = line.trim();
            let comment = line.strip_prefix("#").or_else(|| line.strip_prefix("//"))?;
            comment.trim().strip_prefix(HEADER_TAG).map(str::to_string)
        }) else {
            return Ok(header);
        };
        let invalid = |key: &str, value: &str| {
            Error::Manifest(format!(
                "{}: invalid medha header value {key}={value}",
                file.display()
            ))
        };
        let list = |value: &str| -> Vec<String> {
            value
                .split([',', '|'])
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect()
        };
        for pair in line.split_whitespace() {
            let (key, value) = pair.split_once('=').ok_or_else(|| invalid(pair, ""))?;
            match key {
                "matcher" => header.matcher = list(value),
                "timeout" => {
                    let seconds: u64 = value.parse().map_err(|_| invalid(key, value))?;
                    header.timeout_ms = seconds.saturating_mul(1_000).clamp(1, 60_000);
                }
                "failure" => {
                    header.failure = match value {
                        "fail_closed" | "block" => HookFailureMode::FailClosed,
                        "warn" => HookFailureMode::Warn,
                        "ignore" => HookFailureMode::Ignore,
                        _ => return Err(invalid(key, value)),
                    };
                }
                "network" if value == "true" => {
                    return Err(Error::Manifest(format!(
                        "{}: list the hosts the hook contacts, e.g. network=api.github.com",
                        file.display()
                    )));
                }
                "network" => header.network_hosts = list(value),
                _ => return Err(invalid(key, value)),
            }
        }
        Ok(header)
    }
}

#[cfg(test)]
#[path = "hook_sets_tests.rs"]
mod tests;
