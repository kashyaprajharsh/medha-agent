struct WideNetworkAnswer(NetworkDecision);

#[async_trait::async_trait]
impl HumanGate for WideNetworkAnswer {
    async fn confirm(&self, _: &str, _: Option<&str>, _: bool) -> Approval {
        Approval::Always
    }
    async fn confirm_network(&self, _: Option<&str>, _: bool) -> NetworkDecision {
        self.0
    }
}

#[tokio::test]
async fn tainted_network_approval_never_becomes_a_session_or_persistent_grant() {
    for answer in [NetworkDecision::Session, NetworkDecision::Persistent] {
        let ws = unique_dir("tainted-network");
        let trust = machine_trust_file("tainted-network");
        let grant = NetworkGrant::default();
        let mut manager = PermissionManager::new(&ws, &trust, ws.join("audit.log")).unwrap();
        manager.set_network_grant(grant.clone()).unwrap();
        manager.set_human_gate(Arc::new(WideNetworkAnswer(answer)));
        assert_eq!(
            manager
                .request_network(Some("remote content requested access"), true)
                .await,
            NetworkDecision::Once
        );
        assert!(!grant.granted());
        assert!(
            !trust.exists()
                || !std::fs::read_to_string(&trust)
                    .unwrap()
                    .contains("network_allowed")
        );
    }
}
