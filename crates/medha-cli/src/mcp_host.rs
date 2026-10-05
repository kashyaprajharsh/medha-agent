//! The backend's one owner of remote MCP connections; local command servers stay jailed in each chat.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;

use crate::config;

pub(crate) use runtime::mcp_shared::is_shared;

/// Owns the user's remote MCP connections and serves the chats that attach,
/// until `stopped`.
pub(crate) async fn run_shared(
    address: &str,
    token: Arc<str>,
    changed: Arc<tokio::sync::Notify>,
    stopped: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let home = config::medha_home()?;
    let cfg = config::load()?.unwrap_or_default();
    let manager = mcp::McpManager::new(
        home.clone(),
        mcp::Config {
            enabled: true,
            // Keys that cannot be read yet are picked up by `follow_config`.
            servers: shared(&cfg).0,
            tokens: Some(Arc::new(config::McpTokens)),
            cache: Some(home.join("mcp-cache")),
            ..mcp::Config::default()
        },
    );
    tokio::spawn({
        let manager = manager.clone();
        async move { manager.connect_startup().await }
    });
    tokio::spawn(follow_config(manager.clone(), changed));
    let resolve: mcp::hub::Resolve = Arc::new(|id: &str| {
        let Some(cfg) = config::load().ok().flatten() else {
            return Ok(None);
        };
        cfg.mcp
            .get(id)
            .filter(|server| is_shared(server))
            .map(|server| config::read_mcp_server(id, server))
            .transpose()
            .map_err(|error| format!("{error:#}"))
    });
    tokio::select! {
        served = mcp::hub::run_host(manager.clone(), address, token, resolve) => {
            served.with_context(|| format!("could not listen on {address}"))?;
        }
        () = stopped => {}
    }
    manager.shutdown().await;
    #[cfg(unix)]
    let _ = std::fs::remove_file(address);
    Ok(())
}

/// The host runs what the config says: a server removed, switched off or edited
/// anywhere — the desktop, the TUI or by hand — changes for every chat.
/// The desktop says so at once through `changed`; the file is also checked for
/// changes made elsewhere. A config or key that cannot be read changes nothing
/// and is tried again.
async fn follow_config(manager: mcp::McpManager, changed: Arc<tokio::sync::Notify>) {
    let mut seen = None;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        let told = tokio::select! {
            _ = tick.tick() => false,
            () = changed.notified() => true,
        };
        let modified = config::config_path()
            .ok()
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|meta| meta.modified().ok());
        if !told && (modified.is_none() || modified == seen) {
            continue;
        }
        if let Ok(Some(cfg)) = config::load() {
            let (servers, held) = shared(&cfg);
            manager.reconcile(servers, &held).await;
            // A key still unreadable is tried again on the next tick.
            if held.is_empty() {
                seen = modified;
            }
        }
    }
}

/// The servers this host runs: the user's trusted remote ones, and those whose key
/// could not be read. One held back keeps its running connection rather than being
/// replaced by a keyless one, and never stops the others from changing.
fn shared(cfg: &config::Config) -> (Vec<mcp::ServerConfig>, HashSet<String>) {
    let mut servers = Vec::new();
    let mut held = HashSet::new();
    for (id, server) in cfg
        .mcp
        .iter()
        .filter(|(id, server)| !id.trim().is_empty() && is_shared(server))
    {
        match config::read_mcp_server(id, server) {
            Ok(resolved) => servers.push(resolved),
            Err(_) => {
                held.insert(id.clone());
            }
        }
    }
    (servers, held)
}

#[cfg(test)]
#[path = "mcp_host_tests.rs"]
mod tests;
