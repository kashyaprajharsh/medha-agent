use super::strip;
use serde_json::json;

/// The rules a local server cannot use are removed wherever a schema nests,
/// and a tool's own argument that happens to be called `pattern` is kept.
#[test]
fn pattern_rules_go_and_an_argument_named_pattern_stays() {
    let mut body = json!({ "tools": [{ "type": "function", "function": {
        "name": "grep",
        "parameters": { "type": "object", "properties": {
            "pattern": { "type": "string", "description": "what to look for" },
            "format": { "type": "string", "enum": ["text", "json"] },
            "since": { "type": "string", "format": "date-time" },
            "files": { "type": "array", "items": { "type": "string", "pattern": "^[^/]" } },
            "where": { "anyOf": [
                { "type": "string", "pattern": "\\d+" },
                { "type": "null" }
            ] }
        }, "required": ["pattern"] }
    } }] });
    assert_eq!(strip(&mut body), 3);
    let arguments = &body["tools"][0]["function"]["parameters"]["properties"];
    assert_eq!(arguments["pattern"]["description"], "what to look for");
    assert_eq!(arguments["format"]["enum"], json!(["text", "json"]));
    assert_eq!(arguments["since"], json!({ "type": "string" }));
    assert_eq!(arguments["files"]["items"], json!({ "type": "string" }));
    assert_eq!(arguments["where"]["anyOf"][0], json!({ "type": "string" }));
    assert_eq!(strip(&mut body), 0, "nothing is left to remove");
}
