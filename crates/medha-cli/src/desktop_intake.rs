//! `extensions.preview`: what a dropped or pasted source is, and what adding it would bring, with nothing added.
use crate::config;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::path::Path;

/// `~/x` as the person's home, since the desktop passes paths as typed.
pub(crate) fn expand_home(source: &str) -> Result<String> {
    let Some(rest) = source.strip_prefix("~/") else {
        return Ok(source.into());
    };
    Ok(config::medha_home()?
        .parent()
        .ok_or_else(|| anyhow!("Home is unavailable"))?
        .join(rest)
        .to_string_lossy()
        .into_owned())
}

pub(crate) async fn preview(
    source: &str,
    workspace: &Path,
    plugins: &extensions::Store,
) -> Result<Value> {
    let source = expand_home(source.trim())?;
    if let Some(name) = connector_name(&source) {
        return Ok(json!({"kind": "connector", "name": name, "url": source}));
    }
    let skills = tools::SkillStore::new(
        workspace.join(".medha/skills"),
        Some(config::user_skills_dir()?),
    );
    let listed = matches!(
        extensions::sources::parse(&source),
        extensions::sources::Spec::Listed { .. }
    );
    let skill = if listed {
        Err("it is listed in a plugin marketplace".to_string())
    } else {
        skills.preview(&source).await
    };
    let not_skill = match skill {
        Ok(report) => {
            return Ok(
                json!({"kind": "skill", "name": report.name, "description": report.description, "files": report.listed, "findings": report.scan_findings}),
            );
        }
        Err(why) if why.starts_with(tools::REFUSED) => {
            return Ok(json!({"kind": "skill", "refused": why}));
        }
        Err(why) => why,
    };
    let markets = crate::plugins_cmd::marketplaces(&config::medha_home()?);
    let plugin = plugins
        .preview_from(&source, &markets)
        .map_err(|error| anyhow!("Not a skill ({not_skill}) or a plugin ({error})"))?;
    let manifest = &plugin.manifest;
    let adds: Vec<Value> = manifest
        .components
        .iter()
        .map(|component| json!({"id": component.id()}))
        .collect();
    Ok(
        json!({"kind": "plugin", "name": manifest.name, "id": manifest.id, "description": manifest.description, "version": manifest.version, "access": manifest.permissions, "adds": adds, "files": plugin.files, "findings": plugin.findings, "refused": plugin.refused}),
    )
}

/// A remote MCP server: an https address that is not a code host. Named after its site.
fn connector_name(source: &str) -> Option<String> {
    let rest = source.strip_prefix("https://")?;
    let host = rest.split(['/', ':', '?']).next()?.to_ascii_lowercase();
    if ["github.com", "www.github.com", "gitlab.com"].contains(&host.as_str()) {
        return None;
    }
    let host = ["mcp.", "api.", "www."]
        .iter()
        .find_map(|prefix| host.strip_prefix(prefix))
        .unwrap_or(&host);
    host.split('.')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}
