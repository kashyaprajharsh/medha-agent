//! `medha mcp-host`: the desktop's one owner of remote MCP connections; local command servers stay jailed in each chat.

use std::sync::Arc;

use anyhow::Context;
use tokio::io::AsyncReadExt;

use crate::config;

/// The token travels in the environment: other local processes can read argv.
pub(crate) const ADDRESS_ENV: &str = "MEDHA_MCP_HOST";
pub(crate) const TOKEN_ENV: &str = "MEDHA_MCP_HOST_TOKEN";

pub(crate) fn endpoint() -> Option<mcp::hub::Endpoint> {
    let address = std::env::var(ADDRESS_ENV)
        .ok()
        .filter(|value| !value.is_empty())?;
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|value| !value.is_empty())?;
    Some(mcp::hub::Endpoint { address, token })
}

pub(crate) fn is_shared(server: &config::McpServer) -> bool {
    !server.url.is_empty() && server.command.is_empty()
}

pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let address = args
        .iter()
        .position(|arg| arg == "--socket")
        .and_then(|at| args.get(at + 1))
        .context("usage: medha mcp-host --socket <path>")?
        .clone();
    let token: Arc<str> = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{TOKEN_ENV} must be set"))?
        .into();
    let home = config::medha_home()?;
    let cfg = config::load()?.unwrap_or_default();
    let servers = cfg
        .mcp
        .iter()
        .filter(|(id, server)| !id.trim().is_empty() && is_shared(server))
        .map(|(id, server)| config::resolve_mcp_server(id, server))
        .collect();
    let manager = mcp::McpManager::new(
        home.clone(),
        mcp::Config {
            enabled: true,
            servers,
            tokens: Some(Arc::new(config::McpTokens)),
            cache: Some(home.join("mcp-cache")),
            ..mcp::Config::default()
        },
    );
    tokio::spawn({
        let manager = manager.clone();
        async move { manager.connect_startup().await }
    });
    let resolve: mcp::hub::Resolve = Arc::new(|id: &str| {
        let cfg = config::load().ok().flatten()?;
        let server = cfg.mcp.get(id).filter(|server| is_shared(server))?;
        Some(config::resolve_mcp_server(id, server))
    });
    let orphaned = async {
        let mut sink = [0u8; 64];
        let mut stdin = tokio::io::stdin();
        while matches!(stdin.read(&mut sink).await, Ok(read) if read > 0) {}
    };
    tokio::select! {
        served = mcp::hub::run_host(manager.clone(), &address, token, resolve) => {
            served.with_context(|| format!("could not listen on {address}"))?;
        }
        () = orphaned => {}
    }
    manager.shutdown().await;
    #[cfg(unix)]
    let _ = std::fs::remove_file(&address);
    Ok(())
}
