//! Community plugin folders in the existing `.claude-plugin/plugin.json` layout.
//!
//! Part of the compat adapter: skills, commands, hooks, and MCP servers are
//! mapped onto Medha components; anything else is named in the description.

use super::convert_section;
use crate::package::{Package, PackageSource, read_text};
use crate::sources::{GitSource, Listing};
use crate::{Error, ExtensionComponent, Manifest, RequestedPermissions};
use serde_json::Value;
use std::path::Path;

const MANIFEST: &str = ".claude-plugin/plugin.json";
const ROOT_VAR: &str = "${CLAUDE_PLUGIN_ROOT}";
const DEFAULT_HOOKS: &str = "hooks/hooks.json";
const DEFAULT_MCP: &str = ".mcp.json";

const MARKETPLACE: &str = ".claude-plugin/marketplace.json";

pub(crate) fn is_plugin(dir: &Path) -> bool {
    dir.join(MANIFEST).is_file()
}

/// A marketplace repository's name and the plugins it lists. Relative sources
/// resolve inside the same repository at the same ref.
pub(crate) fn read_marketplace(
    dir: &Path,
    source: &GitSource,
) -> Result<(String, Vec<Listing>), Error> {
    let path = dir.join(MARKETPLACE);
    if !path.is_file() {
        return Err(Error::Source(format!(
            "{} has no marketplace index ({MARKETPLACE})",
            source.url
        )));
    }
    let index = read_json(&path)?;
    let name = index
        .get("name")
        .and_then(Value::as_str)
        .map(slug)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::Manifest(format!("{} has no name", path.display())))?;
    let mut listings = Vec::new();
    for plugin in index
        .get("plugins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(plugin_name) = plugin.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(plugin_source) = listing_source(plugin.get("source"), source) else {
            continue;
        };
        listings.push(Listing {
            name: plugin_name.to_string(),
            description: plugin
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            category: plugin
                .get("category")
                .and_then(Value::as_str)
                .map(str::to_string),
            source: plugin_source,
            entry: listing_entry(plugin),
        });
    }
    Ok((name, listings))
}

fn listing_source(value: Option<&Value>, market: &GitSource) -> Option<GitSource> {
    match value? {
        Value::String(path) if path.starts_with('.') => {
            let subdir = path.trim_start_matches("./").trim_end_matches('/');
            Some(GitSource {
                url: market.url.clone(),
                git_ref: market.git_ref.clone(),
                subdir: Some(subdir.to_string())
                    .filter(|subdir| !subdir.is_empty() && subdir != "."),
            })
        }
        Value::String(spec) => match crate::sources::parse(spec) {
            crate::sources::Spec::Git(source) => Some(source),
            _ => None,
        },
        Value::Object(object) => {
            let git_ref = object
                .get("ref")
                .and_then(Value::as_str)
                .map(str::to_string);
            let url = match object.get("source").and_then(Value::as_str) {
                Some("github") => format!(
                    "https://github.com/{}",
                    object.get("repo").and_then(Value::as_str)?
                ),
                _ => object.get("url").and_then(Value::as_str)?.to_string(),
            };
            Some(GitSource {
                url,
                git_ref,
                subdir: object
                    .get("path")
                    .and_then(Value::as_str)
                    .map(|path| path.trim_start_matches("./").to_string()),
            })
        }
        _ => None,
    }
}

/// Catalog fields that describe the plugin itself; a folder without its own
/// manifest is defined entirely by them.
const ENTRY_FIELDS: &[&str] = &[
    "version",
    "skills",
    "commands",
    "hooks",
    "mcpServers",
    "lspServers",
    "agents",
];

fn listing_entry(plugin: &Value) -> Option<String> {
    let fields: serde_json::Map<String, Value> = ENTRY_FIELDS
        .iter()
        .filter_map(|key| Some((key.to_string(), plugin.get(*key)?.clone())))
        .collect();
    (!fields.is_empty()).then(|| Value::Object(fields).to_string())
}

/// The manifest fields an install from `listing` fills in.
pub(crate) fn entry_for(listing: &Listing) -> String {
    let mut entry = listing
        .entry
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Default::default()));
    entry["name"] = Value::String(listing.name.clone());
    if !listing.description.is_empty() {
        entry["description"] = Value::String(listing.description.clone());
    }
    entry.to_string()
}

