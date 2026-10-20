use crate::logo_rules::refusal;
use sha2::{Digest, Sha256};
use std::path::Path;

/// What models.dev serves for an id it has no mark for.
const PLACEHOLDER: &str = "13bb0b37c627f6e5961487cd0159abc18dd87ff318668357c7c9990b42e7f32f";

#[test]
fn a_logo_that_could_load_or_run_anything_is_refused() {
    let mark = r##"<svg viewBox="0 0 24 24"><use href="#a"/><path id="a" d="M0 0h24"/></svg>"##;
    let commented = format!("<?xml version=\"1.0\"?>\n<!-- Inkscape -->\n{mark}");
    assert_eq!(refusal(mark.as_bytes()), None);
    assert_eq!(refusal(commented.as_bytes()), None);
    for hostile in [
        r#"<svg><script>alert(1)</script></svg>"#,
        r#"<svg onload="alert(1)"><path/></svg>"#,
        r#"<svg><a xlink:href="javascript:alert(1)"/></svg>"#,
        r#"<svg><use href="https://tracker.example/x.svg#a"/></svg>"#,
        r#"<svg><image href="data:image/png;base64,AAAA"/></svg>"#,
        r#"<svg><foreignObject><iframe/></foreignObject></svg>"#,
        r#"<!DOCTYPE svg [<!ENTITY x "y">]><svg/>"#,
        r#"<html><svg/></html>"#,
    ] {
        assert!(refusal(hostile.as_bytes()).is_some(), "{hostile}");
    }
    let large = format!("<svg>{}</svg>", " ".repeat(32 * 1024));
    assert!(refusal(large.as_bytes()).is_some());
}

#[test]
fn every_bundled_logo_is_a_known_providers_safe_mark() {
    let known: Vec<_> = crate::models_dev::parse_catalogue(
        include_str!("fixtures/provider-catalogue.json").as_bytes(),
    )
    .unwrap()
    .providers
    .into_iter()
    .map(|provider| provider.id)
    .collect();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/desktop/src/assets/providers");
    let mut count = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let id = path.file_stem().unwrap().to_str().unwrap().to_owned();
        let data = std::fs::read(&path).unwrap();
        assert!(known.contains(&id), "{id} is not a provider Medha lists");
        assert_eq!(refusal(&data), None, "{id}");
        assert_ne!(format!("{:x}", Sha256::digest(&data)), PLACEHOLDER, "{id}");
        count += 1;
    }
    assert!(count > 150, "only {count} logos are bundled");
}
