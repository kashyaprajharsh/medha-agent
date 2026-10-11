//! Tool arguments repaired against the tool's own schema.
//!
//! Models often send a value of the wrong JSON type: a number written as text,
//! a list written as JSON text, one value where a list belongs, or a list where
//! the schema asks for text. Each repair here is chosen from two things only,
//! the type that was sent and the type the schema declares, never from a tool,
//! server or model name. A value that already fits is never touched, and a
//! value whose repair is not unambiguous is left for the tool to reject.

use serde_json::Value;

/// One argument that was changed, for the record and for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    /// Where the value sits, such as `elements` or `steps[2].done`.
    pub path: String,
    pub sent: &'static str,
    pub passed: &'static str,
}

/// How deep nested arguments are followed. Schemas nest a few levels at most.
const MAX_DEPTH: usize = 16;

/// Repairs `args` in place against an object schema and lists what changed.
pub fn repair(args: &mut Value, schema: &Value) -> Vec<Repair> {
    let mut repairs = Vec::new();
    walk(args, schema, String::new(), 0, &mut repairs);
    repairs
}

/// Top-level arguments a list or object sent in their place would be passed as
/// JSON text. A surface that shows a call while it is still being written uses
/// these to show it the way the finished call will run.
pub fn text_fields(schema: &Value) -> Vec<String> {
    let probe = Value::Array(Vec::new());
    schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, field)| {
            field.is_object() && !fits(&probe, field) && convert(&probe, field).is_some()
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// One sentence a model can act on, naming each repaired argument.
pub fn note(repairs: &[Repair]) -> Option<String> {
    let changed: Vec<String> = repairs
        .iter()
        .map(|repair| {
            format!(
                "`{}` was sent as {} and passed as {}",
                repair.path,
                article(repair.sent),
                article(repair.passed)
            )
        })
        .collect();
    (!changed.is_empty()).then(|| {
        format!(
            "Medha adjusted this call to match the tool's schema: {}. Send these types directly next time.",
            changed.join("; ")
        )
    })
}

fn article(kind: &str) -> String {
    match kind {
        "array" | "object" | "integer" => format!("an {kind}"),
        "null" => "null".into(),
        _ => format!("a {kind}"),
    }
}

fn walk(value: &mut Value, schema: &Value, path: String, depth: usize, out: &mut Vec<Repair>) {
    if depth > MAX_DEPTH || !schema.is_object() {
        return;
    }
    let sent = kind(value);
    if !fits(value, schema)
        && let Some(repaired) = convert(value, schema)
    {
        let passed = kind(&repaired);
        *value = repaired;
        out.push(Repair {
            path: path.clone(),
            sent,
            passed,
        });
    }
    match value {
        Value::Object(map) => {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return;
            };
            for (key, item) in map.iter_mut() {
                if let Some(field) = properties.get(key) {
                    let path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(item, field, path, depth + 1, out);
                }
            }
        }
        Value::Array(items) => {
            let Some(item_schema) = schema.get("items").filter(|items| items.is_object()) else {
                return;
            };
            for (index, item) in items.iter_mut().enumerate() {
                walk(
                    item,
                    item_schema,
                    format!("{path}[{index}]"),
                    depth + 1,
                    out,
                );
            }
        }
        _ => {}
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The JSON types a schema declares, through `type` or a union of branches.
/// Empty when it declares none, which means anything is accepted.
fn declared(schema: &Value) -> Vec<&str> {
    let mut types: Vec<&str> = match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    for union in ["anyOf", "oneOf"] {
        for branch in schema
            .get(union)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            types.extend(declared(branch));
        }
    }
    if schema.get("nullable") == Some(&Value::Bool(true)) {
        types.push("null");
    }
    types
}

fn fits(value: &Value, schema: &Value) -> bool {
    let types = declared(schema);
    types.is_empty()
        || types.iter().any(|kind| match (*kind, value) {
            ("number", Value::Number(_)) => true,
            ("integer", Value::Number(number)) => {
                number.is_i64()
                    || number.is_u64()
                    || number.as_f64().is_some_and(|n| n.fract() == 0.0)
            }
            (kind, value) => kind == self::kind(value),
        })
}

/// The one value the schema's declared type allows, or `None` when there is no
/// single clear choice. Several declared types make every conversion ambiguous,
/// except text that spells null where null is allowed.
fn convert(value: &Value, schema: &Value) -> Option<Value> {
    let types = declared(schema);
    if let Value::String(text) = value
        && types.contains(&"null")
        && text.trim() == "null"
    {
        return Some(Value::Null);
    }
    let [target] = types
        .iter()
        .copied()
        .filter(|kind| *kind != "null")
        .collect::<Vec<_>>()[..]
    else {
        return None;
    };
    match (target, value) {
        ("integer" | "number", Value::String(text)) => number(text.trim(), target == "integer"),
        ("boolean", Value::String(text)) => match text.trim() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        ("array" | "object", Value::String(text)) => {
            let trimmed = text.trim();
            match serde_json::from_str::<Value>(trimmed) {
                Ok(parsed) if kind(&parsed) == target => Some(parsed),
                // Plain text is one item of the list; broken JSON is not.
                _ if target == "array" && !trimmed.starts_with(['[', '{']) => {
                    Some(Value::Array(vec![value.clone()]))
                }
                _ => None,
            }
        }
        ("array", single) if !single.is_null() => Some(Value::Array(vec![single.clone()])),
        // Text with a fixed set of values or a pattern is a word, not a document.
        ("string", Value::Array(_) | Value::Object(_))
            if schema.get("enum").is_none()
                && schema.get("const").is_none()
                && schema.get("pattern").is_none() =>
        {
            serde_json::to_string(value).ok().map(Value::String)
        }
        _ => None,
    }
}

fn number(text: &str, integer: bool) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    if let Ok(whole) = text.parse::<i64>() {
        return Some(Value::from(whole));
    }
    let real: f64 = text.parse().ok().filter(|n: &f64| n.is_finite())?;
    if !integer {
        return serde_json::Number::from_f64(real).map(Value::Number);
    }
    (real.fract() == 0.0 && real.abs() < 9.0e15).then(|| Value::from(real as i64))
}

#[cfg(test)]
#[path = "arg_repair_tests.rs"]
mod tests;
