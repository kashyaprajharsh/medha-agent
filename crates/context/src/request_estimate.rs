use crate::tokens::TokenCounter;
use serde_json::Value;

pub(crate) fn count(request: &kernel::PreparedModelRequest, counter: &dyn TokenCounter) -> u64 {
    fn walk(value: &Value, counter: &dyn TokenCounter) -> u64 {
        match value {
            Value::String(text) if text.starts_with("data:image/") => 4096,
            Value::String(text) => u64::from(counter.count(text)),
            Value::Array(items) => items
                .iter()
                .map(|value| walk(value, counter).saturating_add(4))
                .sum(),
            Value::Object(fields) => fields
                .iter()
                .map(|(key, value)| {
                    if key == "data" && fields.get("type").and_then(Value::as_str) == Some("base64")
                    {
                        4096
                    } else {
                        u64::from(counter.count(key))
                            .saturating_add(2)
                            .saturating_add(walk(value, counter))
                    }
                })
                .sum(),
            _ => 1,
        }
    }
    walk(&request.body, counter).saturating_add(32)
}

#[cfg(test)]
#[path = "request_estimate_tests.rs"]
mod tests;
