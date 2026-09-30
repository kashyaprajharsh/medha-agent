//! Desktop requests for the skill hub: browse, sources, updates, the team
//! lockfile, and using a skill in a chat. The rules live in `skill_hub`, shared
//! with the TUI; this module only shapes them for the desktop.

use std::{collections::HashSet, path::Path};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::{
    config,
    skill_hub::{self, Synced, Update},
};

const LOCKFILE: &str = "medha-skills.lock";

fn store(workspace: &Path) -> Result<tools::SkillStore> {
    Ok(tools::SkillStore::new(
        workspace.join(".medha/skills"),
        Some(config::user_skills_dir()?),
    ))
}

fn text<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params[key]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("{key} is required"))
}

/// Answers the skill-hub methods; `None` for any other method.
pub(crate) async fn handle(
    method: &str,
    params: &Value,
    workspace: &Path,
) -> Option<Result<Value>> {
    let result = match method {
        "extensions.skills.search" => search(params, workspace).await,
        "extensions.skills.sources" => Ok(sources()),
        "extensions.skills.sources.add" => add_source(params),
        "extensions.skills.sources.remove" => remove_source(params),
        "extensions.skills.updates" => updates(params, workspace).await,
        "extensions.skills.lockfile" => lockfile(workspace),
        "extensions.skills.lock" => lock(workspace),
        "extensions.skills.sync" => sync(workspace).await,
        "extensions.skill.use" => use_skill(params, workspace),
        _ => return None,
    };
    Some(result)
}

async fn search(params: &Value, workspace: &Path) -> Result<Value> {
    let found = skill_hub::search(params["query"].as_str().unwrap_or_default())
        .await
        .map_err(anyhow::Error::msg)?;
    let installed: HashSet<String> = store(workspace)?
        .discover(&HashSet::new())
        .listings
        .into_iter()
        .map(|listing| listing.skill.name)
        .collect();
    let hits: Vec<Value> = found
        .hits
        .iter()
        .map(|hit| {
            json!({
                "name": hit.name,
                "description": hit.description,
                "repo": hit.repo,
                "install_url": hit.install_url,
                "installed": installed.contains(&hit.name),
            })
        })
        .collect();
    Ok(json!({ "hits": hits, "errors": found.errors, "sources": skill_hub::browse().len() }))
}

fn sources() -> Value {
    let rows: Vec<Value> = skill_hub::sources()
        .into_iter()
        .map(|source| {
            json!({
                "key": source.tap.key(),
                "repo": source.tap.repo,
                "path": source.tap.path,
                "ref": source.tap.git_ref,
                "built_in": source.built_in,
            })
        })
        .collect();
    json!({ "sources": rows })
}

fn add_source(params: &Value) -> Result<Value> {
    let path = params["path"]
        .as_str()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let (tap, added) =
        skill_hub::add_source(text(params, "spec")?, path).map_err(anyhow::Error::msg)?;
    Ok(json!({ "key": tap.key(), "added": added }))
}

fn remove_source(params: &Value) -> Result<Value> {
    let key = text(params, "key")?;
    if tools::hub::default_taps()
        .iter()
        .any(|tap| tap.key() == key)
    {
        bail!("Built-in sources come with Medha and can't be removed");
    }
    let removed = skill_hub::remove_source(key).map_err(anyhow::Error::msg)?;
    if removed == 0 {
        bail!("No source named {key}");
    }
    Ok(json!({ "removed": removed }))
}

async fn updates(params: &Value, workspace: &Path) -> Result<Value> {
    let store = store(workspace)?;
    let mut names = skill_hub::user_skills(&store, &HashSet::new());
    if let Some(name) = params["name"].as_str() {
        if !names.iter().any(|installed| installed == name) {
            bail!("No installed skill named {name}");
        }
        names = vec![name.to_string()];
    }
    let apply = params["apply"].as_bool().unwrap_or(false);
    let mut rows = Vec::new();
    for (name, outcome) in skill_hub::updates(&store, names, apply).await {
        let row = match outcome {
            Update::UpToDate => json!({ "name": name, "status": "up_to_date" }),
            Update::ModifiedLocally => json!({ "name": name, "status": "modified" }),
            Update::Unmanaged(reason) => {
                json!({ "name": name, "status": "unmanaged", "detail": reason })
            }
            Update::Available(to) => json!({ "name": name, "status": "available", "to": to }),
            Update::Updated { to, caution } => {
                review_if_flagged(&store, &name, caution)?;
                json!({ "name": name, "status": "updated", "to": to, "caution": caution })
            }
            Update::Failed(detail) => json!({ "name": name, "status": "failed", "detail": detail }),
        };
        rows.push(row);
    }
    Ok(json!({ "results": rows }))
}

/// The desktop's rule for anything it installs: a package the scan flagged
/// stays off until the person reviews it.
fn review_if_flagged(store: &tools::SkillStore, name: &str, caution: bool) -> Result<()> {
    if caution {
        store.set_enabled(name, false).map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

fn lockfile(workspace: &Path) -> Result<Value> {
    let path = workspace.join(LOCKFILE);
    let entries = if path.exists() {
        skill_hub::locked(&path).map_err(anyhow::Error::msg)?.len()
    } else {
        0
    };
    Ok(json!({ "path": path, "exists": path.exists(), "entries": entries }))
}

fn lock(workspace: &Path) -> Result<Value> {
    let store = store(workspace)?;
    let names = skill_hub::user_skills(&store, &HashSet::new());
    let path = workspace.join(LOCKFILE);
    let count = skill_hub::lock(&store, &names, &path).map_err(anyhow::Error::msg)?;
    Ok(json!({ "path": path, "count": count }))
}

async fn sync(workspace: &Path) -> Result<Value> {
    let store = store(workspace)?;
    let path = workspace.join(LOCKFILE);
    let entries = if path.exists() {
        skill_hub::locked(&path).map_err(anyhow::Error::msg)?
    } else {
        Vec::new()
    };
    if entries.is_empty() {
        bail!(
            "No shared skill set in this folder yet. Save one first, or add a teammate's {LOCKFILE}."
        );
    }
    let mut rows = Vec::new();
    for (name, outcome) in skill_hub::sync(&store, entries).await {
        let row = match outcome {
            Synced::Current => json!({ "name": name, "status": "current" }),
            Synced::Installed { replaced, caution } => {
                review_if_flagged(&store, &name, caution)?;
                json!({ "name": name, "status": if replaced { "synced" } else { "installed" }, "caution": caution })
            }
            Synced::Failed(detail) => json!({ "name": name, "status": "failed", "detail": detail }),
        };
        rows.push(row);
    }
    Ok(json!({ "results": rows }))
}

fn use_skill(params: &Value, workspace: &Path) -> Result<Value> {
    let name = text(params, "name")?;
    let (description, message) =
        skill_hub::loaded_message(&store(workspace)?, name, &HashSet::new())
            .map_err(anyhow::Error::msg)?;
    Ok(json!({ "name": name, "description": description, "message": message }))
}
