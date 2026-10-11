//! Freeze a conversation's tool definitions between compaction boundaries.

use crate::{Event, EventKind, Executor, ToolSpec};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct ToolInventory {
    pub specs: Vec<ToolSpec>,
    pub sources: BTreeMap<String, String>,
    pub unavailable: BTreeSet<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ToolSnapshot {
    version: String,
    pub(crate) specs: Vec<ToolSpec>,
    sources: BTreeMap<String, String>,
}

impl ToolSnapshot {
    pub(crate) fn restore(events: &[Event]) -> Option<Self> {
        events.iter().rev().find_map(|event| {
            if event.kind == EventKind::ToolSnapshot {
                serde_json::from_value(event.payload.clone()).ok()
            } else if event.kind == EventKind::Compaction
                && crate::events::has_valid_compaction_snapshot(&event.payload)
            {
                serde_json::from_value(event.payload.get("tools")?.clone()).ok()
            } else {
                None
            }
        })
    }

    pub(crate) async fn refresh(
        previous: Option<&Self>,
        executor: &dyn Executor,
        compacted: bool,
    ) -> Self {
        let empty = BTreeMap::new();
        let inventory = executor
            .tool_inventory(previous.map_or(&empty, |snapshot| &snapshot.sources))
            .await;
        Self::merge(previous, inventory, compacted)
    }

    fn merge(previous: Option<&Self>, inventory: ToolInventory, compacted: bool) -> Self {
        let mut remaining: BTreeMap<_, _> = inventory
            .specs
            .iter()
            .map(|spec| (spec.name.as_str(), spec))
            .collect();
        let mut specs = Vec::new();
        if let Some(previous) = previous {
            for old in &previous.specs {
                let same_source =
                    previous.sources.get(&old.name) == inventory.sources.get(&old.name);
                if let Some(fresh) = remaining.remove(old.name.as_str()) {
                    let keep =
                        !compacted && same_source && previous.version == env!("CARGO_PKG_VERSION");
                    specs.push(if keep { old } else { fresh }.clone());
                } else if same_source
                    && previous.version == env!("CARGO_PKG_VERSION")
                    && inventory.unavailable.contains(&old.name)
                {
                    specs.push(old.clone());
                }
            }
        }
        for spec in &inventory.specs {
            if remaining.remove(spec.name.as_str()).is_some() {
                specs.push(spec.clone());
            }
        }
        Self {
            version: env!("CARGO_PKG_VERSION").into(),
            specs,
            sources: inventory.sources,
        }
    }

    pub(crate) fn value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("tool snapshot serializes")
    }
}

#[cfg(test)]
#[path = "tool_snapshot_tests.rs"]
mod tests;
