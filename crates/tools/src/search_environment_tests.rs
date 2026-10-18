use super::*;

#[test]
fn captures_canonical_and_legacy_values_for_redaction_even_when_overridden() {
    let result = resolve(
        Some("explicit-search-secret-890123456"),
        false,
        "MEDHA_TAVILY_API_KEY",
        "TAVILY_API_KEY",
        |name| {
            Some(
                if name.starts_with("MEDHA_") {
                    "canonical-search-secret-890123456"
                } else {
                    "legacy-search-secret-890123456"
                }
                .into(),
            )
        },
    );
    assert_eq!(result.as_deref(), Some("explicit-search-secret-890123456"));
    for value in [
        "explicit-search-secret-890123456",
        "canonical-search-secret-890123456",
        "legacy-search-secret-890123456",
    ] {
        let secrets = kernel::redact::Secrets::new(kernel::redact::Secrets::in_environment());
        assert!(!secrets.redact(value).contains(value));
    }
}

#[test]
fn captured_settings_never_read_the_daemon_environment() {
    assert_eq!(
        resolve(
            None,
            true,
            "MEDHA_BRAVE_API_KEY",
            "BRAVE_API_KEY",
            |_| panic!("read daemon environment")
        ),
        None
    );
}

#[test]
fn canonical_wins_and_legacy_remains_usable_for_one_release() {
    assert_eq!(
        resolve(None, false, "new", "old", |name| Some(name.into())).as_deref(),
        Some("new")
    );
    assert_eq!(
        resolve(None, false, "new", "old", |name| (name == "old")
            .then(|| "legacy".into()))
        .as_deref(),
        Some("legacy")
    );
}
