use super::*;

/// The next session starts with what this one learned, two sessions do not
/// lose each other's answers, and an answer too old to trust is dropped.
#[test]
fn what_a_server_showed_is_kept_between_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("learned.json");
    let first = Learned::at(path.clone());
    let second = Learned::at(path.clone());
    assert_eq!(first.images("chat:local:qwen"), None);

    first.set_images("chat:local:qwen", ImageSupport::Unsupported);
    second.set_images("chat:cloud:vision", ImageSupport::Supported);
    first.set_images("chat:local:other", ImageSupport::Unknown);

    let later = Learned::at(path.clone());
    assert_eq!(
        later.images("chat:local:qwen"),
        Some(ImageSupport::Unsupported)
    );
    assert_eq!(
        later.images("chat:cloud:vision"),
        Some(ImageSupport::Supported)
    );
    assert_eq!(
        later.images("chat:local:other"),
        None,
        "unknown is not an answer"
    );

    let long_ago = now() - TRUSTED_FOR.as_secs() - 1;
    std::fs::write(
        &path,
        format!(r#"{{"images":{{"chat:local:qwen":[false,{long_ago}]}}}}"#),
    )
    .unwrap();
    assert_eq!(Learned::at(path).images("chat:local:qwen"), None);
}

#[test]
fn without_a_file_it_still_remembers_for_the_session() {
    let learned = Learned::default();
    learned.set_images("m", ImageSupport::Unsupported);
    assert_eq!(learned.images("m"), Some(ImageSupport::Unsupported));
}
