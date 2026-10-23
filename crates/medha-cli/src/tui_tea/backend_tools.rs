//! `/tools`: every tool new chats could have, switched on or off for you or for this project.
use super::backend_features::mcp_set_tool;
use super::backend_ui::{Effect, request};
use super::*;
use protocol::{OffReason, ToolScope, ToolSwitch, ToolsCommand, ToolsResult};

pub(super) fn open_tools_picker(model: &mut Model, scope: ToolScope) {
    request(model, Effect::Tools(ToolsCommand::List, scope));
}

/// What a row says for a reason, the scope whose switch can undo it, and where else to go when none can.
fn reason(why: OffReason) -> (&'static str, Option<ToolScope>, &'static str) {
    match why {
        OffReason::UserDeny | OffReason::ServerDeny => (
            "Off for you in every project",
            Some(ToolScope::User),
            "Tab to Just me",
        ),
        OffReason::ProjectDeny => (
            "Off in this project, from medha.lock",
            Some(ToolScope::Project),
            "Tab to This project",
        ),
        OffReason::Core => ("Not part of Core tools", None, "choose All tools to use it"),
        OffReason::ProjectAllow => (
            "Left out by this project's allow list",
            None,
            "change it in medha.lock",
        ),
        OffReason::UserAllow => (
            "Left out by your allow list",
            None,
            "change it in config.toml",
        ),
    }
}

/// An MCP tool's server and own name, split where the backend joins them.
fn server_of(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix(mcp::TOOL_PREFIX)?.split_once("__")
}

fn shown_name(tool: &ToolSwitch) -> String {
    server_of(&tool.name).map_or_else(
        || tool.name.clone(),
        |(server, own)| format!("{server} · {own}"),
    )
}

/// The first reason this scope's switch cannot undo, with where to go instead.
fn blocked(tool: &ToolSwitch, scope: ToolScope) -> Option<&'static str> {
    tool.off
        .iter()
        .map(|why| reason(*why))
        .find(|(_, undoes, _)| *undoes != Some(scope))
        .map(|(_, _, hint)| hint)
}

pub(super) fn title(scope: ToolScope, tools: &[ToolSwitch]) -> String {
    let on = tools.iter().filter(|tool| tool.off.is_empty()).count();
    let scope = match scope {
        ToolScope::User => "Just me",
        ToolScope::Project => "This project",
    };
    format!(
        " tools · {scope} · {on} of {} on · space on/off · tab scope · Esc close ",
        tools.len()
    )
}

pub(super) fn labels(scope: ToolScope, tools: &[ToolSwitch]) -> Vec<String> {
    let width = tools
        .iter()
        .map(|tool| shown_name(tool).chars().count())
        .max()
        .unwrap_or(0);
    tools
        .iter()
        .map(|tool| {
            let text = match (tool.off.first(), blocked(tool, scope)) {
                (None, _) => tool
                    .description
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
                (Some(why), Some(hint)) => format!("{}. {hint}", reason(*why).0),
                (Some(why), None) => reason(*why).0.to_owned(),
            };
            let mark = if tool.off.is_empty() { "●" } else { "○" };
            format!("{mark} {:width$}  {text}", shown_name(tool))
        })
        .chain(std::iter::once("← Back".to_string()))
        .collect()
}

/// Turns the tool on or off under `scope`, or says where to go when this scope cannot turn it on.
pub(super) fn flip(model: &mut Model, scope: ToolScope, tool: &ToolSwitch) {
    let on = tool.off.is_empty();
    if !on && let Some(hint) = blocked(tool, scope) {
        model.push_notice(format!("{} stays off here: {hint}.", shown_name(tool)));
        return;
    }
    match (scope, server_of(&tool.name)) {
        // Just me, for an MCP tool, is its server's own setting, the one `/mcp` edits too.
        (ToolScope::User, Some((server, own))) => mcp_set_tool(model, server, own, !on),
        _ => {
            request(
                model,
                Effect::Tools(
                    ToolsCommand::Switch {
                        tool: tool.name.clone(),
                        on: !on,
                        scope,
                    },
                    scope,
                ),
            );
        }
    }
}

pub(super) fn tools_reply(model: &mut Model, reply: ToolsResult, scope: ToolScope) {
    match reply {
        ToolsResult::Tools(tools) => {
            // A list that comes back after a switch keeps the cursor on the row that was switched.
            let selected = match &model.picker {
                Some(picker) if matches!(picker.kind, PickerKind::Tools { .. }) => picker.selected,
                _ => 0,
            };
            model.picker = Some(Picker::with_selected(
                PickerKind::Tools { scope, tools },
                selected,
            ));
        }
        ToolsResult::Changed => open_tools_picker(model, scope),
    }
}
