use super::*;

/// `[tools]` in medha.lock (preset overridable by MEDHA_TOOLS) and in config.toml. Narrowing happens once, at
/// the boundary: children inherit from the narrowed executor, so nothing is widened by delegating.
pub(super) fn narrow(
    setup: &Setup<'_>,
    user: &std::sync::Mutex<config::Config>,
    registered_tools: &std::collections::HashSet<String>,
    executor: Arc<dyn kernel::Executor>,
) -> Result<Arc<dyn kernel::Executor>> {
    let Setup {
        lock,
        options,
        notices,
        ..
    } = *setup;
    let tools = lockfile::ToolsConfig {
        preset: options
            .tools_preset
            .clone()
            .unwrap_or(lock.tools.preset.clone()),
        filter: lock.tools.filter.clone(),
    };
    tools.validate().map_err(anyhow::Error::msg)?;
    let user_tools = user
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .tools
        .clone();
    user_tools
        .validate()
        .map_err(|error| anyhow::anyhow!("config.toml: {error}"))?;
    pricing::warn_unknown(lock, &user_tools, registered_tools, notices);
    if !tools.narrows() && user_tools.is_empty() {
        return Ok(executor);
    }
    Ok(Arc::new(orchestrator::NarrowedExecutor::filtered(
        executor,
        move |tool| off_reasons(tool, &tools, &user_tools).is_empty(),
    )))
}

/// Why new chats leave `tool` out, each reason naming the setting to change; empty means it is offered.
pub fn off_reasons(
    tool: &str,
    project: &lockfile::ToolsConfig,
    user: &kernel::tool_pattern::ToolFilter,
) -> Vec<protocol::OffReason> {
    use protocol::OffReason;
    [
        (
            project.preset == "minimal" && !lockfile::MINIMAL_TOOLS.contains(&tool),
            OffReason::Core,
        ),
        (project.filter.leaves_out(tool), OffReason::ProjectAllow),
        (project.filter.denies(tool), OffReason::ProjectDeny),
        (user.leaves_out(tool), OffReason::UserAllow),
        (user.denies(tool), OffReason::UserDeny),
    ]
    .into_iter()
    .filter_map(|(off, why)| off.then_some(why))
    .collect()
}

/// Every tool new chats in `folder` could have, each with its `off_reasons` under the settings as saved now.
pub fn tool_switches(
    folder: &Path,
    all: &dyn kernel::Executor,
) -> Result<Vec<protocol::ToolSwitch>> {
    let mut project = crate::workspace::load_lock(folder, &crate::project_session::Quiet)?.tools;
    if let Ok(preset) = std::env::var("MEDHA_TOOLS") {
        project.preset = preset;
    }
    let user = config::load()?.unwrap_or_default();
    let mut rows: Vec<protocol::ToolSwitch> = all
        .specs()
        .into_iter()
        .map(|spec| protocol::ToolSwitch {
            off: off_reasons(&spec.name, &project, &user.tools),
            reach: reach(&spec),
            name: spec.name,
            description: spec.description,
        })
        .collect();
    // A server leaves out the tools its own settings turn off, so they come from those settings.
    for (id, server) in &user.mcp {
        for tool in &server.deny_tools {
            let name = format!("{}{id}__{tool}", mcp::TOOL_PREFIX);
            if rows.iter().all(|row| row.name != name) {
                rows.push(protocol::ToolSwitch {
                    name,
                    description: String::new(),
                    reach: protocol::Reach::External,
                    off: vec![protocol::OffReason::ServerDeny],
                });
            }
        }
    }
    Ok(rows)
}

/// What a person is told a tool reaches: a web tool only reads, but what it reads is outside.
fn reach(spec: &kernel::ToolSpec) -> protocol::Reach {
    match (spec.category, spec.blast_radius) {
        (kernel::ToolCategory::Web, _) | (_, kernel::BlastRadius::External) => {
            protocol::Reach::External
        }
        (_, kernel::BlastRadius::Read) => protocol::Reach::Read,
        (_, kernel::BlastRadius::ReversibleLocal) => protocol::Reach::ReversibleLocal,
        (_, kernel::BlastRadius::IrreversibleLocal) => protocol::Reach::IrreversibleLocal,
    }
}

#[cfg(test)]
#[path = "tool_rules_tests.rs"]
mod tests;
