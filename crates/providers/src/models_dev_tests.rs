
use super::*;
use crate::catalogue::{Cache, SCHEMA as CACHE_SCHEMA_VERSION};

/// Both cache prices come from the catalogue. A model that lists neither
/// has neither: an absent price is never taken to be zero.
#[test]
fn cache_prices_are_read_from_the_catalogue_when_it_lists_them() {
    let raw = serde_json::from_value(serde_json::json!({
        "one":{"models":{
            "caching":{"cost":{"input":3.0,"output":15.0,"cache_read":0.3,"cache_write":3.75}},
            "plain":{"cost":{"input":1.0,"output":2.0}}
        }}
    }))
    .unwrap();
    let entries = flatten(raw);
    let prices = |model: &str| {
        let meta = exact_lookup(model, &entries).unwrap();
        (meta.cached_input_per_mtok, meta.cache_write_per_mtok)
    };
    assert_eq!(prices("caching"), (Some(0.3), Some(3.75)));
    assert_eq!(prices("plain"), (None, None));
}

/// A host that resells a model lists it as "maker/model", which is also how
/// the maker's own entry is filed. The maker's own price and limits stand.
#[test]
fn another_hosts_spelling_does_not_replace_a_makers_own_entry() {
    let raw = serde_json::from_value(serde_json::json!({
        "maker":{"models":{"m":{"limit":{"context":64000},"cost":{"input":1.0,"output":2.0}}}},
        "reseller":{"models":{
            "maker/m":{"limit":{"context":8000},"cost":{"input":9.0,"output":9.0}},
            "other/n":{"limit":{"context":4000}}}},
    }))
    .unwrap();
    let entries = flatten(raw);
    assert_eq!(entries["maker/m"].input_per_mtok, Some(1.0));
    assert_eq!(entries["maker/m"].context, Some(64000));
    assert_eq!(entries["reseller/maker/m"].input_per_mtok, Some(9.0));
    // A spelling nobody else owns is still found on its own.
    assert_eq!(entries["other/n"].context, Some(4000));
}

/// The catalogue says, model by model, whether earlier thinking must be
/// sent back and under which field. It is read as written there: a model
/// marked only `true` names no field, and one not marked has none.
#[test]
fn the_field_a_model_wants_its_thinking_back_in_is_read_from_the_catalogue() {
    let raw = serde_json::from_value(serde_json::json!({
            "deepseek":{"models":{
                "deepseek-reasoner":{"limit":{"context":64000},"interleaved":{"field":"reasoning_content"}},
                "deepseek-chat":{"limit":{"context":64000}}}},
            "openrouter":{"models":{
                "vendor/detailed":{"limit":{"context":8000},"interleaved":{"field":"reasoning_details"}},
                "vendor/native":{"limit":{"context":8000},"interleaved":true}}},
        }))
        .unwrap();
    let entries = flatten(raw);
    let field = |provider: &str, model: &str| {
        let mut profile = crate::ProviderProfile::openai_chat(
            "https://server.test/v1",
            model,
            crate::AuthKind::Bearer,
        );
        profile.provider_id = Some(provider.into());
        thinking_field_in(&profile, &entries)
    };
    assert_eq!(
        field("deepseek", "deepseek-reasoner").as_deref(),
        Some("reasoning_content")
    );
    assert_eq!(field("deepseek", "deepseek-chat"), None);
    assert_eq!(
        field("openrouter", "vendor/detailed").as_deref(),
        Some("reasoning_details")
    );
    assert_eq!(field("openrouter", "vendor/native"), None);
}

#[test]
fn an_explicit_provider_never_inherits_another_deployments_metadata() {
    let raw = serde_json::from_value(serde_json::json!({
            "one":{"models":{"same":{"limit":{"context":8000},"cost":{"input":1.0,"output":2.0},"tool_call":true,"reasoning_options":[{"type":"effort","values":["low","high"]}]} }},
            "two":{"models":{"same":{"limit":{"context":16000},"cost":{"input":3.0,"output":4.0},"tool_call":false,"reasoning_options":[{"type":"toggle"}]} }}
        })).unwrap();
    let entries = flatten(raw);
    let mut profile =
        crate::ProviderProfile::openai_chat("https://one.test/v1", "same", crate::AuthKind::Bearer);
    profile.provider_id = Some("one".into());
    let meta = profile_meta(&profile, &entries).unwrap();
    assert_eq!(meta.context, Some(8000));
    assert_eq!(meta.input_per_mtok, Some(1.0));
    assert_eq!(meta.capabilities.as_ref().unwrap().tool_calls, Some(true));
    apply_reasoning_efforts(&mut profile, &entries);
    assert_eq!(
        profile.reasoning_efforts.as_deref(),
        Some([ReasoningEffort::Low, ReasoningEffort::High].as_slice())
    );
    profile.provider_id = Some("unknown".into());
    profile.reasoning_efforts = None;
    assert!(profile_meta(&profile, &entries).is_none());
    apply_reasoning_efforts(&mut profile, &entries);
    assert!(profile.reasoning_efforts.is_none());
    profile.provider_id = Some("two".into());
    apply_reasoning_efforts(&mut profile, &entries);
    assert!(
        profile.reasoning_efforts.is_none(),
        "a toggle is not an effort list"
    );
    profile.reasoning_efforts = Some(vec![ReasoningEffort::Medium]);
    profile.provider_id = Some("one".into());
    apply_reasoning_efforts(&mut profile, &entries);
    assert_eq!(
        profile.reasoning_efforts,
        Some(vec![ReasoningEffort::Medium]),
        "intentional deployment overrides win"
    );
}

