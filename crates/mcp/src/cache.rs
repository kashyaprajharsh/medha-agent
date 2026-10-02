//! Last-known tool lists, used only while a server's connection settings are unchanged.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Catalog, McpToolSpec, RemoteAuth, ServerConfig, Transport};

#[derive(Serialize, Deserialize)]
struct Entry {
    fingerprint: String,
    exposed: Vec<McpToolSpec>,
    hidden: Vec<String>,
    #[serde(default)]
    app_only: Vec<McpToolSpec>,
}

/// Credentials count by kind only, so a rotated token keeps the entry.
pub(crate) fn fingerprint(server: &ServerConfig) -> String {
    let mut hash = Sha256::new();
    let mut part = |text: &str| {
        hash.update(text.as_bytes());
        hash.update([0]);
    };
    part(&server.id);
    match &server.transport {
        Transport::Stdio { command, env } => {
            part("stdio");
            command.iter().for_each(|arg| part(arg));
            for (key, value) in env {
                part(key);
                part(value);
            }
        }
        Transport::Remote { url, auth } => {
            part("http");
            part(url);
            part(match auth {
                RemoteAuth::Auto => "auto",
                RemoteAuth::None => "none",
                RemoteAuth::Bearer(_) => "bearer",
                RemoteAuth::OAuth => "oauth",
            });
        }
    }
    part("allow");
    server.tools.allow.iter().for_each(|pattern| part(pattern));
    part("deny");
    server.tools.deny.iter().for_each(|pattern| part(pattern));
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn path(dir: &Path, server: &str) -> PathBuf {
    let name: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{name}.json"))
}

pub(crate) fn load(dir: &Path, server: &ServerConfig) -> Option<Catalog> {
    let text = std::fs::read_to_string(path(dir, &server.id)).ok()?;
    let entry: Entry = serde_json::from_str(&text).ok()?;
    (entry.fingerprint == fingerprint(server)).then_some(Catalog {
        exposed: entry.exposed,
        hidden: entry.hidden,
        app_only: entry.app_only,
    })
}

pub(crate) fn save(dir: &Path, server: &ServerConfig, catalog: &Catalog) {
    let entry = Entry {
        fingerprint: fingerprint(server),
        exposed: catalog.exposed.clone(),
        hidden: catalog.hidden.clone(),
        app_only: catalog.app_only.clone(),
    };
    let Ok(text) = serde_json::to_string(&entry) else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let target = path(dir, &server.id);
    // Per process: the host and a chat may save the same server at once.
    let staged = target.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&staged, text).is_ok() {
        let _ = std::fs::rename(&staged, &target);
    }
}
