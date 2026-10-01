use kernel::{EventLog, Kernel};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) struct Runtime {
    pub plugins: crate::plugin_session::LivePlugins,
    pub store: extensions::Store,
    pub skills: Arc<tools::SkillStore>,
    pub search: tools::SearchHandle,
    pub workspace: Arc<sandbox::WorkspaceSandbox>,
    pub memory: Option<Arc<memory::MemoryProjection>>,
    pub memory_budget: u32,
    pub memory_stale_days: u32,
    pub mcp: Option<Arc<mcp::McpManager>>,
    pub configured_mcp: std::sync::Mutex<std::collections::HashSet<String>>,
}
impl Runtime {
    pub async fn catalog<P: kernel::Provider, L: EventLog>(&self, kernel: &Kernel<P, L>) -> Value {
        let specs = kernel.executor.specs();
        let known = specs.iter().map(|spec| spec.name.clone()).collect();
        let mut result = self.skills.list(&known);
        result["tools"] = json!(specs.iter().map(|spec| json!({"name": spec.name, "description": spec.description, "category": spec.category, "radius": spec.blast_radius})).collect::<Vec<_>>());
        result["plugins"] = crate::desktop_preferences::plugins(&self.store)
            .unwrap_or_else(|error| json!({"notices": [error.to_string()]}));
        result["mcp"] = if let Some(manager) = &self.mcp {
            serde_json::to_value(manager.status().await).unwrap_or_default()
        } else {
            json!([])
        };
        result
    }
    pub async fn reload(&self) -> Result<Value, String> {
        let cfg = crate::config::load()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        *self
            .search
            .lock()
            .map_err(|_| "Search settings unavailable")? = crate::config::resolve_search(&cfg);
        let previous = self
            .configured_mcp
            .lock()
            .map_err(|_| "MCP settings unavailable")?
            .clone();
        if let Some(manager) = &self.mcp {
            for id in &previous {
                if !cfg.mcp.contains_key(id) {
                    manager
                        .remove_server(id)
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        self.configured_mcp
            .lock()
            .map_err(|_| "MCP settings unavailable")?
            .retain(|id| cfg.mcp.contains_key(id));
        Ok(json!({"warnings": self.plugins.apply()}))
    }
}
impl Runtime {
    /// The UI reviews the exact secret-free server definition before this
    /// explicit human connect action, matching the TUI's runtime add path.
    pub async fn connect(
        &self,
        params: &Value,
        writer: &Arc<crate::acp::Writer>,
    ) -> Result<Value, String> {
        use sha2::{Digest, Sha256};
        let id = params["id"].as_str().ok_or("Server name required")?;
        let cfg = crate::config::load()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let server = cfg.mcp.get(id).ok_or("MCP server not found")?;
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(server).map_err(|e| e.to_string())?)
        );
        if params["hash"] != hash {
            return Err(
                "The server changed since review. Reload its details before connecting.".into(),
            );
        }
        self.start(id, server, writer).await
    }

    /// Clicking Connect on a reviewed entry is its review, so no hash is asked.
    pub async fn connect_connector(
        &self,
        params: &Value,
        writer: &Arc<crate::acp::Writer>,
    ) -> Result<Value, String> {
        let connector = params["id"]
            .as_str()
            .and_then(crate::connectors::find)
            .ok_or("Unknown connector")?;
        let mut installed = None;
        crate::config::edit(|cfg| {
            let id = connector.install(&mut cfg.mcp);
            installed = cfg.mcp.get(&id).cloned().map(|server| (id, server));
            Ok(())
        })
        .map_err(|e| e.to_string())?;
        let (id, server) = installed.ok_or("Connector could not be saved")?;
        self.start(&id, &server, writer).await
    }

    async fn start(
        &self,
        id: &str,
        server: &crate::config::McpServer,
        writer: &Arc<crate::acp::Writer>,
    ) -> Result<Value, String> {
        let manager = self
            .mcp
            .as_ref()
            .ok_or("MCP is disabled in this workspace")?;
        if let Some(ready) = manager
            .status()
            .await
            .into_iter()
            .find(|status| status.server == id && status.state == mcp::ServerState::Ready)
        {
            return serde_json::to_value(ready).map_err(|e| e.to_string());
        }
        // Connect is what turns a disconnected server back on, for every chat.
        let mut server = server.clone();
        if server.disabled {
            server.disabled = false;
            crate::config::edit(|cfg| {
                if let Some(saved) = cfg.mcp.get_mut(id) {
                    saved.disabled = false;
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        }
        let outcome = manager
            .add_server(crate::config::resolve_mcp_server(id, &server))
            .await;
        self.configured_mcp
            .lock()
            .map_err(|_| "MCP settings unavailable")?
            .insert(id.into());
        match outcome {
            Err(mcp::Error::NeedsAuth(_)) => {
                sign_in(Arc::clone(manager), id.into(), Arc::clone(writer));
                Ok(json!({"server": id, "state": "signing_in"}))
            }
            Err(mcp::Error::NeedsToken(_)) => Ok(json!({"server": id, "state": "needs_token"})),
            Err(error) => Err(error.to_string()),
            Ok(status) => {
                let status = serde_json::to_value(status).map_err(|e| e.to_string())?;
                if status["state"] == "needs_auth" {
                    sign_in(Arc::clone(manager), id.into(), Arc::clone(writer));
                    return Ok(json!({"server": id, "state": "signing_in"}));
                }
                Ok(status)
            }
        }
    }

    /// Signs in again to a connected or expired OAuth server.
    pub async fn sign_in_again(
        &self,
        params: &Value,
        writer: &Arc<crate::acp::Writer>,
    ) -> Result<Value, String> {
        let id = params["id"].as_str().ok_or("Server name required")?;
        let manager = self
            .mcp
            .as_ref()
            .ok_or("MCP is disabled in this workspace")?;
        if !manager
            .status()
            .await
            .iter()
            .any(|status| status.server == id)
        {
            return self.connect(params, writer).await;
        }
        sign_in(Arc::clone(manager), id.into(), Arc::clone(writer));
        Ok(json!({"server": id, "state": "signing_in"}))
    }
}

/// OAuth waits on the person in a browser, so it runs beside the bridge
/// instead of holding its request loop; the link and the outcome arrive as
/// `mcp.auth` and `mcp.signed_in` notifications.
fn sign_in(manager: Arc<mcp::McpManager>, id: String, writer: Arc<crate::acp::Writer>) {
    tokio::spawn(async move {
        let (urls, mut links) = tokio::sync::mpsc::unbounded_channel();
        let relay = {
            let (writer, id) = (Arc::clone(&writer), id.clone());
            tokio::spawn(async move {
                while let Some(url) = links.recv().await {
                    writer.notify("mcp.auth", json!({"server": id, "url": url}));
                }
            })
        };
        let outcome = manager.authorize(&id, &urls).await;
        drop(urls);
        let _ = relay.await;
        writer.notify(
            "mcp.signed_in",
            match outcome {
                Ok(status) => json!({"server": id, "ok": true, "status": status}),
                Err(error) => json!({"server": id, "ok": false, "error": error.to_string()}),
            },
        );
    });
}

impl Runtime {
    /// Off for every chat and remembered; calls already running finish first.
    pub async fn disconnect(&self, params: &Value) -> Result<Value, String> {
        let id = params["id"].as_str().ok_or("Server name required")?;
        let manager = self.mcp.as_ref().ok_or("MCP is disabled")?;
        crate::config::edit(|cfg| {
            if let Some(saved) = cfg.mcp.get_mut(id) {
                saved.disabled = true;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
        match manager.set_disabled(id, true).await {
            Ok(_) | Err(mcp::Error::UnknownServer(_)) => Ok(json!({"disconnected": true})),
            Err(error) => Err(error.to_string()),
        }
    }
}
