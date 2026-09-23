//! Resolves enabled plugins into this session's skill, MCP server, and hook slots.
//!
//! Discovery runs once, so every slot sees the same approved package hashes.

use extensions::{Discovery, ExtensionComponent, ListedPlugin, Store};
use std::collections::HashSet;
use std::path::Path;

pub struct SessionPlugins {
    store: Store,
    discovery: Discovery,
    warnings: Vec<String>,
}

impl SessionPlugins {
    pub fn discover(store: Store) -> Self {
        let (discovery, warnings) = match store.discover() {
            Ok(discovery) => {
                let notices = discovery.notices();
                (discovery, notices)
            }
            Err(error) => (Discovery::default(), vec![error.to_string()]),
        };
        Self {
            store,
            discovery,
            warnings,
        }
    }

    pub fn add_skills(&mut self, skills: &tools::SkillStore) {
        let sources: Vec<(String, std::path::PathBuf)> = self
            .components()
            .into_iter()
            .filter_map(|(plugin, component)| match component {
                ExtensionComponent::Skill { path, .. } => Some((
                    plugin.package.manifest.id.clone(),
                    plugin.package.root.join(path),
                )),
                _ => None,
            })
            .collect();
        self.warnings.extend(skills.set_plugin_skills(&sources));
    }

    /// `configured` holds the ids of servers from user config, which a plugin
    /// server may never replace.
    pub fn mcp_servers(&mut self, configured: &HashSet<String>) -> Vec<mcp::ServerConfig> {
        let mut servers: Vec<mcp::ServerConfig> = Vec::new();
        let mut problems = Vec::new();
        for (plugin, component) in self.components() {
            let ExtensionComponent::Mcp {
                id: component_id,
                command,
                url,
                allow_tools,
                deny_tools,
            } = component
            else {
                continue;
            };
            let plugin_id = &plugin.package.manifest.id;
            let server_id = server_id(plugin_id, &component_id);
            if configured.contains(&server_id) || servers.iter().any(|s| s.id == server_id) {
                problems.push(format!(
                    "{plugin_id}/{component_id}: MCP server id '{server_id}' is already in use"
                ));
                continue;
            }
            let network = plugin.grant.as_ref().is_some_and(|grant| grant.network);
            let transport = match url {
                Some(url) if !network => {
                    problems.push(format!(
                        "{plugin_id}/{component_id}: remote MCP server {url} needs network; \
                         the plugin must list its host in permissions.network_hosts"
                    ));
                    continue;
                }
                Some(url) => mcp::Transport::Remote {
                    url: url.clone(),
                    auth: mcp::RemoteAuth::Auto,
                },
                None => {
                    let root = plugin.package.root.to_string_lossy();
                    mcp::Transport::Stdio {
                        command: command
                            .iter()
                            .map(|part| part.replace(extensions::PLUGIN_ROOT_PLACEHOLDER, &root))
                            .collect(),
                        env: Vec::new(),
                    }
                }
            };
            servers.push(mcp::ServerConfig {
                id: server_id,
                transport,
                // Enabling the exact package hash is the operator's approval.
                requires_approval: false,
                allow_network: Some(network),
                tools: mcp::ToolFilter {
                    allow: allow_tools.clone(),
                    deny: deny_tools.clone(),
                },
                read_roots: vec![plugin.package.root.clone()],
                ..mcp::ServerConfig::default()
            });
        }
        self.warnings.extend(problems);
        servers
    }

    pub fn hook_runner(&self, workspace: &Path) -> extensions::ProcessHookRunner {
        extensions::ProcessHookRunner::new(self.store.clone(), &self.discovery, workspace)
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn components(&self) -> Vec<(ListedPlugin, ExtensionComponent)> {
        self.discovery
            .enabled()
            .flat_map(|plugin| {
                plugin
                    .package
                    .manifest
                    .components
                    .iter()
                    .map(move |component| (plugin.clone(), component.clone()))
            })
            .collect()
    }
}

/// Applies plugin enable, disable, update, and rollback to a running session:
/// skills and hooks are swapped at once and MCP servers connect in the
/// background. Only servers this session registered for plugins are removed.
pub struct LivePlugins {
    store: Store,
    skills: std::sync::Arc<tools::SkillStore>,
    mcp: Option<std::sync::Arc<mcp::McpManager>>,
    reload_hooks: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    configured: HashSet<String>,
    owned: std::sync::Mutex<HashSet<String>>,
}

impl LivePlugins {
    pub fn new(
        store: Store,
        skills: std::sync::Arc<tools::SkillStore>,
        mcp: Option<std::sync::Arc<mcp::McpManager>>,
        reload_hooks: Box<dyn Fn() -> Vec<String> + Send + Sync>,
        configured: HashSet<String>,
        owned: HashSet<String>,
    ) -> Self {
        Self {
            store,
            skills,
            mcp,
            reload_hooks,
            configured,
            owned: std::sync::Mutex::new(owned),
        }
    }

    pub fn apply(&self) -> Vec<String> {
        let mut session = SessionPlugins::discover(self.store.clone());
        let notices = session.warnings.len();
        session.add_skills(&self.skills);
        let servers = session.mcp_servers(&self.configured);
        let mut warnings: Vec<String> = session.warnings.split_off(notices);
        warnings.extend(
            (self.reload_hooks)()
                .into_iter()
                .filter(|warning| !session.warnings.contains(warning)),
        );
        let Some(mcp) = self.mcp.clone() else {
            return warnings;
        };
        let Ok(mut owned) = self.owned.lock() else {
            return warnings;
        };
        let wanted: HashSet<String> = servers.iter().map(|server| server.id.clone()).collect();
        let removed: Vec<String> = owned.difference(&wanted).cloned().collect();
        let added: Vec<mcp::ServerConfig> = servers
            .into_iter()
            .filter(|server| !owned.contains(&server.id))
            .collect();
        *owned = wanted;
        if !removed.is_empty() || !added.is_empty() {
            tokio::spawn(async move {
                for id in removed {
                    let _ = mcp.remove_server(&id).await;
                }
                for server in added {
                    if let Err(error) = mcp.add_server(server.clone()).await {
                        tracing::warn!(server = %server.id, %error, "plugin MCP server did not start");
                    }
                }
            });
        }
        warnings
    }
}

/// MCP tool names are `mcp__<server>__<tool>`; provider tool names allow only
/// ASCII letters, digits, `_`, and `-`, and `__` separates the parts.
fn server_id(plugin_id: &str, component_id: &str) -> String {
    let mut id = String::new();
    for c in format!("{plugin_id}-{component_id}").chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if !(c == '-' && id.ends_with('-')) {
            id.push(c.to_ascii_lowercase());
        }
    }
    id
}

#[cfg(test)]
#[path = "plugin_session_tests.rs"]
mod tests;
