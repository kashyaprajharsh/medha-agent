//! Ephemeral local MCP definitions owned by one chat, separate from user and
//! plugin configuration and from the shared trusted remote host.
use anyhow::{Result, ensure};
use protocol::SessionMcpServer;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub fn validate(servers: &[SessionMcpServer]) -> Result<()> {
    ensure!(
        servers.len() <= 16,
        "At most 16 session MCP servers are supported"
    );
    let mut names = HashSet::new();
    let encoded = serde_json::to_vec(servers)?;
    ensure!(
        encoded.len() <= 1024 * 1024,
        "Session MCP configuration exceeds 1 MiB"
    );
    for server in servers {
        ensure!(
            !server.name.is_empty() && server.name.len() <= 128 && !server.name.contains('\0'),
            "Invalid session MCP name"
        );
        ensure!(names.insert(&server.name), "Duplicate session MCP name");
        ensure!(
            server.command.is_absolute(),
            "Session MCP commands must be absolute paths"
        );
        ensure!(
            !server.command.as_os_str().is_empty(),
            "Empty session MCP command"
        );
        ensure!(
            server.args.len() <= 128 && server.env.len() <= 128,
            "Too many MCP arguments or environment entries"
        );
        ensure!(
            server.args.iter().all(|arg| !arg.contains('\0')),
            "MCP arguments contain NUL"
        );
        for (name, value) in &server.env {
            ensure!(
                !name.is_empty() && !name.contains(['=', '\0']) && !value.contains('\0'),
                "Invalid MCP environment entry"
            );
        }
    }
    Ok(())
}

pub fn identity(servers: &[SessionMcpServer]) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let mut sorted: Vec<_> = servers.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    Some(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&sorted).expect("MCP input serializes"))
    ))
}

pub(crate) fn definitions(servers: &[SessionMcpServer]) -> Vec<mcp::ServerConfig> {
    servers
        .iter()
        .map(|server| {
            // Names cannot collide with user/plugin ids, including names that are
            // distinct in an editor but normalize to the same tool identifier.
            let digest = format!("{:x}", Sha256::digest(server.name.as_bytes()));
            let id = format!("editor_{}", &digest[..16]);
            mcp::ServerConfig {
                id,
                transport: mcp::Transport::Stdio {
                    command: std::iter::once(server.command.to_string_lossy().into_owned())
                        .chain(server.args.iter().cloned())
                        .collect(),
                    env: server
                        .env
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                },
                // Supplied by the authenticated local editor, still subject to the
                // same per-chat jail and model tool-effect approval policy.
                requires_approval: false,
                ..Default::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn server(name: &str) -> SessionMcpServer {
        SessionMcpServer {
            name: name.into(),
            command: std::env::current_exe().unwrap(),
            args: Vec::new(),
            env: Default::default(),
        }
    }
    #[test]
    fn identity_is_order_independent_and_credentials_are_redacted() {
        let mut a = server("one");
        a.env.insert("TOKEN".into(), "private-token".into());
        let b = server("two");
        assert_eq!(identity(&[a.clone(), b.clone()]), identity(&[b, a.clone()]));
        assert!(!format!("{a:?}").contains("private-token"));
        assert!(validate(&[a.clone(), a]).is_err());
    }
    #[test]
    fn server_names_are_distinct_without_normalization_collisions() {
        let definitions = definitions(&[server("a-b"), server("a_b")]);
        assert_ne!(definitions[0].id, definitions[1].id);
        assert!(
            validate(&[SessionMcpServer {
                command: "relative".into(),
                ..server("x")
            }])
            .is_err()
        );
    }
}
