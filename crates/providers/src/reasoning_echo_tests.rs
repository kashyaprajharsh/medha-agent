use super::decided;

/// Thinking is sent back only where the catalogue names the chat field for it.
/// A model listed under another field, or not listed, is never sent it, since a
/// strict server refuses the whole request. The person's own setting overrides both ways.
#[test]
fn the_catalogue_decides_and_the_persons_setting_overrides_it() {
    let listed = |field: Option<&'static str>| move || field.map(str::to_owned);
    assert!(decided(None, listed(Some("reasoning_content"))));
    assert!(!decided(None, listed(Some("reasoning_details"))));
    assert!(!decided(None, listed(None)));
    assert!(
        decided(Some(true), listed(None)),
        "a server the catalogue does not know"
    );
    assert!(!decided(Some(false), listed(Some("reasoning_content"))));
}
