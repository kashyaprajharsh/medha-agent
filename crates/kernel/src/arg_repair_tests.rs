use super::*;
use serde_json::json;

fn repaired(mut args: Value, schema: Value) -> (Value, Vec<Repair>) {
    let repairs = repair(&mut args, &schema);
    (args, repairs)
}

/// A drawing server that takes its elements as JSON text, and a model that
/// sent the list itself. Captured from a real server's published schema.
#[test]
fn a_list_sent_where_the_schema_asks_for_text_is_passed_as_json_text() {
    let schema = json!({
        "type": "object",
        "required": ["elements"],
        "properties": {"elements": {
            "type": "string",
            "description": "JSON array string of Excalidraw elements."
        }}
    });
    let elements = json!([{"type": "rectangle", "x": 1}, {"type": "text", "text": "hi"}]);
    let (args, repairs) = repaired(json!({"elements": elements}), schema.clone());
    assert_eq!(args["elements"], json!(elements.to_string()));
    let parsed: Value = serde_json::from_str(args["elements"].as_str().unwrap()).unwrap();
    assert_eq!(parsed, elements, "nothing is lost on the way");
    assert_eq!(
        note(&repairs).unwrap(),
        "Medha adjusted this call to match the tool's schema: `elements` was sent as an array \
         and passed as a string. Send these types directly next time."
    );

    // The model that already sent text is passed through untouched.
    let text = json!({"elements": "[{\"type\":\"rectangle\"}]"});
    let (args, repairs) = repaired(text.clone(), schema);
    assert_eq!(args, text);
    assert!(repairs.is_empty() && note(&repairs).is_none());
}

#[test]
fn the_fields_a_live_view_must_show_as_text_follow_the_same_rule() {
    let schema = json!({"type": "object", "properties": {
        "elements": {"type": "string"},
        "mode": {"type": "string", "enum": ["a", "b"]},
        "either": {"type": ["string", "array"]},
        "count": {"type": "integer"},
        "free": {}
    }});
    assert_eq!(text_fields(&schema), ["elements"]);
    assert!(text_fields(&json!({"type": "object"})).is_empty());
}

#[test]
fn each_type_mismatch_is_repaired_from_the_schema_alone() {
    let schema = json!({"type": "object", "properties": {
        "count": {"type": "integer"},
        "ratio": {"type": "number"},
        "force": {"type": "boolean"},
        "paths": {"type": "array", "items": {"type": "string"}},
        "options": {"type": "object", "properties": {"depth": {"type": "integer"}}},
        "tags": {"type": "array", "items": {"type": "string"}},
        "limit": {"type": ["integer", "null"]},
        "body": {"type": "string"}
    }});
    let (args, repairs) = repaired(
        json!({
            "count": "42", "ratio": " 0.5 ", "force": "true",
            "paths": "[\"a.rs\", \"b.rs\"]",
            "options": "{\"depth\": \"3\"}",
            "tags": "only-one",
            "limit": "null",
            "body": {"k": 1}
        }),
        schema,
    );
    assert_eq!(
        args,
        json!({
            "count": 42, "ratio": 0.5, "force": true,
            "paths": ["a.rs", "b.rs"],
            "options": {"depth": 3},
            "tags": ["only-one"],
            "limit": null,
            "body": "{\"k\":1}"
        })
    );
    let paths: Vec<&str> = repairs.iter().map(|repair| repair.path.as_str()).collect();
    assert!(
        paths.contains(&"options.depth"),
        "nested values follow their own schema"
    );
    assert_eq!(repairs.len(), 9);
}

#[test]
fn values_that_fit_or_cannot_be_repaired_without_guessing_are_left_alone() {
    let schema = json!({"type": "object", "properties": {
        "count": {"type": "integer"},
        "whole": {"type": "integer"},
        "flag": {"type": "boolean"},
        "list": {"type": "array"},
        "either": {"type": ["string", "integer"]},
        "pick": {"anyOf": [{"type": "array"}, {"type": "object"}]},
        "mode": {"type": "string", "enum": ["fast", "slow"]},
        "free": {},
        "unknown_to_schema": {"type": "string"}
    }});
    let sent = json!({
        "count": "forty-two",
        "whole": "4.5",
        "flag": "yes",
        "list": "[broken",
        "either": [1],
        "pick": "x",
        "mode": ["fast"],
        "free": ["anything"],
        "extra": "kept as sent"
    });
    let (args, repairs) = repaired(sent.clone(), schema.clone());
    assert_eq!(args, sent);
    assert!(repairs.is_empty(), "{repairs:?}");

    let fitting =
        json!({"count": 3, "whole": 4.0, "flag": false, "list": [], "either": "s", "mode": "fast"});
    assert_eq!(repaired(fitting.clone(), schema).0, fitting);
}

#[test]
fn deep_and_odd_shapes_stay_bounded() {
    let mut nested = json!({"type": "integer"});
    for _ in 0..40 {
        nested = json!({"type": "object", "properties": {"n": nested}});
    }
    let mut value = json!("7");
    for _ in 0..40 {
        value = json!({"n": value});
    }
    let (_, repairs) = repaired(value, nested);
    assert!(
        repairs.is_empty(),
        "past the depth limit nothing is touched"
    );
    assert!(
        repaired(json!("not an object"), json!({"type": "object"}))
            .1
            .is_empty()
    );
    assert!(repaired(json!({"a": "1"}), json!(true)).1.is_empty());
}
