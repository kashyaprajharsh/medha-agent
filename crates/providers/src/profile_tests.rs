
use super::*;

#[test]
fn output_reservation_cannot_consume_the_entire_window() {
    let mut profile =
        ProviderProfile::openai_chat("https://example.test/v1", "model", AuthKind::None);
    profile.max_ctx = Some(8_000);
    assert!(
        profile.validate().is_ok(),
        "server-default output remains valid"
    );
    profile.max_output_tokens = Some(8_000);
    assert!(profile.validate().is_err());
    profile.max_output_tokens = Some(4_000);
    assert!(profile.validate().is_ok());
}

#[test]
fn legacy_needs_key_migrates_to_bearer_auth() {
    let profile: ProviderProfile = serde_json::from_value(serde_json::json!({
        "base_url": "https://example.test/v1",
        "model": "model",
        "needs_key": true
    }))
    .unwrap();
    assert_eq!(profile.protocol, Protocol::OpenAiChat);
    assert_eq!(profile.auth, AuthKind::Bearer);

    let serialized = serde_json::to_value(&profile).unwrap();
    assert_eq!(serialized["auth"], "bearer");
    assert!(serialized.get("needs_key").is_none());
}

#[test]
fn catalogue_identity_is_optional_and_never_changes_the_credential_target() {
    let original = "https://host.test:8443/accounts/a/v1/";
    let mut profile: ProviderProfile = serde_json::from_value(
        serde_json::json!({"base_url":original,"model":"model-3.8","needs_key":true}),
    )
    .unwrap();
    assert!(profile.provider_id.is_none());
    assert_eq!(profile.credential_target(), original);
    profile.provider_id = Some("wafer.ai".into());
    assert!(profile.validate().is_ok());
    assert_eq!(profile.catalogue_model_id(), "wafer.ai/model-3.8");
    let reloaded: ProviderProfile =
        serde_json::from_value(serde_json::to_value(&profile).unwrap()).unwrap();
    assert_eq!(reloaded.credential_target(), original);
    assert_eq!(reloaded.provider_id.as_deref(), Some("wafer.ai"));
}

#[test]
fn validation_rejects_secret_headers_and_protocol_counter_mismatch() {
    let mut profile =
        ProviderProfile::openai_chat("https://example.test/v1", "model", AuthKind::Bearer);
    profile
        .headers
        .insert("Authorization".into(), "secret".into());
    assert!(profile.validate().is_err());

    profile.headers.clear();
    profile.protocol = Protocol::GeminiInteractions;
    profile.token_counter = TokenCounter::Vllm;
    assert!(profile.validate().is_err());
}

#[test]
fn validation_rejects_credentials_embedded_in_base_url() {
    let profile = ProviderProfile::openai_chat(
        "https://user:secret@example.test/v1",
        "model",
        AuthKind::None,
    );
    assert!(profile.validate().is_err());
}

#[test]
fn capability_override_round_trips_without_becoming_a_secret() {
    let mut profile =
        ProviderProfile::openai_chat("https://example.test/v1", "vision-model", AuthKind::Bearer);
    profile.capabilities = Some(ModelCapabilities {
        modalities: Some(crate::models_dev::ModalitySet {
            input: Some(["text".into(), "image".into()].into_iter().collect()),
            output: Some(["text".into()].into_iter().collect()),
        }),
        attachment: Some(true),
        user_images: Some(true),
        tool_result_images: Some(false),
        image_mime_types: Some(["image/png".into()].into_iter().collect()),
        max_image_bytes: Some(5_000_000),
        tool_calls: Some(true),
        reasoning: Some(false),
        ..ModelCapabilities::default()
    });
    profile.image_input = ImageInputMode::Native;

    let encoded = serde_json::to_value(&profile).unwrap();
    assert_eq!(encoded["capabilities"]["attachment"], true);
    assert_eq!(encoded["capabilities"]["tool_result_images"], false);
    assert_eq!(encoded["image_input"], "native");
    assert!(encoded.to_string().contains("vision-model"));
    assert!(!encoded.to_string().contains("Bearer"));

    let decoded: ProviderProfile = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded.capabilities, profile.capabilities);
    assert_eq!(decoded.image_input, ImageInputMode::Native);
}
