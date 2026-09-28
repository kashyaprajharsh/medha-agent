//! Every key Medha uses, on one page: models, web search and MCP servers.
//! Values never leave the credential store; only whether each is set does.
//! A key can be set or removed only for something already configured.

use crate::config;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

const SEARCH: [(&str, &str, tools::SearchProvider); 2] = [
    ("tavily", "Tavily", tools::SearchProvider::Tavily),
    ("brave", "Brave Search", tools::SearchProvider::Brave),
];

pub(crate) fn list(cfg: &config::Config) -> Value {
    let mut endpoints: Vec<(String, Vec<String>)> = Vec::new();
    for profile in cfg.model_profiles() {
        if !profile.provider.auth.requires_credential() {
            continue;
        }
        let url = profile.provider.base_url.clone();
        match endpoints.iter_mut().find(|(known, _)| *known == url) {
            Some((_, names)) => names.push(profile.name),
            None => endpoints.push((url, vec![profile.name])),
        }
    }
    let models: Vec<Value> = endpoints
        .into_iter()
        .map(|(url, names)| {
            json!({"group": "model", "id": url, "label": names.join(", "), "detail": url, "present": config::key_present(&url)})
        })
        .collect();
    let active = cfg.search_provider();
    let search: Vec<Value> = SEARCH
        .iter()
        .map(|(id, label, provider)| {
            let present = config::search_cred_id(*provider).is_some_and(config::key_present);
            json!({"group": "search", "id": id, "label": label, "present": present, "in_use": active == *provider})
        })
        .collect();
    let mcp: Vec<Value> = cfg
        .mcp
        .iter()
        .map(|(id, server)| {
            json!({
                "group": "mcp",
                "id": id,
                "label": id,
                "detail": if server.url.is_empty() { server.command.first().cloned().unwrap_or_default() } else { server.url.clone() },
                "present": config::mcp_key_present(id, server),
                "oauth": server.auth == "oauth",
                "signed_in": config::mcp_signed_in(id, server),
            })
        })
        .collect();
    json!({"models": models, "search": search, "mcp": mcp, "store": config::credential_store_label()})
}

fn endpoint<'a>(cfg: &config::Config, id: &'a str) -> Result<&'a str> {
    cfg.model_profiles()
        .iter()
        .any(|profile| profile.provider.base_url == id)
        .then_some(id)
        .ok_or_else(|| anyhow!("No saved model uses that endpoint"))
}

fn search_id(id: &str) -> Result<&'static str> {
    SEARCH
        .iter()
        .find(|(name, _, _)| *name == id)
        .and_then(|(_, _, provider)| config::search_cred_id(*provider))
        .ok_or_else(|| anyhow!("That search provider has no key"))
}

pub(crate) fn set(cfg: &config::Config, params: &Value) -> Result<()> {
    let id = params["id"].as_str().unwrap_or_default();
    let key = params["key"].as_str().map(str::trim).unwrap_or_default();
    if key.is_empty() {
        bail!("Paste the key first");
    }
    match params["group"].as_str() {
        Some("model") => config::store_key(endpoint(cfg, id)?, key),
        Some("search") => config::store_key(search_id(id)?, key),
        Some("mcp") => {
            let server = cfg
                .mcp
                .get(id)
                .ok_or_else(|| anyhow!("MCP server not found"))?;
            config::store_mcp_key(id, server, key)
        }
        _ => bail!("Unknown key"),
    }
}

pub(crate) fn remove(cfg: &config::Config, params: &Value) -> Result<()> {
    let id = params["id"].as_str().unwrap_or_default();
    match params["group"].as_str() {
        Some("model") => config::remove_key(endpoint(cfg, id)?),
        Some("search") => config::remove_key(search_id(id)?),
        Some("mcp") => {
            let server = cfg
                .mcp
                .get(id)
                .ok_or_else(|| anyhow!("MCP server not found"))?;
            config::remove_mcp_key(id, server)
        }
        _ => bail!("Unknown key"),
    }
}

#[cfg(test)]
#[path = "desktop_keys_tests.rs"]
mod tests;
