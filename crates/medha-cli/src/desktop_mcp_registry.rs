//! Browsing the public MCP Registry. Entries are third-party and unreviewed:
//! each becomes a pre-filled add form, and connecting still goes through the
//! same review as a server added by hand. Only setups Medha can run are offered;
//! the rest say why not.

use serde_json::{Value, json};
use std::time::Duration;

const REGISTRY: &str = "https://registry.modelcontextprotocol.io/v0/servers";

pub(crate) async fn search(query: &str, cursor: Option<&str>) -> anyhow::Result<Value> {
    let mut request = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?
        .get(REGISTRY)
        .query(&[("limit", "30"), ("version", "latest")]);
    if !query.trim().is_empty() {
        request = request.query(&[("search", query.trim())]);
    }
    if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
        request = request.query(&[("cursor", cursor)]);
    }
    let response = request.send().await?;
    if !response.status().is_success() {
        anyhow::bail!("The MCP Registry answered {}", response.status());
    }
    Ok(listing(&response.json::<Value>().await?))
}

/// Active servers with the ways Medha can set each one up.
pub(crate) fn listing(page: &Value) -> Value {
    let servers: Vec<Value> = page["servers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| {
            let status = &row["_meta"]["io.modelcontextprotocol.registry/official"]["status"];
            status.is_null() || status == "active"
        })
        .map(|row| {
            let server = &row["server"];
            let mut setups: Vec<Value> = server["remotes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(remote)
                .collect();
            setups.extend(
                server["packages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(package),
            );
            json!({
                "name": server["name"],
                "title": server["title"],
                "description": server["description"],
                "version": server["version"],
                "repository": server["repository"]["url"],
                "setups": setups,
            })
        })
        .collect();
    json!({"servers": servers, "next": page["metadata"]["nextCursor"]})
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn remote(remote: &Value) -> Value {
    let url = text(&remote["url"]);
    let unsupported = |reason: &str| json!({"kind": "remote", "url": url, "unsupported": reason});
    if !matches!(text(&remote["type"]), "streamable-http" | "sse") {
        return unsupported("uses a transport Medha does not support");
    }
    if url.contains('{') {
        return unsupported("needs values filled into its URL");
    }
    let headers: Vec<&Value> = remote["headers"].as_array().into_iter().flatten().collect();
    let sign_in = match headers.as_slice() {
        [] => "detect",
        [header]
            if text(&header["name"]).eq_ignore_ascii_case("authorization")
                && text(&header["value"]).starts_with("Bearer ") =>
        {
            "token"
        }
        _ => return unsupported("needs custom request headers"),
    };
    json!({
        "kind": "remote",
        "url": url,
        "sign_in": sign_in,
        "token_help": headers.first().map(|header| text(&header["description"])),
    })
}

fn package(package: &Value) -> Value {
    let identifier = text(&package["identifier"]);
    let version = text(&package["version"]);
    let unsupported =
        |reason: &str| json!({"kind": "local", "package": identifier, "unsupported": reason});
    if text(&package["transport"]["type"]) != "stdio" {
        return unsupported("runs as a network service, not a local process");
    }
    let mut command = match text(&package["registryType"]) {
        "npm" => vec![
            "npx".to_owned(),
            "-y".to_owned(),
            versioned(identifier, version, "@"),
        ],
        "pypi" => vec!["uvx".to_owned(), versioned(identifier, version, "==")],
        other => {
            return unsupported(&format!(
                "is a {other} package, which Medha cannot start yet"
            ));
        }
    };
    for argument in package["packageArguments"].as_array().into_iter().flatten() {
        let value = argument["value"]
            .as_str()
            .or_else(|| argument["default"].as_str());
        match (value, argument["isRequired"].as_bool().unwrap_or(false)) {
            (Some(value), _) => {
                if let Some(flag) = argument["name"]
                    .as_str()
                    .filter(|_| argument["type"] == "named")
                {
                    command.push(flag.to_owned());
                }
                command.push(value.to_owned());
            }
            (None, true) => return unsupported("needs arguments Medha cannot fill in"),
            (None, false) => {}
        }
    }
    let variables: Vec<Value> = package["environmentVariables"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|variable| {
            json!({
                "name": variable["name"],
                "description": variable["description"],
                "required": variable["isRequired"].as_bool().unwrap_or(false),
                "secret": variable["isSecret"].as_bool().unwrap_or(false),
                "default": variable["default"],
            })
        })
        .collect();
    json!({"kind": "local", "package": identifier, "command": command, "variables": variables})
}

/// A short server id from a registry name like `io.github.org/tool-mcp`.
pub(crate) fn short_name(name: &str) -> String {
    let last = name.rsplit('/').next().unwrap_or(name).to_lowercase();
    let trimmed = ["-mcp-server", "_mcp_server", "-mcp", "_mcp", "mcp-", "mcp"]
        .iter()
        .find_map(|affix| {
            last.strip_suffix(affix)
                .or_else(|| last.strip_prefix(affix))
        })
        .filter(|rest| !rest.is_empty())
        .unwrap_or(&last);
    let id: String = trimmed
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let id = id.trim_matches('-').to_owned();
    if id.is_empty() { "server".into() } else { id }
}

/// The `/mcp add` line for one setup, and where the cursor belongs: after
/// `--key` when a secret is needed, else at the first value to fill in.
pub(crate) fn add_command(name: &str, setup: &Value) -> Option<(String, usize)> {
    if setup.get("unsupported").is_some() {
        return None;
    }
    let mut line = format!("/mcp add {}", short_name(name));
    if setup["kind"] == "remote" {
        line.push_str(&format!(" --url {}", text(&setup["url"])));
        if setup["sign_in"] == "token" {
            line.push_str(" --bearer ");
        }
        return Some((line.clone(), line.len()));
    }
    let variables: Vec<&Value> = setup["variables"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let secret = variables.iter().find(|variable| variable["secret"] == true);
    let mut cursor = None;
    for variable in &variables {
        let name = text(&variable["name"]);
        if name.is_empty() {
            continue;
        }
        if Some(variable) == secret {
            line.push_str(&format!(" --env {name}=${{key}}"));
            continue;
        }
        let default = variable["default"].as_str().unwrap_or_default();
        if default.is_empty() && variable["required"] != true {
            continue;
        }
        line.push_str(&format!(" --env {name}={default}"));
        if default.is_empty() {
            cursor.get_or_insert(line.len());
        }
    }
    if secret.is_some() {
        line.push_str(" --key ");
        cursor = Some(line.len());
    }
    let command: Vec<&str> = setup["command"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    line.push_str(&format!(" -- {}", command.join(" ")));
    let end = line.len();
    Some((line, cursor.unwrap_or(end)))
}

fn versioned(identifier: &str, version: &str, separator: &str) -> String {
    if version.is_empty() {
        identifier.to_owned()
    } else {
        format!("{identifier}{separator}{version}")
    }
}

#[cfg(test)]
#[path = "desktop_mcp_registry_tests.rs"]
mod tests;
