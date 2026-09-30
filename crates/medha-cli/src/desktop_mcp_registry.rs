//! Browsing the public MCP Registry. Entries are third-party and unreviewed:
//! each becomes a pre-filled add form, and connecting still goes through the
//! same review as a server added by hand. Only setups Medha can run are offered;
//! the rest say why not.

use serde_json::{Value, json};
use std::time::Duration;

const REGISTRY: &str = "https://registry.modelcontextprotocol.io/v0/servers";
/// GitHub's curated list: the same server format, ranked by stars, and fast to search.
const FEATURED: &str = "https://api.mcp.github.com/v0.1/servers";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Featured,
    Full,
}

pub(crate) async fn search(
    query: &str,
    cursor: Option<&str>,
    source: Source,
) -> anyhow::Result<Value> {
    let name = match source {
        Source::Featured => "GitHub’s MCP list",
        Source::Full => "The MCP Registry",
    };
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?
        .get(search_url(query, cursor, source)?)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("{name} took too long to answer. Try again in a moment.")
            } else {
                anyhow::anyhow!("Couldn't reach {name}. Check your connection and try again.")
            }
        })?;
    if !response.status().is_success() {
        anyhow::bail!("{name} answered {}", response.status());
    }
    Ok(listing(&response.json::<Value>().await?))
}

fn search_url(query: &str, cursor: Option<&str>, source: Source) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(match source {
        Source::Featured => FEATURED,
        Source::Full => REGISTRY,
    })?;
    let mut params = url.query_pairs_mut();
    params.append_pair("limit", "30");
    let cursor = cursor.filter(|cursor| !cursor.is_empty());
    match source {
        // GitHub also supports numbered pages. Use that API instead of its
        // opaque bookmarks, which have been rejected on subsequent requests.
        Source::Featured => {
            let page = cursor.unwrap_or("1").parse::<u64>()?;
            anyhow::ensure!(page > 0, "Invalid catalogue page");
            params.append_pair("page", &page.to_string());
        }
        Source::Full => {
            params.append_pair("version", "latest");
            if let Some(cursor) = cursor {
                params.append_pair("cursor", cursor);
            }
        }
    }
    if !query.trim().is_empty() {
        params.append_pair("search", query.trim());
    }
    drop(params);
    Ok(url)
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
            let github = &server["_meta"]["io.modelcontextprotocol.registry/publisher-provided"]["github"];
            let owner = text(&github["nameWithOwner"]).split('/').next().unwrap_or_default();
            json!({
                "name": server["name"],
                "title": if server["title"].is_null() { &github["displayName"] } else { &server["title"] },
                "description": server["description"],
                "version": server["version"],
                "repository": server["repository"]["url"],
                "stars": github["stargazerCount"],
                "organization": (github["isInOrganization"] == true && !owner.is_empty()).then_some(owner),
                "setups": setups,
            })
        })
        .collect();
    let next = match (
        page["metadata"]["page"].as_u64(),
        page["metadata"]["total_pages"].as_u64(),
    ) {
        (Some(current), Some(total)) => json!((current < total).then(|| (current + 1).to_string())),
        _ => page["metadata"]["nextCursor"].clone(),
    };
    json!({"servers": servers, "next": next})
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
                && (text(&header["value"]).is_empty()
                    || text(&header["value"]).starts_with("Bearer ")) =>
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
    let runtime = text(&package["runtimeHint"]);
    let (mut command, package_name) = match text(&package["registryType"]) {
        "npm" if runtime.is_empty() || runtime == "npx" => (
            vec!["npx".to_owned(), "-y".to_owned()],
            Some(versioned(identifier, version, "@")),
        ),
        "pypi" if runtime.is_empty() || runtime == "uvx" => (
            vec!["uvx".to_owned()],
            Some(versioned(identifier, version, "==")),
        ),
        // Some curated entries describe an installed executable, such as
        // `uv --directory {path} run server.py`, rather than a registry package.
        "other" if !runtime.is_empty() && runtime == identifier => (vec![runtime.to_owned()], None),
        other => {
            return unsupported(&format!(
                "is a {other} package, which Medha cannot start yet"
            ));
        }
    };
    let mut inputs = Vec::new();
    if let Err(reason) = arguments(&package["runtimeArguments"], &mut command, &mut inputs) {
        return unsupported(reason);
    }
    // The curated list can include the CLI and its subcommand in the runtime
    // arguments already. Appending the package again would break that command.
    let has_cli = package["runtimeArguments"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|arg| arg["type"] == "positional" && arg["valueHint"] == "cli");
    if let Some(package_name) = package_name.filter(|_| !has_cli) {
        command.push(package_name);
    }
    if let Err(reason) = arguments(&package["packageArguments"], &mut command, &mut inputs) {
        return unsupported(reason);
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
                "default": variable.get("value").unwrap_or(&variable["default"]),
            })
        })
        .collect();
    json!({"kind": "local", "package": identifier, "command": command, "variables": variables, "inputs": inputs})
}

/// Arguments are argv entries, never shell fragments. A named argument with
/// no value is a flag; curated entries can put its value in the next entry.
fn arguments(
    arguments: &Value,
    command: &mut Vec<String>,
    inputs: &mut Vec<Value>,
) -> Result<(), &'static str> {
    for argument in arguments.as_array().into_iter().flatten() {
        let value = argument["value"]
            .as_str()
            .or_else(|| argument["default"].as_str());
        let flag = argument["name"]
            .as_str()
            .filter(|name| !name.is_empty() && argument["type"] == "named");
        match (value, argument["isRequired"].as_bool().unwrap_or(false)) {
            (Some(value), _) => {
                if let Some(variables) = argument["variables"].as_object() {
                    for (name, variable) in variables {
                        if !value.contains(&format!("{{{name}}}")) {
                            continue;
                        }
                        if variable["isSecret"] == true {
                            return Err("needs a secret in its command arguments");
                        }
                        if !inputs.iter().any(|input| input["name"] == *name) {
                            inputs.push(json!({
                                "name": name,
                                "description": variable["description"],
                                "default": variable["default"],
                            }));
                        }
                    }
                }
                if let Some(flag) = flag {
                    command.push(flag.to_owned());
                }
                command.push(value.to_owned());
            }
            (None, _) if flag.is_some() => command.push(flag.unwrap().to_owned()),
            (None, true) => return Err("needs arguments Medha cannot fill in"),
            (None, false) => {}
        }
    }
    Ok(())
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
    if version.is_empty() || version == "latest" {
        identifier.to_owned()
    } else {
        format!("{identifier}{separator}{version}")
    }
}

#[cfg(test)]
#[path = "desktop_mcp_registry_tests.rs"]
mod tests;
