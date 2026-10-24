use super::*;

#[test]
fn a_plain_key_reads_back_unchanged() {
    assert_eq!(unseal("sk-plain").unwrap(), "sk-plain");
}

/// A sealed value is never handed out as if it were the key.
#[cfg(not(windows))]
#[test]
fn a_key_sealed_on_windows_is_refused_elsewhere() {
    assert!(unseal("sealed:v1:AAAA").is_err());
}

#[cfg(windows)]
#[test]
fn a_sealed_key_hides_its_text_and_opens_back_to_it() {
    let key = format!("{{\"refresh\":\"{}\"}}", "r".repeat(3000));
    let sealed = seal(&key).unwrap();
    assert!(sealed.starts_with(SEALED));
    assert!(
        !sealed.contains("rrrr"),
        "the key's text must not survive sealing"
    );
    assert_eq!(unseal(&sealed).unwrap(), key);

    let mut keys = BTreeMap::from([("chatgpt://a".to_owned(), key.clone())]);
    seal_kept(&mut keys).unwrap();
    let once = keys["chatgpt://a"].clone();
    seal_kept(&mut keys).unwrap();
    assert_eq!(
        keys["chatgpt://a"], once,
        "a sealed key is not sealed twice"
    );
    unseal_all(&mut keys).unwrap();
    assert_eq!(keys["chatgpt://a"], key);
}
