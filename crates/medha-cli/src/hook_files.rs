//! Project hooks as ordinary scripts in `.medha/hooks/<event>/`, shared by the
//! TUI's `/hooks` and the desktop Hooks page. The folder is packaged as
//! `project.hooks`, so any new or changed script waits for review before it runs.

use std::path::{Path, PathBuf};

/// `(folder, what it does, whether it can filter by tool)`.
pub(crate) const EVENTS: &[(&str, &str, bool)] = &[
    ("pre-tool", "before a tool runs — can block it", true),
    (
        "post-tool",
        "after a tool runs — can add a note for the agent",
        true,
    ),
    ("tool-failure", "when a tool fails — can add a hint", true),
    (
        "prompt-submit",
        "when you send a message — can block it or add context",
        false,
    ),
    (
        "session-start",
        "when a session starts — can add context",
        false,
    ),
    (
        "task-completion",
        "when the agent says it is done — can send it back to work",
        false,
    ),
    (
        "agent-start",
        "when a sub-agent starts — can add context",
        false,
    ),
];

pub(crate) const TOOLS: &[(&str, &str)] = &[
    ("*", "All tools"),
    ("shell.exec", "Shell commands"),
    ("write,edit", "File changes"),
    ("read", "File reads"),
    ("web", "Web search and fetch"),
    ("mcp__*", "MCP server tools"),
];

fn hooks_dir(workspace: &Path, event: &str) -> Result<PathBuf, String> {
    if !EVENTS.iter().any(|(name, _, _)| *name == event) {
        let names: Vec<&str> = EVENTS.iter().map(|(name, _, _)| *name).collect();
        return Err(format!(
            "'{event}' is not an event; use one of {}",
            names.join(", ")
        ));
    }
    Ok(workspace.join(".medha").join("hooks").join(event))
}

/// Writes the hook script; a copied script keeps its body and gains a header.
pub(crate) fn add(
    workspace: &Path,
    event: &str,
    matcher: &str,
    body: &str,
) -> Result<PathBuf, String> {
    let dir = hooks_dir(workspace, event)?;
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let header = (matcher != "*").then(|| format!("# medha: matcher={matcher}\n"));
    let source = Path::new(body.trim_matches(['"', '\'']));
    let (name, text) = if source.is_file() {
        let text = std::fs::read_to_string(source).map_err(|error| error.to_string())?;
        let name = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or("the script path has no file name")?;
        let text = match (header, text.split_once('\n')) {
            (Some(header), Some((first, rest))) if first.starts_with("#!") => {
                format!("{first}\n{header}{rest}")
            }
            (Some(header), _) => format!("{header}{text}"),
            (None, _) => text,
        };
        (name, text)
    } else {
        let name: String = body
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join("-")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let name = format!("{}.sh", name.trim_matches('-'));
        (
            name,
            format!("#!/bin/sh\n{}{body}\n", header.unwrap_or_default()),
        )
    };
    let path = dir.join(&name);
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    std::fs::write(&path, text).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(path)
}

/// Deletes one script by its file name; nothing outside the event's folder,
/// and never through a link.
pub(crate) fn remove(workspace: &Path, event: &str, file: &str) -> Result<(), String> {
    if file.is_empty() || file.contains(['/', '\\']) || file == "." || file == ".." {
        return Err("Choose a hook script by its file name".into());
    }
    let path = hooks_dir(workspace, event)?.join(file);
    let meta = std::fs::symlink_metadata(&path).map_err(|_| format!("{file} is not a hook"))?;
    if !meta.is_file() {
        return Err(format!("{file} is not a hook script"));
    }
    std::fs::remove_file(&path).map_err(|error| error.to_string())
}

/// Every script in the project's hook folders, by event, with its tool filter.
pub(crate) fn list(workspace: &Path) -> Vec<serde_json::Value> {
    let mut rows = Vec::new();
    for (event, _, _) in EVENTS {
        let Ok(dir) = hooks_dir(workspace, event) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
                    && !path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
            })
            .collect();
        files.sort();
        for path in files {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let matcher = text
                .lines()
                .take(5)
                .find_map(|line| line.trim().strip_prefix("# medha: matcher="))
                .unwrap_or("*")
                .trim()
                .to_owned();
            let command = text
                .lines()
                .filter(|line| !line.starts_with("#!") && !line.trim().starts_with("# medha:"))
                .collect::<Vec<_>>()
                .join("\n");
            let preview: String = command.trim().chars().take(400).collect();
            rows.push(serde_json::json!({
                "event": event,
                "file": path.file_name().map(|name| name.to_string_lossy().into_owned()),
                "matcher": matcher,
                "command": preview,
            }));
        }
    }
    rows
}

#[cfg(test)]
#[path = "hook_files_tests.rs"]
mod tests;
