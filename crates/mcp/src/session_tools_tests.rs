use super::*;
use crate::{Config, Error, Slot, build_catalog};
use std::{path::PathBuf, sync::Arc};

async fn connected() -> McpManager {
    let server = ServerConfig {
        id: "server".into(),
        requires_approval: false,
        ..Default::default()
    };
    let manager = McpManager::new(
        PathBuf::from("/tmp"),
        Config {
            enabled: true,
            servers: vec![server],
            ..Default::default()
        },
    );
    manager
        .inner
        .servers
        .lock()
        .await
        .get_mut("server")
        .unwrap()
        .state = ServerState::Ready;
    manager.tools_mut().insert(
        "server".into(),
        build_catalog(
            "server",
            &ToolFilter::default(),
            false,
            vec![rmcp::model::Tool::new(
                "lookup",
                "a tool",
                Arc::new(serde_json::Map::new()),
            )],
        ),
    );
    manager
}

#[tokio::test]
async fn parked_connection_keeps_registration_but_removal_drops_it() {
    let manager = connected().await;
    let initial = manager.session_tools(&BTreeMap::new()).await;
    assert_eq!(initial.specs.len(), 1);
    let generation = manager.inner.servers.lock().await["server"].generation;
    manager
        .record_failure_quietly("server", generation, &Error::NeedsAuth("server".into()))
        .await;
    assert!(manager.tool_specs().is_empty());
    let unavailable = manager.session_tools(&initial.sources).await;
    assert!(unavailable.unavailable.contains("mcp__server__lookup"));
    let saved = serde_json::to_value(manager.snapshot().await).unwrap();
    let shared: Snapshot = serde_json::from_value(saved).unwrap();
    assert_eq!(
        collect(&shared, &initial.sources).unavailable,
        unavailable.unavailable
    );
    manager.remove_server("server").await.unwrap();
    let removed = manager.session_tools(&initial.sources).await;
    assert!(removed.specs.is_empty() && removed.unavailable.is_empty());
}

#[tokio::test]
async fn catalog_withdrawal_and_server_replacement_never_restore_an_old_tool() {
    let manager = connected().await;
    let initial = manager.session_tools(&BTreeMap::new()).await;
    manager.tools_mut().clear();
    assert!(
        manager
            .session_tools(&initial.sources)
            .await
            .unavailable
            .is_empty()
    );
    let mut slots = manager.inner.servers.lock().await;
    slots.insert(
        "server".into(),
        Slot::new(ServerConfig {
            id: "server".into(),
            requires_approval: false,
            transport: crate::Transport::Stdio {
                command: vec!["replacement".into()],
                env: vec![],
            },
            ..Default::default()
        }),
    );
    drop(slots);
    assert!(
        manager
            .session_tools(&initial.sources)
            .await
            .unavailable
            .is_empty()
    );
}
