//! Enabled plugin actions as `/` commands. Running one sends its prompt as the
//! user's message; the model never sees the list.

use extensions::Store;

const ARGUMENTS: &str = "$ARGUMENTS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginCommand {
    pub(crate) name: String,
    pub(crate) description: String,
    prompt: String,
}

/// `/<title>` unless a built-in or another plugin already uses it; then
/// `/<plugin>:<title>`, where `<plugin>` is the last part of the plugin id.
pub(crate) fn load(store: &Store, builtin: &[&str]) -> Vec<PluginCommand> {
    let actions = store.actions().unwrap_or_default();
    let short = |title: &str| format!("/{}", slug(title));
    let clashes = |name: &str| {
        builtin.contains(&name) || actions.iter().filter(|a| short(&a.title) == name).count() > 1
    };
    actions
        .iter()
        .filter(|action| !slug(&action.title).is_empty())
        .map(|action| {
            let plain = short(&action.title);
            let name = if clashes(&plain) {
                let plugin = action.plugin_id.rsplit('.').next().unwrap_or_default();
                format!("/{}:{}", slug(plugin), slug(&action.title))
            } else {
                plain
            };
            PluginCommand {
                name,
                description: format!("{} · {}", action.description, action.plugin_id),
                prompt: action.prompt.clone(),
            }
        })
        .collect()
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
