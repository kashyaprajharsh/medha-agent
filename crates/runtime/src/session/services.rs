use super::*;

pub(super) fn language_servers(
    setup: &Setup<'_>,
    sbx_cfg: &sandbox::SandboxConfig,
    registry: &mut ToolRegistry,
) -> Option<Arc<lsp::LspManager>> {
    let Setup { lock, cwd, .. } = *setup;
    // Switched off means no LSP tools at all, not tools that can never start a server.
    if !lock.lsp.enabled {
        return None;
    }
    let mut lsp_config = lsp::Config {
        enabled: true,
        startup_timeout: std::time::Duration::from_millis(lock.lsp.startup_timeout_ms),
        request_timeout: std::time::Duration::from_millis(lock.lsp.request_timeout_ms),
        diagnostics_timeout: std::time::Duration::from_millis(lock.lsp.diagnostics_timeout_ms),
        diagnostic_settle: std::time::Duration::from_millis(lock.lsp.diagnostic_settle_ms),
        idle_timeout: std::time::Duration::from_millis(lock.lsp.idle_timeout_ms),
        restart_backoff: std::time::Duration::from_millis(lock.lsp.restart_backoff_ms),
        max_restart_attempts: lock.lsp.max_restart_attempts,
        max_servers: lock.lsp.max_servers,
        max_results: lock.lsp.max_results,
        max_text_chars: lock.lsp.max_text_chars,
        max_open_documents: lock.lsp.max_open_documents,
        install_timeout: std::time::Duration::from_millis(lock.lsp.install_timeout_ms),
        write_timeout: std::time::Duration::from_millis(lock.lsp.write_timeout_ms),
        max_frame_bytes: lock.lsp.max_frame_bytes,
        allow_network: lock.lsp.allow_network,
        home: sbx_cfg.home.clone(),
        ..lsp::Config::default()
    };
    lsp_config.allow_network = false;
    let source_folder = cwd.to_path_buf();
    let manager = Arc::new(
        lsp::LspManager::new(cwd.to_path_buf(), lsp_config.clone()).with_project_config(Arc::new(
            move |folder| crate::project_session::lsp_config(&source_folder, folder, &lsp_config),
        )),
    );
    crate::project_trust::register_lsp(&manager);
    registry.register_lsp(manager.clone());
    Some(manager)
}

pub(super) struct Mcp {
    pub mcp_manager: Option<Arc<mcp::McpManager>>,
    pub configured_mcp: std::collections::HashSet<String>,
    pub session_mcp_ids: std::collections::HashSet<String>,
    pub plugin_mcp_ids: std::collections::HashSet<String>,
}

pub(super) async fn connections(
    setup: &Setup<'_>,
    sbx_cfg: &sandbox::SandboxConfig,
    model_profiles: &Arc<std::sync::Mutex<config::Config>>,
    registry: &mut ToolRegistry,
    session_plugins: &mut plugin_session::SessionPlugins,
) -> Result<Mcp> {
    let Setup {
        lock,
        options,
        cwd,
        medha_home,
        notices,
        ..
    } = *setup;
    // MCP definitions are portable; credentials remain in the user store.
    let mut mcp_servers: Vec<mcp::ServerConfig> = model_profiles
        .lock()
        .ok()
        .map(|cfg| {
            cfg.mcp
                .iter()
                // Remote servers intentionally have no command.
                .filter(|(id, server)| {
                    let reachable = !server.command.is_empty() || !server.url.is_empty();
                    !id.trim().is_empty() && reachable
                })
                .map(|(id, server)| config::resolve_mcp_server(id, server))
                .collect()
        })
        .unwrap_or_default();
    let session_servers = crate::session_mcp::definitions(&options.session_mcp);
    let session_mcp_ids = session_servers
        .iter()
        .map(|server| server.id.clone())
        .collect();
    for server in session_servers {
        anyhow::ensure!(
            !mcp_servers.iter().any(|known| known.id == server.id),
            "An editor MCP server conflicts with a configured server"
        );
        mcp_servers.push(server);
    }
    let configured_mcp: std::collections::HashSet<String> =
        mcp_servers.iter().map(|server| server.id.clone()).collect();
    let shared_mcp: std::collections::HashSet<String> = model_profiles
        .lock()
        .map(|cfg| {
            cfg.mcp
                .iter()
                .filter(|(_, server)| crate::mcp_shared::is_shared(server))
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default();
    let plugin_mcp = session_plugins.mcp_servers(&configured_mcp);
    let plugin_mcp_ids = plugin_mcp.iter().map(|server| server.id.clone()).collect();
    mcp_servers.extend(plugin_mcp);
    // An idle manager allows live additions without a restart.
    let mcp_manager = {
        let mcp_config = |servers: Vec<mcp::ServerConfig>| mcp::Config {
            enabled: true,
            servers,
            startup_timeout: std::time::Duration::from_millis(lock.mcp.startup_timeout_ms),
            request_timeout: std::time::Duration::from_millis(lock.mcp.request_timeout_ms),
            max_text_chars: lock.mcp.max_text_chars,
            allow_network: lock.mcp.allow_network,
            health_interval: std::time::Duration::from_millis(lock.mcp.health_interval_ms),
            max_reconnects: lock.mcp.max_reconnects,
            park_probe: std::time::Duration::from_millis(lock.mcp.park_probe_ms),
            auth_timeout: std::time::Duration::from_millis(lock.mcp.auth_timeout_ms),
            http_timeout: std::time::Duration::from_millis(lock.mcp.http_timeout_ms),
            tokens: Some(Arc::new(config::McpTokens)),
            cache: Some(medha_home.join("mcp-cache")),
            home: sbx_cfg.home.clone(),
        };
        // With a shared host, the user's remote servers are the host's to run;
        // without one answering, this chat runs them as before.
        let mut manager = None;
        if let Some(endpoint) = options.mcp_host.clone() {
            let own: Vec<_> = mcp_servers
                .iter()
                .filter(|server| !shared_mcp.contains(&server.id))
                .cloned()
                .collect();
            let attached = mcp::McpManager::new(cwd.to_path_buf(), mcp_config(own.clone()));
            // A host that is still starting gets a moment to bind.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let joined = loop {
                match tokio::time::timeout_at(
                    deadline.into(),
                    attached.attach_hub(endpoint.clone()),
                )
                .await
                {
                    Ok(Ok(())) => break true,
                    Ok(Err(_)) if std::time::Instant::now() < deadline => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    _ => break false,
                }
            };
            if joined {
                let shared: Vec<_> = mcp_servers
                    .iter()
                    .filter(|server| shared_mcp.contains(&server.id))
                    .cloned()
                    .collect();
                tokio::spawn({
                    let attached = attached.clone();
                    async move { attached.share(shared).await }
                });
                mcp_servers = own;
                manager = Some(attached);
            } else {
                notices.say(
                    "note: Medha's connection host did not answer; this chat connects its own MCP servers",
                );
            }
        }
        let had_servers = !mcp_servers.is_empty();
        let manager =
            Arc::new(manager.unwrap_or_else(|| {
                mcp::McpManager::new(cwd.to_path_buf(), mcp_config(mcp_servers))
            }));
        registry.register_mcp(manager.clone());
        if had_servers {
            tokio::spawn({
                let manager = manager.clone();
                async move { manager.connect_startup().await }
            });
        }
        Some(manager)
    };
    Ok(Mcp {
        mcp_manager,
        configured_mcp,
        session_mcp_ids,
        plugin_mcp_ids,
    })
}
