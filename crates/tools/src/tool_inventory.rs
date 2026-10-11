//! Build a registry inventory without confusing an outage with removal.

use crate::ToolRegistry;
use kernel::{BlastRadius, Executor, ToolCategory, ToolSpec, tool_snapshot::ToolInventory};
use std::collections::BTreeMap;

pub(crate) async fn collect(
    registry: &ToolRegistry,
    previous_sources: &BTreeMap<String, String>,
) -> ToolInventory {
    let mut inventory = ToolInventory {
        specs: registry
            .specs()
            .into_iter()
            .filter(|spec| !mcp::McpManager::is_mcp_tool(&spec.name))
            .collect(),
        ..Default::default()
    };
    if let Some(mcp) = &registry.mcp {
        let tools = mcp.session_tools(previous_sources).await;
        inventory.sources = tools.sources;
        inventory.unavailable = tools.unavailable;
        inventory
            .specs
            .extend(tools.specs.into_iter().map(|spec| ToolSpec {
                name: spec.name,
                description: spec.description,
                schema: spec.schema,
                blast_radius: BlastRadius::External,
                category: ToolCategory::Other,
                icon: "•".into(),
            }));
    }
    inventory
        .specs
        .sort_by(|left, right| left.name.cmp(&right.name));
    inventory
}

#[cfg(test)]
#[path = "tool_inventory_tests.rs"]
mod tests;
