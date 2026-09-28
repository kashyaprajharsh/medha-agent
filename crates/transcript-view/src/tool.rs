use serde_json::{Value, json};

pub(crate) const INPUT_LIMIT: usize = 4_000;
pub(crate) const OUTPUT_LIMIT: usize = 8_000;

/// One name per tool. The combined `agent` tool picks its verb with `action`,
/// and the `fs.*` spellings are the same tools as their short names.
pub(crate) fn canonical(tool: &str, args: &Value) -> String {
    match (tool, args.get("action").and_then(Value::as_str)) {
        ("agent", Some(action)) => format!("agent.{action}"),
        ("fs.read" | "read_artifact", _) => "read".into(),
        ("fs.list" | "tree", _) => "ls".into(),
        ("fs.edit", _) => "edit".into(),
        ("fs.write", _) => "write".into(),
        _ => tool.into(),
    }
}

/// Why a step failed or was refused.
pub fn failure_detail(output: &Value) -> Option<String> {
    output
        .get("error")
        .or_else(|| output.get("reason"))
        .and_then(Value::as_str)
        .map(|text| clip(text, 300))
}

/// The steps of an `update_plan` call, as `{explanation, steps: [{title, status}]}`.
pub fn plan(tool: &str, args: &Value) -> Option<Value> {
    if tool != "update_plan" {
        return None;
    }
    let steps: Vec<Value> = args
        .get("steps")?
        .as_array()?
        .iter()
        .filter_map(|step| {
            let title = ["title", "step", "content"]
                .iter()
                .find_map(|key| step.get(key)?.as_str())?;
            let status = step
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending");
            Some(json!({ "title": clip(title, 200), "status": status }))
        })
        .collect();
    Some(json!({
        "explanation": args.get("explanation").and_then(Value::as_str).map(|text| clip(text, 300)),
        "steps": steps,
    }))
}

/// A tool with no view of its own, as `key: value` lines rather than JSON.
/// Keys starting with `_` are bookkeeping for the model and are left out.
pub(crate) fn fields(value: &Value, limit: usize) -> Option<String> {
    let lines: Vec<String> = value
        .as_object()?
        .iter()
        .filter(|(key, value)| !key.starts_with('_') && !value.is_null())
        .map(|(key, value)| match value {
            Value::String(text) if text.contains('\n') => {
                format!("{key}:\n  {}", text.trim_end().replace('\n', "\n  "))
            }
            Value::String(text) => format!("{key}: {text}"),
            other => format!("{key}: {}", clip(&other.to_string(), 200)),
        })
        .collect();
    (!lines.is_empty()).then(|| bounded(&lines.join("\n"), limit))
}

pub(crate) fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

pub(crate) fn counted(count: u64, one: &str, many: &str) -> String {
    format!("{} {}", grouped(count), if count == 1 { one } else { many })
}

pub(crate) fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// First line only, at most `max` characters, with an ellipsis when cut.
pub fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    let line = text.lines().next().unwrap_or_default();
    let cut = line.chars().count() > max || text.lines().nth(1).is_some();
    let kept: String = line.chars().take(max).collect();
    if cut { format!("{kept}…") } else { kept }
}

pub(crate) fn bounded(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}\n… {} more characters", total - max)
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;
