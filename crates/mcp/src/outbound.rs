use super::*;

pub(super) fn target(config: &ServerConfig) -> Option<(String, String)> {
    match &config.transport {
        Transport::Remote { url, .. } => Some((url.clone(), screen_identity(&config.id, config))),
        Transport::Stdio { .. } => None,
    }
}

impl McpManager {
    pub(super) async fn outbound_target(
        &self,
        server: &str,
    ) -> Result<Option<(String, String)>, Error> {
        if let Some(link) = self.hub_for(server) {
            return link.outbound(server);
        }
        let config = self.server_config(server).await?;
        Ok(target(&config))
    }

    pub async fn check_outbound(&self, qualified: &str, args: &Value) -> Result<(), Error> {
        let (server, tool) = parse_qualified(qualified)?;
        if let Some((url, _)) = self.outbound_target(&server).await? {
            kernel::egress::peek_tool(&url, &serde_json::json!({"tool":tool,"arguments":args}))
                .map_err(Error::Protocol)?;
        }
        Ok(())
    }

    pub async fn remote_call(&self, qualified: &str) -> Result<bool, Error> {
        let (server, _) = parse_qualified(qualified)?;
        Ok(self.outbound_target(&server).await?.is_some())
    }
}
