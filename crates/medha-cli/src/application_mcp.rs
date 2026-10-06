//! Explicit application MCP controls; parsing, persistence and connections are
//! owned by the backend. Credential-bearing commands are never reflected.
use crate::{acp::Writer, config, desktop_extensions::Runtime};
use protocol::{McpCommand as Command, McpResult as Reply};
use serde_json::json;
use std::sync::Arc;

pub(crate) async fn command(
    runtime: &Runtime,
    command: Command,
    writer: &Arc<Writer>,
    busy: bool,
) -> Result<Reply, String> {
    let manager = runtime.mcp.as_ref();
    let guarded = !matches!(
        &command,
        Command::List
            | Command::Connectors { .. }
            | Command::Catalogue { .. }
            | Command::Authorize { .. }
    );
    if guarded && busy {
        return Err("Finish or stop active work before changing MCP servers.".into());
    }
    match command {
        Command::List => servers(runtime).await.map(Reply::Servers),
        Command::Tools { id } => {
            let manager = manager.ok_or("MCP is disabled in this workspace")?;
            let server = saved(&id)?;
            if server.disabled {
                return Err(
                    "This server is switched off. Turn it on before listing its tools.".into(),
                );
            }
            if manager.server_tools(&id).is_empty() {
                let hash = definition_hash(&server);
                runtime
                    .connect(&json!({"id":id,"hash":hash}), writer)
                    .await?;
            }
            Ok(Reply::Tools {
                id: id.clone(),
                tools: manager
                    .server_tools(&id)
                    .into_iter()
                    .map(|(name, exposed)| protocol::McpTool { name, exposed })
                    .collect(),
            })
        }
        Command::Expose { id, tool, exposed } => {
            config::edit(|cfg| {
                let server = cfg
                    .mcp
                    .get_mut(&id)
                    .ok_or_else(|| anyhow::anyhow!("MCP server not found"))?;
                if exposed {
                    server.deny_tools.retain(|entry| entry != &tool);
                } else if !server.deny_tools.contains(&tool) {
                    server.deny_tools.push(tool.clone());
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?;
            if let Some(manager) = manager {
                manager
                    .add_server(config::resolve_mcp_server(&id, &saved(&id)?))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(Reply::Changed)
        }
        Command::Add { definition } => {
            let parsed =
                config::parse_mcp_add_args(definition.0.split_whitespace().map(str::to_owned))
                    .map_err(|_| {
                        "Invalid MCP definition. Check the server name, URL or command and options."
                            .to_owned()
                    })?;
            let id = parsed.id;
            config::edit(|cfg| {
                if let Some(key) = &parsed.key {
                    config::store_mcp_key(&id, &parsed.server, key)?;
                }
                cfg.mcp.insert(id.clone(), parsed.server.clone());
                Ok(())
            })
            .map_err(|e| e.to_string())?;
            if manager.is_none() {
                return Ok(Reply::Changed);
            }
            let status = runtime
                .connect(
                    &json!({"id":id,"hash":definition_hash(&parsed.server)}),
                    writer,
                )
                .await?;
            Ok(Reply::Status(status))
        }
        Command::Disable { id, disabled } => {
            config::edit(|cfg| {
                cfg.mcp
                    .get_mut(&id)
                    .ok_or_else(|| anyhow::anyhow!("MCP server not found"))?
                    .disabled = disabled;
                Ok(())
            })
            .map_err(|e| e.to_string())?;
            if let Some(manager) = manager {
                if disabled {
                    match manager.set_disabled(&id, true).await {
                        Ok(_) | Err(mcp::Error::UnknownServer(_)) => {}
                        Err(error) => return Err(error.to_string()),
                    }
                } else {
                    runtime
                        .connect(
                            &json!({"id":id,"hash":definition_hash(&saved(&id)?)}),
                            writer,
                        )
                        .await?;
                }
            }
            Ok(Reply::Changed)
        }
        Command::Remove { id } => {
            let previous = saved(&id)?;
            // Supersede authorization before deleting its credential, matching
            // the existing no-resurrection ordering.
            if let Some(manager) = manager {
                manager
                    .remove_server(&id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            config::edit(|cfg| {
                cfg.mcp
                    .remove(&id)
                    .ok_or_else(|| anyhow::anyhow!("MCP server not found"))?;
                config::delete_mcp_key(&id, &previous);
                Ok(())
            })
            .map_err(|e| e.to_string())?;
            Ok(Reply::Changed)
        }
        Command::Connect { id, hash } => runtime
            .connect(&json!({"id":id,"hash":hash}), writer)
            .await
            .map(Reply::Status),
        Command::Authorize { id } => runtime
            .sign_in_again(
                &json!({"id":id,"hash":definition_hash(&saved(&id)?)}),
                writer,
            )
            .await
            .map(Reply::Status),
        Command::Authenticate { id, oauth } => {
            config::edit(|cfg| {
                cfg.mcp
                    .get_mut(&id)
                    .ok_or_else(|| anyhow::anyhow!("MCP server not found"))?
                    .auth = if oauth { "oauth" } else { "none" }.into();
                Ok(())
            })
            .map_err(|e| e.to_string())?;
            runtime
                .connect(
                    &json!({"id":id,"hash":definition_hash(&saved(&id)?)}),
                    writer,
                )
                .await
                .map(Reply::Status)
        }
        Command::Credential { id, key } => {
            config::edit(|cfg| {
                let server = cfg
                    .mcp
                    .get_mut(&id)
                    .ok_or_else(|| anyhow::anyhow!("MCP server not found"))?;
                server.auth = "bearer".into();
                config::store_mcp_key(&id, server, &key.0)?;
                Ok(())
            })
            .map_err(|e| e.to_string())?;
            runtime
                .connect(
                    &json!({"id":id,"hash":definition_hash(&saved(&id)?)}),
                    writer,
                )
                .await
                .map(Reply::Status)
        }
        Command::Connectors { query } => {
            let configured = config::load()
                .map_err(|e| e.to_string())?
                .unwrap_or_default()
                .mcp;
            let query = query.trim().to_lowercase();
            let rows = crate::connectors::catalog()
                .iter()
                .filter(|connector| {
                    [&connector.id, &connector.name, &connector.description]
                        .iter()
                        .any(|value| value.to_lowercase().contains(&query))
                })
                .map(|connector| protocol::ConnectorPick {
                    id: connector.id.clone(),
                    name: connector.name.clone(),
                    description: connector.description.clone(),
                    configured: connector.configured(&configured).is_some(),
                })
                .collect();
            Ok(Reply::Connectors(rows))
        }
        Command::ConnectConnector { id } => runtime
            .connect_connector(&json!({"id":id}), writer)
            .await
            .map(Reply::Status),
        Command::Catalogue { query } => {
            use crate::desktop_mcp_registry::{Source, search};
            let mut result = search(&query, None, Source::Featured)
                .await
                .map_err(|e| e.to_string())?;
            if !query.trim().is_empty() && picks(&result).is_empty() {
                result = search(&query, None, Source::Full)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(Reply::Catalogue(picks(&result)))
        }
    }
}

fn saved(id: &str) -> Result<config::McpServer, String> {
    config::load()
        .map_err(|e| e.to_string())?
        .unwrap_or_default()
        .mcp
        .remove(id)
        .ok_or_else(|| "MCP server not found".into())
}
fn definition_hash(server: &config::McpServer) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(server).expect("MCP definition serializes"))
    )
}
async fn servers(runtime: &Runtime) -> Result<Vec<protocol::McpDefinition>, String> {
    let cfg = config::load()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let statuses = match &runtime.mcp {
        Some(manager) => manager.status().await,
        None => Vec::new(),
    };
    Ok(cfg
        .mcp
        .iter()
        .map(|(id, server)| {
            let status = statuses.iter().find(|status| &status.server == id);
            protocol::McpDefinition {
                id: id.clone(),
                url: (!server.url.is_empty()).then(|| server.url.clone()),
                command: server.command.first().cloned(),
                args: Vec::new(), // never echo token-bearing argv
                disabled: server.disabled,
                remote: !server.url.is_empty(),
                running: status.is_some_and(|status| status.state == mcp::ServerState::Ready),
                tools: runtime
                    .mcp
                    .as_ref()
                    .map_or(0, |manager| manager.server_tools(id).len()),
                hash: definition_hash(server),
                error: status.and_then(|status| status.detail.clone()),
            }
        })
        .collect())
}
fn picks(listing: &serde_json::Value) -> Vec<protocol::CataloguePick> {
    let mut rows = Vec::new();
    for server in listing["servers"].as_array().into_iter().flatten() {
        let name = server["name"].as_str().unwrap_or_default();
        let title = server["title"]
            .as_str()
            .filter(|title| !title.is_empty())
            .unwrap_or(name);
        let description: String = server["description"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .take(70)
            .collect();
        for setup in server["setups"].as_array().into_iter().flatten() {
            if let Some((command, cursor)) = crate::desktop_mcp_registry::add_command(name, setup) {
                rows.push(protocol::CataloguePick {
                    label: format!(
                        "{title} · {} — {description}",
                        if setup["kind"] == "remote" {
                            "remote"
                        } else {
                            "local"
                        }
                    ),
                    command,
                    cursor,
                });
            }
        }
    }
    rows
}