/// Fills the fetched folder's manifest with catalog fields it lacks, creating
/// the manifest when the folder has none. Native packages are left alone.
pub(crate) fn apply_entry(dir: &Path, entry: &str) -> Result<(), Error> {
    if dir.join(crate::MANIFEST_FILE).is_file() {
        return Ok(());
    }
    let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(entry) else {
        return Ok(());
    };
    let path = dir.join(MANIFEST);
    let mut meta = if path.is_file() {
        read_json(&path)?
    } else {
        Value::Object(Default::default())
    };
    let Some(fields) = meta.as_object_mut() else {
        return Err(Error::Manifest(format!(
            "{} is not an object",
            path.display()
        )));
    };
    let before = fields.len();
    for (key, value) in entry {
        fields.entry(key).or_insert(value);
    }
    if fields.len() == before && path.is_file() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| crate::io_error("creating plugin manifest", parent, error))?;
    }
    std::fs::write(&path, meta.to_string())
        .map_err(|error| crate::io_error("writing plugin manifest", &path, error))
}

pub(crate) fn load_plugin(
    dir: &Path,
    id: Option<&str>,
    medha_version: &str,
) -> Result<Package, Error> {
    let meta = read_json(&dir.join(MANIFEST))?;
    let name = meta
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| Error::Manifest(format!("{} has no name", dir.join(MANIFEST).display())))?;
    let id = id.map_or_else(|| default_id(dir, name), str::to_string);
    let version = meta
        .get("version")
        .and_then(Value::as_str)
        .filter(|version| medha_extension_api::is_version(version))
        .unwrap_or("0.0.0");
    let mut components = Vec::new();
    let mut skipped = Vec::new();
    let mut permissions = RequestedPermissions::default();
    skills(dir, "skills", &mut components);
    for path in paths(meta.get("skills")) {
        if dir.join(&path).join("SKILL.md").is_file() {
            push_skill(dir, &path, &mut components);
        } else {
            skills(dir, &path, &mut components);
        }
    }
    commands(dir, &mut components);
    for field in ["lspServers", "outputStyles"] {
        if meta.get(field).is_some() {
            skipped.push(field.into());
        }
    }
    if let Some(hooks) = section(dir, meta.get("hooks"), DEFAULT_HOOKS, "hooks")? {
        let before = components.len();
        convert_section(&hooks, &mut components, &mut skipped);
        if components.len() > before {
            permissions.read_paths.push(".".into());
            permissions.write_paths.push(".".into());
        }
    }
    if let Some(servers) = section(dir, meta.get("mcpServers"), DEFAULT_MCP, "mcpServers")? {
        mcp_servers(&servers, &mut components, &mut permissions, &mut skipped);
    }
    if dir.join("agents").is_dir() {
        skipped.push("agents".into());
    }
    if components.is_empty() {
        return Err(Error::Manifest(format!(
            "{name} has no skills, commands, hooks, or MCP servers Medha can run"
        )));
    }
    let mut description = meta
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if !skipped.is_empty() {
        skipped.sort();
        skipped.dedup();
        description.push_str(&format!(" (not used by Medha: {})", skipped.join(", ")));
    }
    let manifest = Manifest {
        schema_version: medha_extension_api::MANIFEST_SCHEMA_VERSION,
        id,
        name: name.to_string(),
        version: version.to_string(),
        medha: "*".into(),
        description: Some(description.trim().to_string()).filter(|text| !text.is_empty()),
        permissions,
        components,
    };
    let tree = dir.to_path_buf();
    Package::assemble(
        dir.to_path_buf(),
        Some(&tree),
        manifest,
        medha_version,
        PackageSource::Plugin,
        &[],
    )
}

/// An installed folder is named by its id; a folder elsewhere gets `local.<name>`.
fn default_id(dir: &Path, name: &str) -> String {
    let folder = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let qualified = folder.contains('.')
        && folder
            .split('.')
            .all(|part| !part.is_empty() && part == slug(part) && part.len() <= 64);
    if qualified {
        folder
    } else {
        format!("local.{}", slug(name))
    }
}

