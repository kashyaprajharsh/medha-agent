use super::{decided, listed_in};

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

/// A model is judged by the server it is reached at, not by its name alone:
/// the same name hosted elsewhere may be refused the very field this server
/// demands. A server added by hand, with no provider picked, is found by its
/// address; a model its list does not name yet is treated as its others are.
#[test]
fn a_model_is_judged_by_its_server_however_it_was_added() {
    let thinks = r#""limit":{"context":64000},"reasoning":true,"modalities":{"output":["text"]}"#;
    let wants = r#""interleaved":{"field":"reasoning_content"}"#;
    let raw = format!(
        r#"{{"wants":{{"name":"Wants","api":"https://api.wants.test","npm":"@ai-sdk/openai-compatible","models":{{
            "big":{{"name":"Big",{thinks},{wants}}},"small":{{"name":"Small",{thinks},{wants}}}}}}},
        "strict":{{"name":"Strict","api":"https://strict.test/v1","npm":"@ai-sdk/openai-compatible","models":{{
            "big":{{"name":"Big",{thinks}}}}}}},
        "mixed":{{"name":"Mixed","api":"https://mixed.test/v1","npm":"@ai-sdk/openai-compatible","models":{{
            "one":{{"name":"One",{thinks},{wants}}},"two":{{"name":"Two",{thinks}}}}}}}}}"#
    );
    let data = crate::models_dev::parse_catalogue(raw.as_bytes()).unwrap();
    let field = |picked: Option<&str>, address: &str, model: &str| {
        let mut profile =
            crate::ProviderProfile::openai_chat(address, model, crate::AuthKind::Bearer);
        profile.provider_id = picked.map(str::to_owned);
        listed_in(&profile, &data.providers, &data.entries)
    };
    let wanted = Some("reasoning_content".to_owned());

    // Added by hand: the address finds the server, though "big" is also hosted elsewhere.
    assert_eq!(field(None, "https://api.wants.test/v1", "big"), wanted);
    assert_eq!(field(None, "https://strict.test/v1", "big"), None);
    // A name the list does not have yet, on a server whose thinking models all agree.
    assert_eq!(field(None, "https://api.wants.test/v1", "next"), wanted);
    assert_eq!(
        field(Some("wants"), "https://api.wants.test", "next"),
        wanted
    );
    // Where they do not agree, or none wants it, nothing is guessed.
    assert_eq!(field(None, "https://mixed.test/v1", "three"), None);
    assert_eq!(field(None, "https://strict.test/v1", "next"), None);
    // An address the catalogue does not know, and a name two servers share.
    assert_eq!(field(None, "https://gateway.test/v1", "big"), None);
}

/// The catalogue shipped with the app answers for real servers typed in by
/// hand, with a model name it has never seen: the ones known to demand their
/// thinking back get it, and a strict one is sent none.
#[test]
fn the_shipped_catalogue_knows_real_servers_by_their_address() {
    let shipped = include_str!("fixtures/provider-catalogue.json");
    let data = crate::models_dev::parse_catalogue(shipped.as_bytes()).unwrap();
    let field = |address: &str| {
        let key = crate::AuthKind::Bearer;
        let profile = crate::ProviderProfile::openai_chat(address, "a-name-not-listed", key);
        listed_in(&profile, &data.providers, &data.entries)
    };
    let wanted = Some("reasoning_content".to_owned());
    assert_eq!(field("https://api.deepseek.com/v1"), wanted);
    assert_eq!(field("https://api.moonshot.ai/v1"), wanted);
    assert_eq!(field("https://api.mistral.ai/v1"), None);
    assert_eq!(field("https://api.groq.com/openai/v1"), None);
    assert_eq!(field("http://localhost:11434/v1"), None);
}