#[test]
fn null_and_unrecognized_effort_values_do_not_break_other_models_metadata() {
    // Sarvam's current public records include null as the default option;
    // it is not an effort name and must not invalidate the full catalogue.
    let providers = serde_json::from_value(serde_json::json!({
            "sarvam":{"models":{
                "sarvam-30b":{"limit":{"context":32768},"reasoning_options":[{"type":"effort","values":[null,"low","medium","high"]}]},
                "sarvam-105b":{"reasoning":true,"reasoning_options":[{"type":"effort","values":[null,"low","medium","high"]}]}
            }},
            "google":{"models":{"gemini-3.8-flash":{"reasoning_options":[{"type":"effort","values":["low","medium","high"]}]}}}
        })).expect("unrecognized options cannot invalidate unrelated capabilities/prices");
    let entries = flatten(providers);
    for id in [
        "sarvam/sarvam-30b",
        "sarvam/sarvam-105b",
        "google/gemini-3.8-flash",
    ] {
        assert_eq!(
            entries[id].reasoning_efforts.as_deref(),
            Some(
                [
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High
                ]
                .as_slice()
            )
        );
    }
    assert_eq!(entries["sarvam/sarvam-30b"].context, Some(32768));
    let future: ModelEntry = serde_json::from_value(serde_json::json!({
        "reasoning_options":[{"type":"effort","values":[0,{"future":true}]}]
    }))
    .unwrap();
    assert!(future.reasoning_efforts().is_none());
}

