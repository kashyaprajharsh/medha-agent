//! Finding the shared MCP host, and which servers may be handed to it.

/// The token travels in the environment: other local processes can read argv.
pub const ADDRESS_ENV: &str = "MEDHA_MCP_HOST";
pub const TOKEN_ENV: &str = "MEDHA_MCP_HOST_TOKEN";

pub fn endpoint() -> Option<mcp::hub::Endpoint> {
    let address = std::env::var(ADDRESS_ENV)
        .ok()
        .filter(|value| !value.is_empty())?;
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|value| !value.is_empty())?;
    Some(mcp::hub::Endpoint { address, token })
}

/// One needing approval stays per chat, so an approval never reaches other chats.
pub fn is_shared(server: &crate::config::McpServer) -> bool {
    !server.url.is_empty() && server.command.is_empty() && server.trust == "trusted"
}
