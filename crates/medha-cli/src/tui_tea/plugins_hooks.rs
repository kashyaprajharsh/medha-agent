//! `/hooks`: add a project hook by answering two questions, without learning a
//! file format. The result is an ordinary script in `.medha/hooks/<event>/`.

use super::{Screen, review_hooks};
use crate::tui_tea::{Model, Picker, PickerKind};
use std::path::Path;

pub(super) const EVENTS: &[(&str, &str, bool)] = &[
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

pub(super) const TOOLS: &[(&str, &str)] = &[
    ("*", "All tools"),
    ("shell.exec", "Shell commands"),
    ("edit", "File edits"),
    ("read", "File reads"),
    ("web", "Web search and fetch"),
    ("mcp__*", "MCP server tools"),
];

pub(super) fn event_labels() -> Vec<String> {
    EVENTS
        .iter()
        .map(|(event, description, _)| format!("{event} — {description}"))
        .chain(std::iter::once(
            "📋 See installed hooks and plugins".to_string(),
        ))
        .collect()
}

pub(super) fn tool_labels() -> Vec<String> {
    TOOLS
        .iter()
        .map(|(_, label)| (*label).to_string())
        .collect()
}

pub(super) fn open(model: &mut Model) {
    model.picker = Some(Picker::new(PickerKind::Plugins(Screen::HookEvent)));
}

pub(super) fn choose_event(model: &mut Model, selected: usize) {
    match EVENTS.get(selected) {
        Some((event, _, true)) => {
            model.picker = Some(Picker::new(PickerKind::Plugins(Screen::HookTools(
                (*event).to_string(),
            ))));
        }
        Some((event, _, false)) => ask_for_command(model, event, "*"),
        None => super::open(model),
    }
}

pub(super) fn choose_tools(model: &mut Model, event: &str, selected: usize) {
    let matcher = TOOLS.get(selected).map_or("*", |(matcher, _)| matcher);
    ask_for_command(model, event, matcher);
}

fn ask_for_command(model: &mut Model, event: &str, matcher: &str) {
    model.picker = None;
    model.input = format!("/hooks add {event} {matcher} ");
    model.cursor = model.input.len();
    model.push_notice(
        "type a shell command, or paste the path to a script to copy in, then Enter — \
         exit 0 continues, exit 2 blocks and shows stderr",
    );
}

/// `/hooks`, or `/hooks add <event> <matcher> <command | script path>`.
pub(crate) fn command(model: &mut Model, args: &str) {
    let mut parts = args.splitn(4, ' ');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("add"), Some(event), Some(matcher), Some(body)) if !body.trim().is_empty() => {
            let workspace = model.restore.root().to_path_buf();
            match add(&workspace, event, matcher, body.trim()) {
                Ok(path) => {
                    model.push_notice(format!("added hook {}", path.display()));
                    review_hooks(model);
                }
                Err(error) => model.push_notice(format!("hooks: {error}")),
            }
        }
        (None | Some(""), ..) => open(model),
        _ => model.push_notice("usage: /hooks · /hooks add <event> <tools|*> <command or script>"),
    }
}

/// Writes the hook script; a copied script keeps its body and gains a header.
pub(super) fn add(
    workspace: &Path,
    event: &str,
    matcher: &str,
    body: &str,
) -> Result<std::path::PathBuf, String> {
    if !EVENTS.iter().any(|(name, _, _)| *name == event) {
        let names: Vec<&str> = EVENTS.iter().map(|(name, _, _)| *name).collect();
        return Err(format!(
            "'{event}' is not an event; use one of {}",
            names.join(", ")
        ));
    }
    let dir = workspace.join(".medha").join("hooks").join(event);
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
