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
        .map(|component| json!({"id": component.id(), "kind": json!(component)["kind"]}))
        .collect();
    Ok(
        json!({"kind": "plugin", "name": manifest.name, "id": manifest.id, "description": manifest.description, "version": manifest.version, "access": manifest.permissions, "adds": adds, "files": plugin.files, "findings": plugin.findings, "refused": plugin.refused}),
    )
}

fn skill_store(workspace: &Path) -> Result<tools::SkillStore> {
    Ok(tools::SkillStore::new(
        workspace.join(".medha/skills"),
        Some(config::user_skills_dir()?),
    )
    .with_project_source(runtime::project_trust::skill_source()))
}

fn scope_of(params: &Value) -> Result<tools::SkillScope> {
    match params["scope"].as_str() {
        None | Some("user") => Ok(tools::SkillScope::User),
        Some("project") => Ok(tools::SkillScope::Project),
        Some(other) => Err(anyhow!("scope must be user or project, not {other}")),
    }
}

/// Installs through the scan; a skill with findings to read is added turned off.
pub(crate) async fn install_skill(workspace: &Path, source: &str, params: &Value) -> Result<Value> {
    let skills = skill_store(workspace)?;
    let report = skills
        .install_into(source, scope_of(params)?)
        .await
        .map_err(anyhow::Error::msg)?;
    if report.scan_verdict == "caution" {
        skills
            .set_enabled(&report.name, false)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(
        json!({"name": report.name, "installed": true, "enabled": report.scan_verdict == "safe", "findings": report.scan_findings}),
    )
}

/// The live check in the editor: the install's own reading of SKILL.md.
pub(crate) fn check_skill(text: &str) -> Value {
    match tools::check_skill_md(text) {
        Ok((name, description, required_tools)) => {
            json!({"name": name, "description": description, "required_tools": required_tools})
        }
        Err(error) => json!({"error": error}),
    }
}

/// Saves a skill written in the app by installing it from a private folder, so it passes the same scan.
pub(crate) async fn write_skill(workspace: &Path, params: &Value) -> Result<Value> {
    let files = params["files"]
        .as_array()
        .ok_or_else(|| anyhow!("files must be a list"))?;
    let mut written = Vec::new();
    for file in files {
        let path = file["path"].as_str().unwrap_or_default();
        let text = file["text"]
            .as_str()
            .ok_or_else(|| anyhow!("{path} has no text"))?;
        let inside = !path.is_empty()
            && Path::new(path)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)));
        if !inside {
            return Err(anyhow!("{path} must be a path inside the skill"));
        }
        written.push((path, text));
    }
    let skill_md = written
        .iter()
        .find(|(path, _)| *path == "SKILL.md")
        .ok_or_else(|| anyhow!("a skill needs a SKILL.md"))?
        .1;
    let (name, _, _) = tools::check_skill_md(skill_md).map_err(anyhow::Error::msg)?;
    let scope = scope_of(params)?;
    let target = match scope {
        tools::SkillScope::Project => workspace.join(".medha/skills"),
        _ => config::user_skills_dir()?,
    };
    if target.join(&name).exists() && params["replace"] != true {
        return Err(anyhow!("A skill named {name} already exists there"));
    }
    let folder = std::env::temp_dir().join(format!("medha-write-{}", ulid::Ulid::new()));
    let root = folder.join(&name);
    let staged = (|| {
        for (path, text) in &written {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap_or(&root))?;
            std::fs::write(file, text)?;
        }
        Ok::<_, std::io::Error>(())
    })();
    let saved = match staged {
        Ok(()) => install_skill(workspace, &root.to_string_lossy(), params).await,
        Err(error) => Err(error.into()),
    };
    let _ = std::fs::remove_dir_all(&folder);
    saved
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
