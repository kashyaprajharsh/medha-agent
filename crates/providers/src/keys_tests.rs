use super::*;

fn shipped() -> Vec<ProviderRecord> {
    crate::models_dev::parse_catalogue(include_str!("fixtures/provider-catalogue.json").as_bytes())
        .unwrap()
        .providers
}

fn read(key: &str, providers: &[ProviderRecord]) -> String {
    let ids = |list: &[&ProviderRecord]| {
        list.iter()
            .map(|provider| provider.id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    };
    match recognise(key, providers) {
        Recognised::One(provider) => format!("one:{}", provider.id),
        Recognised::Several(list) => format!("several:{}", ids(&list)),
        Recognised::Admin(provider) => format!("admin:{}", provider.id),
        Recognised::Unknown => "unknown".into(),
    }
}

#[test]
fn the_longest_opening_wins_and_a_shared_opening_asks() {
    let providers = shipped();
    for (key, expected) in [
        ("sk-or-v1-abc", "one:openrouter"),
        ("sk-or-", "one:openrouter"),
        ("  sk-proj-abc\n", "one:openai"),
        ("sk-ant-api03-abc", "one:anthropic"),
        ("AQ.Ab12", "one:google"),
        ("AIzaSyAbc", "one:google"),
        ("sk-admin-abc", "admin:openai"),
        ("sk-abc123", "several:openai,deepseek,moonshotai"),
        ("SK-ABC", "unknown"),
        ("a1b2c3", "unknown"),
        ("", "unknown"),
    ] {
        assert_eq!(read(key, &providers), expected, "{key:?}");
    }
}

#[test]
fn a_key_is_never_offered_to_a_provider_that_does_not_own_its_opening() {
    let providers: Vec<_> = shipped()
        .into_iter()
        .filter(|provider| !matches!(provider.id.as_str(), "openrouter" | "deepseek"))
        .collect();
    assert_eq!(read("sk-or-v1-abc", &providers), "unknown");
    assert_eq!(read("sk-abc", &providers), "several:openai,moonshotai");
}

#[test]
fn every_provider_in_the_table_is_in_the_shipped_catalogue() {
    let providers = shipped();
    for (prefix, owners) in PREFIXES {
        for id in *owners {
            assert!(
                providers.iter().any(|provider| provider.id == *id),
                "{prefix} names {id}, which the catalogue no longer has"
            );
        }
    }
}
