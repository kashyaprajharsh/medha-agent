//! Child patches use the existing exact merge check through the quiet route.

use super::*;

struct Change {
    control: Arc<orchestrator::AgentControl>,
    pending: orchestrator::Pending,
    force: bool,
}

#[async_trait]
impl kernel::mutations::Apply for Change {
    async fn apply(&self) -> Result<Value, String> {
        let current = self
            .control
            .pending(&self.pending.dispatch)
            .await
            .ok_or("That patch is no longer available")?;
        if current.patch.diff != self.pending.patch.diff
            || current.patch.base != self.pending.patch.base
            || current.patch.files != self.pending.patch.files
        {
            return Err("That patch changed; review its current exact diff".into());
        }
        let result = self
            .control
            .merge(&self.pending.patch, self.force)
            .await
            .map_err(|error| error.to_string())?;
        self.control.forget(&self.pending.dispatch).await;
        Ok(json!({"applied":true,"files":self.pending.patch.files,"merge":result}))
    }
}

pub(crate) async fn check(
    control: &Arc<orchestrator::AgentControl>,
    args: &Value,
) -> Result<(), ToolError> {
    let Some(id) = args["patch_id"].as_str() else {
        return Ok(());
    };
    let Some((reviews, web)) = kernel::egress::mutation_context() else {
        return Ok(());
    };
    let Some(pending) = control.pending(id).await else {
        return Ok(());
    };
    if pending.dispatch != id
        || !pending.patch.files.iter().any(|path| {
            kernel::protected_paths::classify(std::path::Path::new(path), web).is_some()
        })
    {
        return Ok(());
    }
    let identity =
        serde_json::to_vec(&json!({"id":id,"patch":pending.patch,"force":args["force"]}))
            .map_err(|error| ToolError::Failed(error.to_string()))?;
    let preview = if args["force"] == true {
        format!("Verification override requested.\n\n{}", pending.patch.diff)
    } else {
        pending.patch.diff.clone()
    };
    let review = reviews
        .propose(
            &identity,
            &pending.patch.files.join(", "),
            "changes protected paths in a child patch",
            &preview,
            Arc::new(Change {
                control: control.clone(),
                pending: pending.clone(),
                force: args["force"] == true,
            }),
        )
        .map_err(ToolError::Security)?;
    Err(ToolError::Structured(
        json!({"error":"Protected patch left unapplied; review its exact diff and Allow once", "changed":false, "mutation":{"id":review}}),
    ))
}