#[test]
fn catalogue_efforts_round_trip_and_do_not_turn_toggles_or_unknown_levels_into_efforts() {
    let providers = serde_json::from_value(serde_json::json!({
            "google": {"models": {
                "gemini-3.8-flash": {"reasoning":true, "reasoning_options":[{"type":"effort","values":["low","medium","high"]}]},
                "gemini-3.6-flash": {"reasoning_options":[{"type":"effort","values":["minimal","low","medium","high"]}]},
                "toggle-only": {"reasoning":true,"reasoning_options":[{"type":"toggle"}]},
                "future": {"reasoning":true,"reasoning_options":[{"type":"effort","values":["future-level"]}]},
                "mixed": {"reasoning_options":[{"type":"new-control","anything":true},{"type":"effort","values":["low","future-level","high","low"]}]}
            }}
        })).unwrap();
    let entries = flatten(providers);
    assert_eq!(
        entries["google/gemini-3.8-flash"]
            .reasoning_efforts
            .as_deref(),
        Some(
            [
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High
            ]
            .as_slice()
        )
    );
    assert!(
        entries["google/gemini-3.6-flash"]
            .reasoning_efforts
            .as_ref()
            .unwrap()
            .contains(&ReasoningEffort::Minimal)
    );
    assert!(entries["google/toggle-only"].reasoning_efforts.is_none());
    assert!(entries["google/future"].reasoning_efforts.is_none());
    assert_eq!(
        entries["google/mixed"].reasoning_efforts.as_deref(),
        Some([ReasoningEffort::Low, ReasoningEffort::High].as_slice())
    );
    let cache = Cache {
        schema_version: CACHE_SCHEMA_VERSION,
        fetched_at_unix: 0,
        revision: String::new(),
        providers: Vec::new(),
        entries,
    };
    let reloaded: Cache = serde_json::from_str(&serde_json::to_string(&cache).unwrap()).unwrap();
    assert_eq!(
        reloaded.entries["google/gemini-3.8-flash"].reasoning_efforts,
        cache.entries["google/gemini-3.8-flash"].reasoning_efforts
    );
    let previous: Cache =
        serde_json::from_str(r#"{"schema_version":3,"fetched_at_unix":0,"entries":{}}"#).unwrap();
    assert!(
        !previous.current_schema(),
        "an upgrade refreshes previously discarded effort metadata"
    );
}

#[test]
fn native_reasoning_metadata_is_exact_and_preserves_explicit_profile_levels() {
    let entries = flatten(serde_json::from_value(serde_json::json!({
            "google": {"models":{"catalogue-model":{"reasoning_options":[{"type":"effort","values":["low","high"]}]}}},
            "another-provider":{"models":{"different-model":{"reasoning_options":[{"type":"effort","values":["minimal"]}]}}}
        })).unwrap());
    let mut profile = crate::ProviderProfile::openai_chat(
        "http://localhost/v1",
        "catalogue-model",
        crate::AuthKind::None,
    );
    apply_reasoning_efforts(&mut profile, &entries);
    assert!(
        profile.reasoning_efforts.is_none(),
        "native Gemini metadata cannot change Chat Completions controls"
    );
    profile.protocol = Protocol::GeminiInteractions;
    apply_reasoning_efforts(&mut profile, &entries);
    assert_eq!(
        profile.reasoning_efforts.as_deref(),
        Some([ReasoningEffort::Low, ReasoningEffort::High].as_slice())
    );
    profile.reasoning_efforts = Some(vec![ReasoningEffort::Minimal]);
    apply_reasoning_efforts(&mut profile, &entries);
    assert_eq!(
        profile.reasoning_efforts.as_deref(),
        Some([ReasoningEffort::Minimal].as_slice())
    );
    profile.reasoning_efforts = None;
    profile.model = "different-model".into();
    apply_reasoning_efforts(&mut profile, &entries);
    assert!(
        profile.reasoning_efforts.is_none(),
        "another provider's unqualified catalogue row is not native Google metadata"
    );
    profile.model = "catalogue-model-preview".into();
    apply_reasoning_efforts(&mut profile, &entries);
    assert!(
        profile.reasoning_efforts.is_none(),
        "do not infer preview/alias effort levels by fuzzy matching"
    );
}

#[test]
fn legacy_metadata_cache_requires_refresh_without_losing_offline_prices() {
    let legacy = r#"{"fetched_at_unix":123,"entries":{"m":{"context":128000,"input_per_mtok":1.0,"output_per_mtok":4.0}}}"#;
    let mut cache: Cache = serde_json::from_str(legacy).unwrap();
    assert!(!cache.current_schema());
    assert_eq!(cache.entries["m"].input_per_mtok, Some(1.0));
    assert!(cache.entries["m"].capabilities.is_none());
    cache.schema_version = CACHE_SCHEMA_VERSION;
    let reloaded: Cache = serde_json::from_str(&serde_json::to_string(&cache).unwrap()).unwrap();
    assert!(reloaded.current_schema());
}

#[test]
fn exact_identity_keeps_similar_and_unknown_deployments_unknown() {
    let mut m = BTreeMap::new();
    m.insert(
        "qwen3-32b".to_string(),
        ModelMeta {
            context: Some(131_072),
            input_per_mtok: None,
            output_per_mtok: None,
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
            capabilities: None,
            reasoning_efforts: None,
            thinking_field: None,
        },
    );
    m.insert(
        "claude-opus-4".to_string(),
        ModelMeta {
            context: Some(200_000),
            input_per_mtok: Some(15.0),
            output_per_mtok: Some(75.0),
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
            capabilities: None,
            reasoning_efforts: None,
            thinking_field: None,
        },
    );

    assert_eq!(
        exact_lookup("qwen3-32b", &m).and_then(|x| x.context),
        Some(131_072)
    );
    assert_eq!(
        exact_lookup("nvidia/qwen3-32b-instruct", &m).and_then(|x| x.context),
        None
    );
    assert!(exact_lookup("totally-unknown-model", &m).is_none());
    // Pricing present only when models.dev lists both sides.
    let opus = exact_lookup("claude-opus-4", &m).unwrap();
    assert_eq!(opus.input_per_mtok, Some(15.0));
    assert_eq!(opus.output_per_mtok, Some(75.0));
}

#[test]
fn similar_and_unknown_provider_ids_never_borrow_prices_or_context() {
    let generic = ModelMeta {
        context: Some(8_192),
        input_per_mtok: Some(1.0),
        output_per_mtok: Some(2.0),
        cached_input_per_mtok: None,
        cache_write_per_mtok: None,
        capabilities: None,
        reasoning_efforts: None,
        thinking_field: None,
    };
    let specific = ModelMeta {
        context: Some(131_072),
        input_per_mtok: Some(3.0),
        output_per_mtok: Some(4.0),
        cached_input_per_mtok: None,
        cache_write_per_mtok: None,
        capabilities: None,
        reasoning_efforts: None,
        thinking_field: None,
    };
    let entries = BTreeMap::from([
        ("qwen3".to_string(), generic),
        ("qwen3-32b".to_string(), specific.clone()),
    ]);

    for _ in 0..32 {
        assert!(exact_lookup("vendor/qwen3-32b-instruct", &entries).is_none());
        assert!(exact_lookup("unknown/qwen3-32b", &entries).is_none());
        let matched = exact_lookup("qwen3-32b", &entries).unwrap();
        assert_eq!(matched.context, specific.context);
    }
}

#[test]
fn capability_states_keep_unknown_separate_from_unsupported() {
    let unknown = ModelCapabilities::default();
    assert_eq!(unknown.input_state("image"), CapabilityState::Unknown);
    assert_eq!(unknown.attachment_state(), CapabilityState::Unknown);

    let known = ModelCapabilities {
        modalities: Some(ModalitySet {
            input: Some(BTreeSet::from(["text".into(), "image".into()])),
            output: Some(BTreeSet::from(["text".into()])),
        }),
        attachment: Some(false),
        tool_calls: Some(true),
        reasoning: None,
        ..ModelCapabilities::default()
    };
    assert_eq!(known.input_state("image"), CapabilityState::Supported);
    assert_eq!(known.input_state("audio"), CapabilityState::Unsupported);
    assert_eq!(known.output_state("image"), CapabilityState::Unsupported);
    assert_eq!(known.attachment_state(), CapabilityState::Unsupported);
    assert_eq!(known.tool_call_state(), CapabilityState::Supported);
    assert_eq!(known.reasoning_state(), CapabilityState::Unknown);
    assert_eq!(known.user_image_state(), CapabilityState::Supported);
    assert_eq!(known.tool_result_image_state(), CapabilityState::Unknown);
}

#[test]
fn partial_modality_metadata_preserves_unknown_directions_on_round_trip() {
    for (json, input, output) in [
        (r#"{}"#, CapabilityState::Unknown, CapabilityState::Unknown),
        (
            r#"{"output":["image"]}"#,
            CapabilityState::Unknown,
            CapabilityState::Supported,
        ),
        (
            r#"{"input":["image"]}"#,
            CapabilityState::Supported,
            CapabilityState::Unknown,
        ),
        (
            r#"{"input":[],"output":[]}"#,
            CapabilityState::Unsupported,
            CapabilityState::Unsupported,
        ),
    ] {
        let modalities: ModalitySet = serde_json::from_str(json).unwrap();
        let encoded = serde_json::to_string(&modalities).unwrap();
        let reloaded: ModalitySet = serde_json::from_str(&encoded).unwrap();
        assert_eq!(reloaded.input_state("image"), input, "{json}");
        assert_eq!(reloaded.output_state("image"), output, "{json}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );
    }
}

#[test]
fn legacy_cache_drops_ambiguous_modalities_but_preserves_other_metadata() {
    let json = r#"{"schema_version":1,"fetched_at_unix":123,"entries":{"m":{"context":128000,"input_per_mtok":1.0,"output_per_mtok":4.0,"capabilities":{"modalities":{"input":[],"output":[]},"tool_call":true}}}}"#;
    let mut cache: Cache = serde_json::from_str(json).unwrap();
    cache.discard_ambiguous_legacy_modalities();
    let meta = &cache.entries["m"];
    assert_eq!(meta.context, Some(128000));
    assert_eq!(meta.input_per_mtok, Some(1.0));
    assert_eq!(meta.output_per_mtok, Some(4.0));
    let capabilities = meta.capabilities.as_ref().unwrap();
    assert_eq!(capabilities.input_state("image"), CapabilityState::Unknown);
    assert_eq!(capabilities.tool_call_state(), CapabilityState::Supported);
    assert!(!cache.current_schema());

    let mut current: Cache = serde_json::from_str(json).unwrap();
    current.schema_version = CACHE_SCHEMA_VERSION;
    current.discard_ambiguous_legacy_modalities();
    assert_eq!(
        current.entries["m"]
            .capabilities
            .as_ref()
            .unwrap()
            .input_state("image"),
        CapabilityState::Unsupported
    );
}

#[test]
fn exact_capability_lookup_never_uses_fuzzy_model_names() {
    let capabilities = ModelCapabilities {
        modalities: Some(ModalitySet {
            input: Some(BTreeSet::from(["image".into()])),
            output: Some(BTreeSet::from(["text".into()])),
        }),
        ..ModelCapabilities::default()
    };
    let entries = BTreeMap::from([(
        "vendor/qwen3-32b".to_string(),
        ModelMeta {
            capabilities: Some(capabilities.clone()),
            ..ModelMeta::default()
        },
    )]);

    assert!(exact_lookup("vendor/qwen3-32b-instruct", &entries).is_none());
    assert_eq!(
        exact_lookup("vendor/qwen3-32b", &entries).and_then(|meta| meta.capabilities.clone()),
        Some(capabilities)
    );
}
