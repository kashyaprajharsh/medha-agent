use super::*;
use crate::test_http::{Response, Server};
const SMALL: &str = r#"{"fixture":{"name":"Fixture","api":"https://fixture.test/v1","npm":"@ai-sdk/openai-compatible","models":{"m":{"name":"M","limit":{"context":8000},"modalities":{"output":["text"]}}}}}"#;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "medha-catalogue-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn cache(&self) -> PathBuf {
        self.0.join("cache.json")
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn bundled_catalogue_maps_protocols_and_keeps_manual_and_local_paths() {
    let data = Catalogue::new(None, "http://127.0.0.1:1".into());
    let data = data.current.read().unwrap();
    assert!(data.providers.len() >= 220);
    assert!(data.entries.len() > 8000);
    let find = |id| data.providers.iter().find(|p| p.id == id).unwrap();
    assert_eq!(find("google").protocol, Some(Protocol::GeminiInteractions));
    assert!(find("mistral").available);
    assert_eq!(
        find("mistral").url.as_deref(),
        Some("https://api.mistral.ai/v1")
    );
    assert!(!find("anthropic").available);
    assert!(!find("github-copilot").available);
    assert!(find("anthropic").unavailable_reason.is_some());
    assert_eq!(find("ollama").auth, AuthKind::None);
    assert!(data.providers.iter().filter(|p| p.available).count() > 180);
    assert_eq!(data.revision, revision(&data.providers, &data.entries));
    let mut ids = std::collections::HashSet::new();
    assert!(
        data.providers.iter().all(|p| ids.insert(&p.id)),
        "no duplicate provider identities"
    );
}

#[test]
fn a_remote_catalogue_cannot_replace_a_local_provider_identity() {
    let remote = SMALL.replace("\"fixture\":", "\"ollama\":");
    assert!(
        crate::models_dev::parse_catalogue(remote.as_bytes())
            .unwrap_err()
            .contains("local provider")
    );
}

