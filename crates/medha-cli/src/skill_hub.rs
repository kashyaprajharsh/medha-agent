//! Skill sources, search, updates and the team lockfile: the rules both the TUI
//! and the desktop run, so the two surfaces cannot drift apart. Callers own the
//! wording; everything here returns data.

use std::{collections::HashSet, path::Path};

use tools::{SkillStore, Tap, TapStore, hub};

use crate::config;

/// A browsable source. Built-ins ship with Medha and cannot be removed.
pub(crate) struct Source {
    pub tap: Tap,
    pub built_in: bool,
}

/// The user's registered sources; empty when the file is missing or unreadable.
pub(crate) fn registered() -> Vec<Tap> {
    config::user_taps_path()
        .ok()
        .map(TapStore::new)
        .and_then(|store| store.list().ok())
        .unwrap_or_default()
}

/// Built-ins first, then the user's own; a user source that shadows a built-in
/// is listed once, as the built-in.
pub(crate) fn sources() -> Vec<Source> {
    let defaults = hub::default_taps();
    let own: Vec<Tap> = registered()
        .into_iter()
        .filter(|tap| !defaults.iter().any(|d| d.key() == tap.key()))
        .collect();
    defaults
        .into_iter()
        .map(|tap| Source {
            tap,
            built_in: true,
        })
        .chain(own.into_iter().map(|tap| Source {
            tap,
            built_in: false,
        }))
        .collect()
}

/// Every source search should read, so search works before anything is added.
pub(crate) fn browse() -> Vec<Tap> {
    sources().into_iter().map(|source| source.tap).collect()
}

/// Returns the parsed source and whether it was new (false: updated in place).
pub(crate) fn add_source(spec: &str, path: Option<&str>) -> Result<(Tap, bool), String> {
    let tap = Tap::parse(spec, path)?;
    let store = TapStore::new(config::user_taps_path().map_err(|e| e.to_string())?);
    let added = store.add(tap.clone())?;
    Ok((tap, added))
}

/// Removes the user's sources matching `key`; built-ins are not stored, so
/// they cannot be removed this way.
pub(crate) fn remove_source(key: &str) -> Result<usize, String> {
    TapStore::new(config::user_taps_path().map_err(|e| e.to_string())?).remove(key)
}

pub(crate) async fn search(query: &str) -> Result<hub::SearchResults, String> {
    hub::search(&browse(), query).await
}

/// Active user skills: the only ones with a recorded source to update from or
/// lock. Project skills are committed with the workspace instead.
pub(crate) fn user_skills(store: &SkillStore, known_tools: &HashSet<String>) -> Vec<String> {
    store
        .discover(known_tools)
        .effective()
        .filter(|listing| listing.skill.scope == tools::SkillScope::User)
        .map(|listing| listing.skill.name.clone())
        .collect()
}

/// The message that puts a skill's procedure in front of the model on the next
/// turn, worded the same on every surface. Returns the description with it.
pub(crate) fn loaded_message(
    store: &SkillStore,
    name: &str,
    known_tools: &HashSet<String>,
) -> Result<(String, String), String> {
    let skill = store.load(name, known_tools)?;
    let field = |key: &str| skill[key].as_str().unwrap_or_default().to_string();
    let message = format!(
        "[Loaded skill: {name}] Follow this procedure for the current and related work:\n\n{}",
        field("procedure")
    );
    Ok((field("description"), message))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Update {
    UpToDate,
    /// Edited on disk since install; never overwritten.
    ModifiedLocally,
    Unmanaged(&'static str),
    Available(String),
    Updated {
        to: String,
        caution: bool,
    },
    Failed(String),
}

/// Checks each skill; with `apply`, installs every available update.
pub(crate) async fn updates(
    store: &SkillStore,
    names: Vec<String>,
    apply: bool,
) -> Vec<(String, Update)> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let outcome = match hub::check_update(store, &name).await {
            hub::UpdateStatus::UpToDate => Update::UpToDate,
            hub::UpdateStatus::ModifiedLocally => Update::ModifiedLocally,
            hub::UpdateStatus::Unmanaged(reason) => Update::Unmanaged(reason),
            hub::UpdateStatus::Available { to, .. } if !apply => Update::Available(to),
            hub::UpdateStatus::Available { to, .. } => match store.provenance(&name) {
                Some(provenance) => match store.install_from(&provenance.source).await {
                    Ok(report) => Update::Updated {
                        to,
                        caution: report.scan_verdict == "caution",
                    },
                    Err(error) => Update::Failed(format!("update failed: {error}")),
                },
                None => Update::Failed("source unavailable".into()),
            },
        };
        out.push((name, outcome));
    }
    out
}

/// Snapshots `names` into the lockfile at `path`; returns how many were locked.
pub(crate) fn lock(store: &SkillStore, names: &[String], path: &Path) -> Result<usize, String> {
    let entries = hub::lock_entries(store, names);
    let count = entries.len();
    tools::SkillLock::new(path.to_path_buf()).write(entries)?;
    Ok(count)
}

pub(crate) fn locked(path: &Path) -> Result<Vec<hub::LockEntry>, String> {
    tools::SkillLock::new(path.to_path_buf()).read()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Synced {
    /// Already at the locked bytes.
    Current,
    Installed {
        replaced: bool,
        caution: bool,
    },
    Failed(String),
}

/// Installs or repairs each locked skill at its pinned revision.
pub(crate) async fn sync(
    store: &SkillStore,
    entries: Vec<hub::LockEntry>,
) -> Vec<(String, Synced)> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let outcome = if entry.content_hash.is_some()
            && store.installed_hash(&entry.name) == entry.content_hash
        {
            Synced::Current
        } else {
            match store.install_from(&hub::locked_source(&entry)).await {
                Ok(report) => Synced::Installed {
                    replaced: report.replaced,
                    caution: report.scan_verdict == "caution",
                },
                Err(error) => Synced::Failed(error),
            }
        };
        out.push((entry.name, outcome));
    }
    out
}

#[cfg(test)]
#[path = "skill_hub_tests.rs"]
mod tests;
