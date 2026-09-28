use crate::tool::{INPUT_LIMIT, bounded, canonical, clip, fields, plan, text};
use serde_json::Value;

/// The verb for a tool known only by name, as a live progress line has it.
pub fn tool_verb(tool: &str) -> String {
    verb(&canonical(tool, &Value::Null), &Value::Null)
}

/// How a tool call reads in the transcript: a verb, the one thing it acted on,
/// and, only when the line cannot say it all, the rest as plain text.
#[derive(Debug, Default, PartialEq)]
pub struct Call {
    pub verb: String,
    pub target: Option<String>,
    pub file_path: Option<String>,
    pub input_label: Option<&'static str>,
    pub input: Option<String>,
    pub plan: Option<Value>,
}

pub(crate) fn describe(name: &str, tool: &str, args: &Value) -> Call {
    let (input_label, input) = match input(name, args) {
        Some((label, text)) => (Some(label), Some(bounded(&text, INPUT_LIMIT))),
        None => (None, None),
    };
    Call {
        verb: verb(name, args),
        file_path: matches!(name, "read" | "edit" | "write" | "image.view")
            .then(|| {
                text(args, "path")
                    .or_else(|| text(args, "file_path"))
                    .map(str::to_owned)
            })
            .flatten(),
        target: target(name, args).map(|text| clip(&text, 240)),
        input_label,
        input,
        plan: plan(tool, args),
    }
}

fn verb(name: &str, args: &Value) -> String {
    let verb = match name {
        "read" => "Read",
        "ls" => "Listed",
        "glob" => "Found files",
        "grep" => "Searched",
        "code" if args.get("symbol").is_some() => "Found references",
        "code" => "Outlined",
        "edit" if args.get("old_string").is_none() && args.get("content").is_some() => "Wrote",
        "edit" => "Edited",
        "write" => "Wrote",
        "shell.exec" | "git" => "Ran",
        "web.search" => "Searched the web",
        "web.fetch" => "Fetched",
        "update_plan" => "Updated the plan",
        "agent.spawn" if args.get("tasks").is_some() => "Started agents",
        "agent.spawn" => "Started agent",
        "agent.wait" => "Waited for agents",
        "agent.list" => "Checked on agents",
        "agent.transcript" => "Read the work of",
        "agent.steer" => "Steered",
        "agent.message" => "Messaged",
        "agent.followup" => "Gave more work to",
        "agent.cancel" => "Stopped",
        "clarify" => "Asked you",
        "skill" | "skill.load" => "Loaded skill",
        "skill.save" => "Saved skill",
        "memory.search" => "Searched memory",
        "memory.write" => "Remembered",
        "memory.update" => "Updated memory",
        "memory.forget" => "Forgot",
        "sessions.search" => "Searched sessions",
        "diagnostics" => "Checked diagnostics",
        "image.view" => "Looked at",
        "mcp.status" => "Checked MCP servers",
        _ => return titled(name),
    };
    verb.to_owned()
}

fn titled(name: &str) -> String {
    let last = name.rsplit("__").next().unwrap_or(name);
    let segment = last
        .rsplit(['.', ':'])
        .next()
        .unwrap_or(last)
        .replace('_', " ");
    let mut chars = segment.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_else(|| name.to_owned())
}

fn target(name: &str, args: &Value) -> Option<String> {
    match name {
        "grep" => text(args, "pattern").map(|pattern| match text(args, "path") {
            Some(path) if path != "." => format!("“{pattern}” in {path}"),
            _ => format!("“{pattern}”"),
        }),
        "code" => match (text(args, "symbol"), text(args, "path")) {
            (Some(symbol), Some(path)) => Some(format!("{symbol} in {path}")),
            (symbol, path) => symbol.or(path).map(str::to_owned),
        },
        "ls" => Some(
            text(args, "path")
                .filter(|path| *path != ".")
                .unwrap_or("the workspace")
                .to_owned(),
        ),
        "git" => text(args, "subcommand").map(|command| format!("git {command}")),
        "agent.spawn" => Some(match args.get("tasks").and_then(Value::as_array) {
            Some(tasks) => tasks.iter().map(agent_name).collect::<Vec<_>>().join(", "),
            None => agent_name(args),
        }),
        "agent.wait" | "agent.list" | "update_plan" => None,
        "clarify" => args
            .get("questions")?
            .get(0)
            .and_then(|question| text(question, "question").or_else(|| text(question, "header")))
            .map(str::to_owned),
        _ => [
            "path",
            "file_path",
            "command",
            "pattern",
            "url",
            "query",
            "name",
        ]
        .iter()
        .find_map(|key| text(args, key))
        .map(str::to_owned)
        .or_else(|| text(args, "agent").map(|agent| agent.trim_start_matches('/').to_owned())),
    }
}

/// The name an agent is known by: the one it was given, or the first words of
/// its objective, as the orchestrator names an agent that was not named.
fn agent_name(args: &Value) -> String {
    if let Some(name) = text(args, "name") {
        return name.trim_start_matches('/').to_owned();
    }
    let slug: Vec<String> = text(args, "objective")
        .unwrap_or_default()
        .split_whitespace()
        .take(4)
        .map(|word| {
            word.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .map(|word| word.to_lowercase())
        .collect();
    if slug.is_empty() {
        "agent".into()
    } else {
        slug.join("-")
    }
}

fn input(name: &str, args: &Value) -> Option<(&'static str, String)> {
    match name {
        "shell.exec" => {
            let command = text(args, "command")?;
            (command.contains('\n') || command.chars().count() > 240)
                .then(|| ("Command", command.to_owned()))
        }
        "agent.spawn" => match args.get("tasks").and_then(Value::as_array) {
            Some(tasks) => Some((
                "Tasks",
                tasks
                    .iter()
                    .map(|task| format!("{}\n{}", agent_name(task), task_text(task)))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            )),
            None => Some(("Task", task_text(args))),
        },
        "agent.steer" | "agent.message" | "agent.followup" => {
            text(args, "text").map(|message| ("Message", message.to_owned()))
        }
        "clarify" => {
            let asked: Vec<String> = args
                .get("questions")?
                .as_array()?
                .iter()
                .map(|question| {
                    let mut lines = vec![
                        text(question, "question")
                            .or_else(|| text(question, "header"))
                            .unwrap_or_default()
                            .to_owned(),
                    ];
                    for option in question["options"].as_array().into_iter().flatten() {
                        if let Some(label) = text(option, "label") {
                            lines.push(format!("  · {label}"));
                        }
                    }
                    lines.join("\n")
                })
                .collect();
            Some(("Questions", asked.join("\n\n")))
        }
        "read" | "ls" | "glob" | "grep" | "code" | "edit" | "write" | "git" | "web.search"
        | "web.fetch" | "update_plan" | "agent.wait" | "agent.list" | "agent.transcript"
        | "agent.cancel" | "skill" | "skill.load" | "image.view" | "mcp.status" | "diagnostics" => {
            None
        }
        _ => fields(args, INPUT_LIMIT).map(|text| ("Details", text)),
    }
}

fn task_text(args: &Value) -> String {
    let objective = text(args, "objective").unwrap_or_default();
    match text(args, "contract") {
        Some(contract) => format!("{objective}\n\nExpected back: {contract}"),
        None => objective.to_owned(),
    }
}