pub(crate) fn slug(value: &str) -> String {
    let mut slug = String::new();
    for c in value.chars() {
        let c = if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        if !(c == '-' && (slug.is_empty() || slug.ends_with('-'))) {
            slug.push(c);
        }
    }
    let slug = slug.trim_end_matches('-');
    slug.chars()
        .take(64)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

fn read_json(path: &Path) -> Result<Value, Error> {
    serde_json::from_str(&read_text(path)?)
        .map_err(|error| Error::Manifest(format!("{}: {error}", path.display())))
}

/// A manifest field may be a relative file path, an inline object, or absent,
/// in which case the conventional file is used when present.
fn section(
    dir: &Path,
    field: Option<&Value>,
    default: &str,
    key: &str,
) -> Result<Option<Value>, Error> {
    let value = match field {
        Some(Value::String(path)) => read_json(&dir.join(path.trim_start_matches("./")))?,
        Some(value @ Value::Object(_)) => value.clone(),
        _ if dir.join(default).is_file() => read_json(&dir.join(default))?,
        _ => return Ok(None),
    };
    Ok(Some(value.get(key).cloned().unwrap_or(value)))
}

/// Relative paths from a manifest field that is one path or a list of them.
/// Paths that leave the plugin folder are dropped.
fn paths(field: Option<&Value>) -> Vec<String> {
    let values: Vec<&str> = match field {
        Some(Value::String(path)) => vec![path.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    values
        .into_iter()
        .map(|path| {
            path.trim_start_matches("./")
                .trim_end_matches('/')
                .to_string()
        })
        .filter(|path| {
            !path.is_empty()
                && Path::new(path)
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
        })
        .collect()
}

fn push_skill(dir: &Path, path: &str, components: &mut Vec<ExtensionComponent>) {
    let exists = components.iter().any(|component| {
        matches!(component, ExtensionComponent::Skill { path: existing, .. } if existing == path)
    });
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !exists && dir.join(path).join("SKILL.md").is_file() {
        components.push(ExtensionComponent::Skill {
            id: slug(&name),
            path: path.to_string(),
        });
    }
}

fn skills(dir: &Path, parent: &str, components: &mut Vec<ExtensionComponent>) {
    let Ok(entries) = std::fs::read_dir(dir.join(parent)) else {
        return;
    };
    let mut folders: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    folders.sort();
    for folder in folders {
        push_skill(dir, &format!("{parent}/{folder}"), components);
    }
}

/// Markdown commands become operator actions whose prompt is the body.
fn commands(dir: &Path, components: &mut Vec<ExtensionComponent>) {
    let Ok(entries) = std::fs::read_dir(dir.join("commands")) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect();
    files.sort();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let title = file
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (description, body) = split_frontmatter(&text);
        if body.trim().is_empty() {
            continue;
        }
        components.push(ExtensionComponent::Action {
            id: format!("command-{}", slug(&title)),
            description: description.unwrap_or_else(|| format!("/{title}")),
            title,
            prompt: body.trim().to_string(),
        });
    }
}

fn split_frontmatter(text: &str) -> (Option<String>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, text);
    };
    let description = rest[..end].lines().find_map(|line| {
        line.strip_prefix("description:")
            .map(|value| value.trim().trim_matches('"').to_string())
            .filter(|value| !value.is_empty())
    });
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']);
    (description, body)
}

fn mcp_servers(
    servers: &Value,
    components: &mut Vec<ExtensionComponent>,
    permissions: &mut RequestedPermissions,
    skipped: &mut Vec<String>,
) {
    let Some(servers) = servers.as_object() else {
        return;
    };
    for (name, server) in servers {
        let url = server.get("url").and_then(Value::as_str);
        let command = server.get("command").and_then(Value::as_str);
        let mut argv: Vec<String> = command.into_iter().map(str::to_string).collect();
        argv.extend(
            server
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string),
        );
        let argv: Vec<String> = argv
            .into_iter()
            .map(|part| part.replace(ROOT_VAR, medha_extension_api::PLUGIN_ROOT_PLACEHOLDER))
            .collect();
        let packaged = argv.first().is_some_and(|program| {
            program.starts_with(medha_extension_api::PLUGIN_ROOT_PLACEHOLDER)
        });
        if argv
            .first()
            .is_some_and(|program| program.contains('/') && !packaged)
        {
            skipped.push(format!("MCP server {name} (absolute command path)"));
            continue;
        }
        if server
            .get("env")
            .is_some_and(|env| env.as_object().is_some_and(|env| !env.is_empty()))
        {
            skipped.push(format!("MCP server {name} environment variables"));
        }
        let hosts: Vec<String> = match (url, argv.first().map(String::as_str)) {
            (Some(url), _) => medha_extension_api::url_host(url).into_iter().collect(),
            (None, Some("npx" | "pnpm" | "yarn" | "bunx")) => vec!["registry.npmjs.org".into()],
            (None, Some("uvx" | "pipx")) => {
                vec!["pypi.org".into(), "files.pythonhosted.org".into()]
            }
            _ => Vec::new(),
        };
        for host in hosts {
            if !permissions.network_hosts.contains(&host) {
                permissions.network_hosts.push(host);
            }
        }
        components.push(ExtensionComponent::Mcp {
            id: format!("mcp-{}", slug(name)),
            command: if url.is_some() { Vec::new() } else { argv },
            url: url.map(str::to_string),
            allow_tools: Vec::new(),
            deny_tools: Vec::new(),
        });
    }
}
