use super::*;

#[test]
fn environment_keys_require_their_exact_deployment_pair() {
    let mut env = ModelEnv {
        api_key: Some("person-real-test-key".into()),
        ..Default::default()
    };
    assert!(
        env.key_for("https://one.example/v1")
            .unwrap_err()
            .to_string()
            .contains("requires MEDHA_BASE_URL")
    );
    env.base_url = Some("https://one.example/v1".into());
    assert_eq!(
        env.key_for("https://one.example/v1").unwrap(),
        Some("person-real-test-key")
    );
    // A trailing slash is the same deployment, not a reason to drop the key.
    assert_eq!(
        env.key_for("https://one.example/v1/").unwrap(),
        Some("person-real-test-key")
    );
    for other in [
        "https://two.example/v1",
        "https://one.example/other",
        "http://one.example/v1",
        "https://one.example:8443/v1",
        "https://one.example.attacker.invalid/v1",
    ] {
        assert_eq!(env.key_for(other).unwrap(), None, "key escaped to {other}");
    }
    env.api_key = None;
    assert_eq!(env.key_for("https://two.example/v1").unwrap(), None);
}

#[test]
fn a_flag_cannot_carry_a_saved_profiles_proxy_to_another_deployment() {
    let mut profile = providers::ProviderProfile::openai_chat(
        "https://saved.example/v1",
        "model",
        providers::AuthKind::None,
    );
    profile.proxy = Some("http://proxy.example:8080".into());
    let mut cfg = Config::default();
    cfg.models.insert("saved".into(), profile);
    cfg.default_model = Some("saved".into());
    let env = ModelEnv {
        base_url: Some("https://flag.example/v1".into()),
        model: Some("model".into()),
        api_key: Some("transport-test-key".into()),
        ..Default::default()
    };
    let resolved = resolve_inner(
        Some(&cfg),
        Some("https://flag.example/v1".into()),
        None,
        &env,
        false,
    )
    .unwrap()
    .unwrap();
    assert!(resolved.provider.proxy.is_none());
}
