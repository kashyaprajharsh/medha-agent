use super::StoredNames;
use serde_json::json;

#[test]
fn a_later_page_of_a_stored_read_is_named_after_its_file() {
    let mut names = StoredNames::default();
    names.call("r1", &json!({"path": "docs/WHAT_IS_MEDHA.md"}));
    names.result(
        "r1",
        &json!({"content": "…", "hash": "64d557f8bb", "total_size": 140000}),
    );
    let page = json!({"hash": "64d557f8bb", "offset": 14000, "length": 20000});
    assert_eq!(
        names.target(&page).as_deref(),
        Some("docs/WHAT_IS_MEDHA.md · 14,000–34,000")
    );
}

#[test]
fn a_page_of_a_page_keeps_the_original_file_name() {
    let mut names = StoredNames::default();
    names.call("r1", &json!({"path": "big.log"}));
    names.result("r1", &json!({"hash": "aa11"}));
    names.call("r2", &json!({"hash": "aa11", "offset": 0, "length": 10}));
    names.result("r2", &json!({"hash": "bb22"}));
    assert_eq!(
        names
            .target(&json!({"hash": "bb22", "offset": 5}))
            .as_deref(),
        Some("big.log · from 5")
    );
}

#[test]
fn a_result_too_large_for_context_is_named_by_the_hash_it_was_spilled_to() {
    let mut names = StoredNames::default();
    let output = json!({"content": "x".repeat(20_000), "path": "docs/big.md"});
    let json = serde_json::to_string(&output).unwrap();
    let hash = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(json.as_bytes())
    );
    names.call("r1", &json!({"path": "docs/big.md"}));
    names.result("r1", &output);
    assert_eq!(
        names
            .target(&json!({"hash": hash, "offset": 2000, "length": 9000}))
            .as_deref(),
        Some("docs/big.md · 2,000–11,000")
    );
    names.call("s1", &json!({"command": "cargo test"}));
    names.result("s1", &json!({"stdout": "y".repeat(20_000)}));
    let small = json!({"stdout": "ok"});
    names.call("s2", &json!({"command": "ls"}));
    names.result("s2", &small);
    assert_eq!(names.hash_paths.len(), 2);
}

#[test]
fn an_unknown_hash_still_says_it_is_stored_output() {
    let names = StoredNames::default();
    assert_eq!(
        names
            .target(&json!({"hash": "64d557f8bb6f", "offset": 0, "length": 1000}))
            .as_deref(),
        Some("stored output 64d557f8 · 0–1,000")
    );
    assert_eq!(names.target(&json!({"path": "a.txt"})), None);
}
