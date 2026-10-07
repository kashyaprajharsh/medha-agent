//! Enabled application actions as `/` commands. Running one sends its prompt as the
//! user's message; the model never sees the list.

use extensions::Store;
use std::collections::{HashMap, HashSet};
use std::path::Path;

const ARGUMENTS: &str = "$ARGUMENTS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginCommand {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) action_id: String,
    pub(crate) prompt: String,
}

/// Keep short names when unique; qualify collisions with the full plugin id.
pub(crate) fn load(store: &Store, builtin: &[&str]) -> Vec<PluginCommand> {
    let actions = store.actions().unwrap_or_default();
    let mut counts = HashMap::new();
    for action in &actions {
        *counts.entry(action_name(action)).or_insert(0usize) += 1;
    }
    let mut used = HashSet::new();
    actions
        .iter()
        .filter(|action| !action_name(action).is_empty())
        .map(|action| {
            let title = action_name(action);
            let short = format!("/{title}");
            let base = if counts[&title] > 1 || builtin.contains(&short.as_str()) {
                format!("/{}:{title}", slug(&action.plugin_id))
            } else {
                short
            };
            let mut name = base.clone();
            let mut suffix = 2;
            while builtin.contains(&name.as_str()) || !used.insert(name.clone()) {
                name = format!("{base}-{suffix}");
                suffix += 1;
            }
            PluginCommand {
                name,
                description: format!("{} · {}", action.description, action.plugin_id),
                action_id: action.id.clone(),
                prompt: action.prompt.clone(),
            }
        })
        .collect()
}

pub(crate) fn has_snippets(commands: &[PluginCommand], line: &str) -> bool {
    let name = line.split_whitespace().next().unwrap_or_default();
    commands
        .iter()
        .find(|command| command.name == name)
        .is_some_and(|command| command.prompt.contains("!`"))
}

/// Resolve a typed action through the extension host, which checks its current
/// approval and runs the plugin-authored snippets in the plugin sandbox.
pub(crate) async fn expand_with_snippets(
    commands: &[PluginCommand],
    line: &str,
    store: &Store,
    workspace: &Path,
) -> Result<Option<String>, String> {
    let line = line.trim();
    let (name, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let Some(command) = commands.iter().find(|command| command.name == name) else {
        return Ok(None);
    };
    if !command.prompt.contains("!`") {
        return Ok(expand(commands, line));
    }
    store
        .expand_action_snippets(&command.action_id, &command.prompt, args.trim(), workspace)
        .await
        .map(Some)
        .map_err(|error| error.to_string())
}

/// The message a typed line sends, when its first word is a plugin command.
pub(crate) fn expand(commands: &[PluginCommand], line: &str) -> Option<String> {
    let line = line.trim();
    let (name, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let command = commands.iter().find(|command| command.name == name)?;
    let args = args.trim();
    Some(if command.prompt.contains(ARGUMENTS) {
        command.prompt.replace(ARGUMENTS, args)
    } else if args.is_empty() {
        command.prompt.clone()
    } else {
        format!("{}\n\n{args}", command.prompt)
    })
}

fn action_name(action: &extensions::OperatorAction) -> String {
    let title = slug(&action.title);
    if title.is_empty() {
        slug(action.id.rsplit('/').next().unwrap_or(&action.id))
    } else {
        title
    }
}

fn slug(value: &str) -> String {
    let mut slug = String::new();
    for c in value.trim().chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        if !(c == '-' && (slug.is_empty() || slug.ends_with('-'))) {
            slug.push(c);
        }
    }
    slug.trim_end_matches('-').to_string()
}
