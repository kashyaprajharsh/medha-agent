//! Distinguish an unavailable connection from a withdrawn tool registration.

use crate::{McpManager, McpToolSpec, ServerConfig, ServerState, ToolFilter, hub::Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Registration {
    fingerprint: String,
    permitted: bool,
    filter: ToolFilter,
}

impl Registration {
    pub(crate) fn new(config: &ServerConfig) -> Self {
        Self {
            fingerprint: crate::cache::fingerprint(config),
            permitted: !config.disabled,
            filter: config.tools.clone(),
        }
    }
}

#[derive(Default)]
pub struct SessionTools {
    pub specs: Vec<McpToolSpec>,
    pub sources: BTreeMap<String, String>,
    pub unavailable: BTreeSet<String>,
}

impl McpManager {
    pub async fn session_tools(&self, previous: &BTreeMap<String, String>) -> SessionTools {
        collect(&self.snapshot().await, previous)
    }
}

fn collect(snapshot: &Snapshot, previous: &BTreeMap<String, String>) -> SessionTools {
    let mut out = SessionTools::default();
    for status in &snapshot.servers {
        let Some(registration) = snapshot.registrations.get(&status.server) else {
            continue;
        };
        if !registration.permitted
            || matches!(
                status.state,
                ServerState::Disabled | ServerState::NeedsApproval | ServerState::Stopped
            )
        {
            continue;
        }
        if let Some(catalog) = snapshot.catalogs.get(&status.server) {
            for spec in &catalog.exposed {
                out.sources
                    .insert(spec.name.clone(), registration.fingerprint.clone());
                out.specs.push(spec.clone());
            }
        }
        if status.state == ServerState::Ready {
            continue;
        }
        for (name, fingerprint) in previous {
            let Ok((server, tool)) = crate::parse_qualified(name) else {
                continue;
            };
            if server == status.server
                && fingerprint == &registration.fingerprint
                && registration.filter.admits(&tool)
                && !out.sources.contains_key(name)
            {
                out.sources.insert(name.clone(), fingerprint.clone());
                out.unavailable.insert(name.clone());
            }
        }
    }
    out.specs.sort_by(|left, right| left.name.cmp(&right.name));
    out
}

#[cfg(test)]
#[path = "session_tools_tests.rs"]
mod tests;
