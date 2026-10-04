//! `medha mcp-host`: the desktop's one owner of remote MCP connections; local command servers stay jailed in each chat.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::AsyncReadExt;

use crate::config;

pub(crate) use runtime::mcp_shared::{TOKEN_ENV, is_shared};

/// The desktop discards the host's output, so a failure to start is kept here.
pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let outcome = host(args).await;
    if let (Err(error), Ok(home)) = (&outcome, config::medha_home()) {
        let log = home.join("mcp-host.log");
        if std::fs::metadata(&log).is_ok_and(|meta| meta.len() > 256 * 1024) {
            let _ = std::fs::remove_file(&log);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
        {
            use std::io::Write;
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs());
            let _ = writeln!(file, "{at} {error:#}");
        }
    }
    outcome
}

async fn host(args: &[String]) -> anyhow::Result<()> {
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
    let changed = Arc::new(tokio::sync::Notify::new());
    tokio::spawn(follow_config(manager.clone(), Arc::clone(&changed)));
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
    // The desktop writes here when it changes a server or a key, and closes it on exit.
    let orphaned = async {
        let mut sink = [0u8; 64];
        let mut stdin = tokio::io::stdin();
        while matches!(stdin.read(&mut sink).await, Ok(read) if read > 0) {
            changed.notify_one();
        }
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
