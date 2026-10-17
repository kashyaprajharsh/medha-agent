use super::*;

impl ModelEnv {
    pub fn key_for(&self, deployment: &str) -> Result<Option<&str>> {
        let Some(key) = self
            .api_key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        let paired = self
            .base_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "MEDHA_API_KEY requires MEDHA_BASE_URL for the exact deployment it belongs to"
                )
            })?;
        // A trailing slash names the same deployment.
        let same = |url: &str| url.trim().trim_end_matches('/').to_owned();
        if same(paired) != same(deployment) {
            tracing::warn!(
                paired = paired.trim(),
                deployment = deployment.trim(),
                "MEDHA_API_KEY not sent: MEDHA_BASE_URL names another deployment"
            );
            return Ok(None);
        }
        Ok(Some(key))
    }
}

#[cfg(test)]
#[path = "env_key_tests.rs"]
mod tests;