#[test]
fn an_oversized_cache_keeps_the_last_good_file_and_removes_its_temporary_file() {
    let temp = Temp::new();
    let mut good = crate::models_dev::parse_catalogue(SMALL.as_bytes()).unwrap();
    good.fetched_at_unix = 1;
    save_cache(temp.cache(), &good);
    let before = std::fs::read(temp.cache()).unwrap();
    let mut huge = good;
    huge.fetched_at_unix = 2;
    huge.providers[0].name = "x".repeat(MAX_BYTES + 1);
    huge.revision = revision(&huge.providers, &huge.entries);
    save_cache(temp.cache(), &huge);
    assert_eq!(std::fs::read(temp.cache()).unwrap(), before);
    assert!(!std::fs::read_dir(&temp.0).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
}

#[test]
fn templates_refuse_url_injection_and_do_not_read_an_environment_key() {
    let provider = ProviderRecord::from_metadata(
        "cloud",
        "Cloud",
        Some("https://api.example.test/accounts/${ACCOUNT_ID}/v1"),
        Some("@ai-sdk/openai-compatible"),
    );
    assert!(provider.available);
    assert!(provider.resolve_endpoint(&BTreeMap::new()).is_err());
    for value in [
        ".",
        "..",
        "../other",
        "x?api_key=secret",
        "x#fragment",
        "x/../../",
        "x@evil.test",
        "${ANOTHER}",
        "x%2fother",
        "x\nx",
    ] {
        assert!(
            provider
                .resolve_endpoint(&BTreeMap::from([("ACCOUNT_ID".into(), value.into())]))
                .is_err(),
            "{value}"
        );
    }
    assert_eq!(
        provider
            .resolve_endpoint(&BTreeMap::from([("ACCOUNT_ID".into(), "my-123".into())]))
            .unwrap(),
        "https://api.example.test/accounts/my-123/v1"
    );
    assert!(
        provider
            .resolve_endpoint(&BTreeMap::from([("API_KEY".into(), "secret".into())]))
            .is_err()
    );
    let host = ProviderRecord::from_metadata(
        "host",
        "Host",
        Some("https://${HOST}.example.test/v1"),
        Some("@ai-sdk/openai-compatible"),
    );
    assert!(
        host.resolve_endpoint(&BTreeMap::from([("HOST".into(), "a/b".into())]))
            .is_err()
    );
    let base = ProviderRecord::from_metadata(
        "base",
        "Base",
        Some("${GATEWAY_BASE_URL}/v1"),
        Some("@ai-sdk/openai-compatible"),
    );
    assert_eq!(
        base.resolve_endpoint(&BTreeMap::from([(
            "GATEWAY_BASE_URL".into(),
            "http://127.0.0.1:8080/account/".into()
        )]))
        .unwrap(),
        "http://127.0.0.1:8080/account/v1"
    );
    assert!(
        base.resolve_endpoint(&BTreeMap::from([(
            "GATEWAY_BASE_URL".into(),
            "https://user:secret@example.test".into()
        )]))
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_refreshes_share_one_fetch_and_publish_atomically() {
    let temp = Temp::new();
    let server = Server::scripted(vec![Response {
        delay: Duration::from_millis(100),
        ..Response::json(SMALL)
    }])
    .await;
    let catalogue = Arc::new(Catalogue::new(Some(temp.cache()), server.url.clone()));
    let old = catalogue.current.read().unwrap().clone();
    let mut callers = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let cat = catalogue.clone();
        callers.spawn(async move { cat.refresh(true).await.unwrap() });
    }
    let mut published = None;
    while let Some(result) = callers.join_next().await {
        let data = result.unwrap();
        if let Some(previous) = &published {
            assert!(Arc::ptr_eq(&data, previous));
        }
        published = Some(data);
    }
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert!(
        old.providers.len() > 200,
        "old readers retain an immutable publication"
    );
    let current = catalogue.current.read().unwrap().clone();
    assert!(current.providers.iter().any(|p| p.id == "fixture"));
    let cache = load_cache(&temp.cache()).unwrap();
    assert_eq!(cache.revision, current.revision);
    assert_eq!(cache.revision, revision(&cache.providers, &cache.entries));
    assert!(
        !server.requests.lock().unwrap()[0]
            .0
            .to_ascii_lowercase()
            .contains("authorization")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failure_keeps_good_metadata_and_backoff_does_not_repeat_a_bad_fetch() {
    let server = Server::scripted(vec![Response {
        delay: Duration::from_millis(100),
        ..Response::json("not json")
    }])
    .await;
    let catalogue = Arc::new(Catalogue::new(None, server.url.clone()));
    let old = catalogue.current.read().unwrap().clone();
    let mut callers = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let cat = catalogue.clone();
        callers.spawn(async move { cat.refresh(true).await });
    }
    while let Some(result) = callers.join_next().await {
        assert!(result.unwrap().is_err());
    }
    assert!(Arc::ptr_eq(&old, &catalogue.current.read().unwrap()));
    assert!(catalogue.refresh(false).await.is_ok());
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[test]
fn corrupted_current_cache_does_not_masquerade_as_a_legacy_cache() {
    let temp = Temp::new();
    let cache = serde_json::json!({"schema_version":SCHEMA,"fetched_at_unix":now(),"revision":"wrong","providers":[],"entries":{"injected":{"context":1}}});
    std::fs::write(temp.cache(), cache.to_string()).unwrap();
    let catalogue = Catalogue::new(Some(temp.cache()), String::new());
    assert!(
        !catalogue
            .current
            .read()
            .unwrap()
            .entries
            .contains_key("injected")
    );
    let legacy = serde_json::json!({"schema_version":1,"fetched_at_unix":now(),"entries":{"legacy":{"context":12345,"capabilities":{"modalities":{"input":[],"output":[]}}}}});
    std::fs::write(temp.cache(), legacy.to_string()).unwrap();
    let catalogue = Catalogue::new(Some(temp.cache()), String::new());
    let data = catalogue.current.read().unwrap();
    assert_eq!(data.entries["legacy"].context, Some(12345));
    assert!(
        data.entries["legacy"]
            .capabilities
            .as_ref()
            .unwrap()
            .modalities
            .is_none()
    );
    assert_eq!(
        data.fetched_at_unix, 0,
        "old schema forces a daemon refresh"
    );
}

/// A cache saved before Medha knew which models want their thinking back
/// does not hide that for a week: the catalogue shipped with the app answers
/// at once, the cache's own facts are kept, and it is refreshed.
#[test]
fn a_cache_from_an_earlier_version_still_knows_which_models_take_thinking_back() {
    let shipped = Catalogue::new(None, String::new());
    let marked = |data: &Snapshot| {
        let marked = data.entries.iter();
        marked
            .filter_map(|(id, meta)| Some((id.clone(), meta.thinking_field.clone()?)))
            .collect::<Vec<_>>()
    };
    let known = marked(&shipped.current.read().unwrap());
    assert!(known.len() > 500, "the shipped catalogue lost the field");
    let (id, field) = known[0].clone();

    let temp = Temp::new();
    let earlier = serde_json::json!({"schema_version":SCHEMA - 1,"fetched_at_unix":now(),
        "entries":{id.clone():{"context":64000}}});
    std::fs::write(temp.cache(), earlier.to_string()).unwrap();
    let catalogue = Catalogue::new(Some(temp.cache()), String::new());
    let data = catalogue.current.read().unwrap();
    assert_eq!(data.entries[&id].context, Some(64000));
    assert_eq!(data.entries[&id].thinking_field, Some(field));
    assert_eq!(data.fetched_at_unix, 0, "an earlier cache is refreshed");
}

#[tokio::test]
async fn an_unwritable_cache_does_not_prevent_an_in_memory_refresh() {
    let temp = Temp::new();
    let blocking = temp.0.join("not-a-directory");
    std::fs::write(&blocking, "original").unwrap();
    let server = Server::scripted(vec![Response::json(SMALL)]).await;
    let catalogue = Catalogue::new(Some(blocking.join("cache.json")), server.url.clone());
    let new = catalogue.refresh(true).await.unwrap();
    assert!(new.providers.iter().any(|p| p.id == "fixture"));
    assert_eq!(std::fs::read_to_string(&blocking).unwrap(), "original");
}

#[tokio::test]
async fn oversized_and_redirected_catalogues_do_not_replace_the_snapshot() {
    let destination = Server::scripted(vec![Response::json(SMALL)]).await;
    let redirect = Server::scripted(vec![Response {
        status: "302 Found",
        headers: format!("Location: {}\r\n", destination.url),
        ..Response::json("")
    }])
    .await;
    assert!(fetch(&redirect.url).await.is_err());
    assert!(destination.requests.lock().unwrap().is_empty());
    let large = Server::scripted(vec![Response::json("x".repeat(MAX_BYTES + 1))]).await;
    assert!(fetch(&large.url).await.unwrap_err().contains("16 MiB"));
}
