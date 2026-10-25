//! Exact blocked file changes, applied only by the shared operator action.

use super::*;
use kernel::protected_paths::{classify, matters};
use std::path::Path;

struct Change {
    sandbox: Arc<WorkspaceSandbox>,
    path: String,
    before: sandbox::GuardedFileState,
    after: String,
}

#[async_trait]
impl kernel::mutations::Apply for Change {
    async fn apply(&self) -> Result<Value, String> {
        let guard = self
            .sandbox
            .path_guard(&self.path)
            .await
            .map_err(|error| error.to_string())?;
        let snapshot = file_checks::commit(&self.sandbox, &guard, &self.after, &self.before)
            .await
            .map_err(|error| error.to_string())?;
        let mut result =
            json!({"path":self.path, "written":true, "verified":true, "snapshot":snapshot});
        use sha2::{Digest, Sha256};
        result["expected"] = json!(format!("{:x}", Sha256::digest(self.after.as_bytes())));
        Ok(result)
    }
}

pub(crate) fn check(
    sandbox: &Arc<WorkspaceSandbox>,
    resolved: &std::path::Path,
    requested: &str,
    before: &sandbox::GuardedInspection,
    after: &str,
) -> Result<(), ToolError> {
    let context = kernel::egress::mutation_context();
    let web = context.as_ref().is_some_and(|(_, web)| *web);
    let requested_path = sandbox.root().join(requested);
    let kinds = [classify(&requested_path, web), classify(resolved, web)];
    if !kinds
        .into_iter()
        .flatten()
        .any(|kind| matters(kind, before.bytes.as_deref(), Some(after.as_bytes())))
    {
        return Ok(());
    }
    let Some((reviews, _)) = context else {
        return Err(ToolError::Security(
            "Protected writes require a chat-owned exact review; nothing was written".into(),
        ));
    };
    let path = resolved.to_string_lossy().into_owned();
    let identity = format!("{path}\0{:?}\0{after}", before.state);
    let old = before
        .bytes
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .unwrap_or("[non-text prior content]");
    let diff = make_diff(&path, old, after);
    let id = reviews
        .propose(
            identity.as_bytes(),
            &path,
            "changes project instructions or delayed execution settings",
            &diff,
            Arc::new(Change {
                sandbox: sandbox.clone(),
                path: path.clone(),
                before: before.state.clone(),
                after: after.into(),
            }),
        )
        .map_err(ToolError::Security)?;
    Err(ToolError::Structured(
        json!({"error":"Protected change left unrun; the person can review its exact diff and Allow once", "changed":false, "mutation":{"id":id}}),
    ))
}

pub(crate) async fn preflight(
    sandbox: &Arc<WorkspaceSandbox>,
    args: &Value,
    ledger: Option<&read_revisions::Revisions>,
) -> Result<(), ToolError> {
    let Some(path) = args["path"].as_str() else {
        return Ok(());
    };
    let permitted = sandbox.resolve_if_permitted(path).await;
    let target = permitted
        .clone()
        .unwrap_or_else(|| sandbox.root().join(path));
    // The start of the file decides whether it is binary, whatever its name says.
    let existing = target.is_file().then(|| {
        use std::io::Read;
        let mut start = Vec::new();
        if let Ok(file) = std::fs::File::open(&target) {
            let _ = file.take(8192).read_to_end(&mut start);
        }
        start
    });
    if let Some(refusal) =
        crate::documents::text_write_refusal(Path::new(path), existing.as_deref())
    {
        return Err(ToolError::Args(refusal));
    }
    let Some(resolved) = permitted else {
        return Ok(());
    };
    let web = kernel::egress::mutation_context().is_some_and(|(_, web)| web);
    if classify(&sandbox.root().join(path), web).is_none() && classify(&resolved, web).is_none() {
        return Ok(());
    }
    let Some(before) = sandbox.inspect_if_permitted(path).await.ok().flatten() else {
        return Ok(());
    };
    let old = before
        .bytes
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .unwrap_or_default();
    let after = if let Some(content) = args["content"].as_str() {
        if ledger.is_some_and(|ledger| !ledger.permits(&resolved, &before.state)) && content != old
        {
            return Err(ToolError::Args(format!(
                "Read all current pages of {path} before replacing the whole file, or use an exact targeted edit. A page too long to show in full does not count: read it again with a smaller `limit`."
            )));
        }
        content.to_owned()
    } else if let Some(edits) = args["edits"].as_array() {
        apply_edits(old, edits)?.0
    } else if let (Some(find), Some(replace)) =
        (args["old_string"].as_str(), args["new_string"].as_str())
    {
        edit_match::replace(old, find, replace, args["replace_all"] == true)
            .map_err(|miss| ToolError::Failed(missed(miss, path)))?
            .text
    } else {
        return Ok(());
    };
    check(sandbox, &resolved, path, &before, &after)
}

#[cfg(test)]
#[path = "protected_writes_tests.rs"]
mod tests;
