//! Tool descriptions without the rules some local servers cannot turn into a grammar.

use serde_json::Value;

/// Remove `pattern` and `format` from every tool's schema in a chat request
/// body. Returns how many were removed.
pub(crate) fn strip(body: &mut Value) -> usize {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return 0;
    };
    let schemas = tools
        .iter_mut()
        .filter_map(|tool| tool.pointer_mut("/function/parameters"));
    schemas.map(plain).sum()
}

/// The walk follows a schema's own shape, never every key: a property may
/// itself be named `pattern`, and that one is the tool's and must stay.
fn plain(schema: &mut Value) -> usize {
    let Some(node) = schema.as_object_mut() else {
        return 0;
    };
    let mut removed = 0;
    for rule in ["pattern", "format"] {
        if node.get(rule).is_some_and(Value::is_string) {
            node.remove(rule);
            removed += 1;
        }
    }
    for (key, inner) in node.iter_mut() {
        removed += match (key.as_str(), inner) {
            (
                "properties" | "$defs" | "definitions" | "patternProperties",
                Value::Object(named),
            ) => named.values_mut().map(plain).sum(),
            ("anyOf" | "oneOf" | "allOf" | "prefixItems" | "items", Value::Array(all)) => {
                all.iter_mut().map(plain).sum()
            }
            ("items" | "additionalProperties" | "not" | "contains", one) => plain(one),
            _ => 0,
        };
    }
    removed
}

#[cfg(test)]
#[path = "schema_rules_tests.rs"]
mod tests;
