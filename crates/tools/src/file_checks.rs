//! Shared publication results and inexpensive data-format diagnostics.

use super::*;

pub(crate) async fn commit(
    sandbox: &WorkspaceSandbox,
    guard: &sandbox::WritePathGuard,
    content: &str,
    state: &sandbox::GuardedFileState,
) -> Result<Option<String>, ToolError> {
    sandbox.write_guarded_checked(guard, content, Some(state)).await.map_err(|error| {
        if let sandbox::SandboxError::Published { snapshot, message } = error {
            ToolError::Structured(json!({"path":guard.resolved(), "written":true, "changed":true,
                "verified":false, "snapshot":snapshot, "expected":"unknown", "error":message,
                "note":"The write landed; reread before continuing. No rollback over concurrent work was attempted."}))
        } else { ToolError::Failed(error.to_string()) }
    })
}

fn syntax_error(path: &str, text: &str) -> Option<Result<(), String>> {
    match std::path::Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        "json" => Some(
            serde_json::from_str::<Value>(text)
                .map(|_| ())
                .map_err(|error| error.to_string()),
        ),
        "toml" => Some(
            toml::from_str::<toml::Value>(text)
                .map(|_| ())
                .map_err(|error| error.to_string()),
        ),
        "yaml" | "yml" => Some(
            serde_saphyr::from_str::<serde::de::IgnoredAny>(text)
                .map(|_| ())
                .map_err(|error| error.to_string()),
        ),
        _ => None,
    }
}

pub(crate) fn annotate(payload: &mut Value, path: &str, before: &str, after: &str) {
    payload["verified"] = json!(true);
    payload["changed"] = json!(before != after || payload.get("snapshot") == Some(&Value::Null));
    use sha2::{Digest, Sha256};
    payload["expected"] = json!(format!("{:x}", Sha256::digest(after.as_bytes())));
    let Some(current) = syntax_error(path, after) else {
        return;
    };
    let previous = if payload.get("snapshot") == Some(&Value::Null) {
        Ok(())
    } else {
        syntax_error(path, before).unwrap_or(Ok(()))
    };
    payload["syntax"] = match (previous, current) {
        (Ok(()), Err(error)) => json!({"status":"checked", "introduced":[error], "resolved":[]}),
        (Err(error), Ok(())) => json!({"status":"checked", "introduced":[], "resolved":[error]}),
        (Ok(()), Ok(())) => json!({"status":"checked", "introduced":[], "resolved":[]}),
        (Err(old), Err(current)) if old == current => {
            json!({"status":"checked", "introduced":[], "resolved":[]})
        }
        (Err(_), Err(current)) => {
            json!({"status":"baseline_invalid", "delta":"unknown", "current":current})
        }
    };
}

#[cfg(test)]
#[path = "file_checks_tests.rs"]
mod tests;
