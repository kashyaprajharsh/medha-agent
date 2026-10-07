//! Frontend-independent resource commands. Store/configuration mutations use
//! the existing shared implementations; only the typed API is assembled here.

use crate::{config, desktop_preferences, desktop_skills};
use kernel::EventLog;
use protocol::{ChangeResource as Change, ReadResource as Read, ResourceResult as Reply};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path};

fn decoded<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value)
        .map_err(|_| "The resource has an invalid representation.".to_owned())
}
fn encoded<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|_| "The resource could not be encoded.".to_owned())
}

/// Resolve local install sources against the requested workspace, never the
/// daemon's working directory or MEDHA_HOME (which may be a named state).
fn local_source(workspace: &Path, source: &str) -> Result<String, String> {
    let source = source.trim();
    let path = if let Some(rest) = source.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or("Home directory unavailable")?
            .join(rest)
    } else if Path::new(source).is_absolute() {
        return Ok(source.into());
    } else if source.starts_with("./")
        || source.starts_with("../")
        || workspace.join(source).is_dir()
    {
        workspace.join(source)
    } else {
        return Ok(source.into());
    };
    Ok(path.to_string_lossy().into_owned())
}

pub(crate) fn models() -> Result<protocol::ModelCatalogue, String> {
    let cfg = config::load()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let profiles = cfg
        .model_profiles()
        .into_iter()
        .map(|saved| {
            let profile = saved.provider;
            Ok(protocol::ModelProfile {
                name: saved.name,
                model: profile.model,
                protocol: decoded(
                    serde_json::to_value(profile.protocol).expect("protocol serializes"),
                )?,
                base_url: profile.base_url.clone(),
                context_limit: profile.max_ctx,
                default: saved.is_default,
                key_present: config::key_present(&profile.base_url),
                requires_key: profile.auth.requires_credential(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(protocol::ModelCatalogue {
        profiles,
        search: decoded(json!(cfg.search_provider().as_str()))?,
        searxng_url: cfg.search.searxng_url,
    })
}

fn skill_store(workspace: &Path) -> Result<tools::SkillStore, String> {
    Ok(tools::SkillStore::new(
        workspace.join(".medha/skills"),
        Some(config::user_skills_dir().map_err(|error| error.to_string())?),
    ))
}

pub(crate) fn skills(
    store: &tools::SkillStore,
    known: &HashSet<String>,
) -> protocol::SkillCatalogue {
    let discovery = store.discover(known);
    protocol::SkillCatalogue {
        skills: discovery
            .listings
            .iter()
            .filter(|listing| !listing.shadowed)
            .map(|listing| protocol::SkillSummary {
                name: listing.skill.name.clone(),
                description: listing.skill.description.clone(),
                scope: listing.skill.scope.as_str().into(),
                available: listing.available(),
                enabled: !listing.disabled,
                missing_tools: listing.missing_tools.clone(),
                verdict: store
                    .provenance(&listing.skill.name)
                    .and_then(|source| source.scan_verdict),
            })
            .collect(),
        errors: discovery
            .errors
            .into_iter()
            .map(|(path, error)| format!("{}: {error}", path.display()))
            .collect(),
    }
}

fn plugin_store(workspace: &Path) -> Result<extensions::Store, String> {
    Ok(crate::plugins_cmd::store(
        &config::medha_home().map_err(|e| e.to_string())?,
        workspace,
        &config::state_dir(workspace).map_err(|e| e.to_string())?,
    ))
}

fn access_description(plugin: &extensions::ListedPlugin, grant: &extensions::Grant) -> String {
    let mut lines = Vec::new();
    if grant.network {
        lines.push(format!("network: allowed for this plugin's processes (asked for {}; hosts cannot be limited individually)", plugin.package.manifest.permissions.network_hosts.join(", ")));
    }
    if !grant.read_paths.is_empty() {
        lines.push(format!(
            "read in workspace: {}",
            grant.read_paths.join(", ")
        ));
    }
    if !grant.write_paths.is_empty() {
        lines.push(format!(
            "write in workspace: {}",
            grant.write_paths.join(", ")
        ));
    }
    lines.join("\n")
}

pub(crate) fn plugins(store: &extensions::Store) -> Result<protocol::PluginCatalogue, String> {
    let discovery = store.discover().map_err(|e| e.to_string())?;
    let notices = discovery.notices();
    let mut rows = Vec::new();
    for plugin in &discovery.plugins {
        let manifest = &plugin.package.manifest;
        let grant = plugin.requested_grant();
        let components = manifest
            .components
            .iter()
            .map(|component| protocol::PluginComponent {
                id: component.id().to_string(),
                kind: match component {
                    extensions::ExtensionComponent::Skill { .. } => "skill",
                    extensions::ExtensionComponent::Action { .. } => "action",
                    extensions::ExtensionComponent::Mcp { .. } => "MCP server",
                    extensions::ExtensionComponent::Hook { .. } => "hook",
                }
                .into(),
            })
            .collect();
        let actions = manifest
            .components
            .iter()
            .filter_map(|component| match component {
                extensions::ExtensionComponent::Action {
                    id, title, prompt, ..
                } => Some(protocol::PluginAction {
                    id: id.clone(),
                    title: title.clone(),
                    prompt: prompt.clone(),
                }),
                _ => None,
            })
            .collect();
        let health = store.component_health(&manifest.id, &plugin.package.content_hash);
        let failed = health
            .iter()
            .find_map(|component| component.last_failure.clone());
        let worst = health
            .iter()
            .map(|component| match component.state {
                extensions::ComponentState::Ready => 0,
                extensions::ComponentState::Starting => 1,
                extensions::ComponentState::Restarting => 2,
                extensions::ComponentState::Failed => 3,
            })
            .max();
        let origin = (plugin.scope == extensions::Scope::User)
            .then(|| store.origin(&manifest.id))
            .flatten();
        let version = if manifest.version == "0.0.0" {
            origin
                .as_ref()
                .map(|source| format!("@{}", crate::plugins_cmd::short(&source.commit)))
                .unwrap_or_else(|| manifest.version.clone())
        } else {
            manifest.version.clone()
        };
        let access_description = grant
            .as_ref()
            .ok()
            .map(|grant| access_description(plugin, grant))
            .unwrap_or_default();
        rows.push(protocol::PluginSummary {
            id: manifest.id.clone(),
            version,
            scope: decoded(json!(plugin.scope.as_str()))?,
            activation: match plugin.activation {
                extensions::Activation::Enabled => protocol::PluginActivation::Enabled,
                extensions::Activation::Disabled => protocol::PluginActivation::Disabled,
                extensions::Activation::Changed => protocol::PluginActivation::Changed,
                extensions::Activation::Collision => protocol::PluginActivation::Collision,
                extensions::Activation::Shadowed => protocol::PluginActivation::Shadowed,
            },
            hash: plugin.package.content_hash.clone(),
            root: plugin.package.root.clone(),
            components,
            actions,
            updatable: origin.is_some(),
            can_rollback: store.can_rollback(&manifest.id),
            health: worst.map(|rank| ["ready", "starting", "restarting", "failed"][rank].into()),
            last_failure: failed,
            access_description,
            hooks: manifest
                .components
                .iter()
                .filter_map(|component| match component {
                    extensions::ExtensionComponent::Hook {
                        id,
                        points,
                        matcher,
                        ..
                    } => Some(protocol::HookDescription {
                        id: id.clone(),
                        points: points.iter().map(|point| point.as_str().into()).collect(),
                        matcher: matcher.clone(),
                    }),
                    _ => None,
                })
                .collect(),
            grant: grant
                .as_ref()
                .ok()
                .map(|grant| decoded(json!(grant)))
                .transpose()?,
            blocked: grant.err().map(|error| error.to_string()),
        });
    }
    let pending = store.needs_review().map_err(|e| e.to_string())?;
    let hook_review = pending
        .first()
        .and_then(|plugin| {
            rows.iter().find(|row| {
                row.id == plugin.package.manifest.id && row.hash == plugin.package.content_hash
            })
        })
        .cloned();
    let builtin: Vec<_> = crate::application_catalog::COMMANDS
        .iter()
        .map(|(name, _)| *name)
        .collect();
    let commands = crate::application_commands::load(store, &builtin)
        .into_iter()
        .map(|command| protocol::PluginCommand {
            name: command.name,
            description: command.description,
            action_id: command.action_id,
            has_snippets: command.prompt.contains("!`"),
        })
        .collect();
    Ok(protocol::PluginCatalogue {
        plugins: rows,
        notices,
        commands,
        hook_review,
    })
}

async fn preferences(workspace: &Path, method: &str, params: Value) -> Result<Value, String> {
    desktop_preferences::handle(method, &params, workspace)
        .await
        .map_err(|error| error.to_string())
}

async fn skill_hub(workspace: &Path, method: &str, params: Value) -> Result<Value, String> {
    desktop_skills::handle(method, &params, workspace)
        .await
        .ok_or("Unknown skill operation")?
        .map_err(|error| error.to_string())
}

pub(crate) async fn read<L: EventLog>(
    log: &L,
    workspace: &Path,
    request: Read,
    live_skills: Option<&tools::SkillStore>,
    live_plugins: Option<&extensions::Store>,
    known: &HashSet<String>,
) -> Result<Reply, String> {
    Ok(match request {
        Read::Models => Reply::Models(models()?),
        Read::Pulse => {
            let cfg = config::load().map_err(|e| e.to_string())?;
            Reply::Pulse(protocol::ResourceNotice {
                messages: vec![config::pulse(cfg.as_ref(), None, None, Some(workspace)).render()],
            })
        }
        Read::Sessions => Reply::Sessions(
            log.sessions()
                .await
                .into_iter()
                .map(|session| protocol::SessionSummary {
                    id: session.id.to_string(),
                    started: session.started_ts,
                    updated: session.last_ts,
                    title: Some(session.title),
                    model: None,
                    events: session.events,
                })
                .collect(),
        ),
        Read::Skills => {
            let fallback;
            let store = match live_skills {
                Some(store) => store,
                None => {
                    fallback = skill_store(workspace)?;
                    &fallback
                }
            };
            Reply::Skills(skills(store, known))
        }
        Read::Skill { name } => {
            let fallback;
            let store = match live_skills {
                Some(store) => store,
                None => {
                    fallback = skill_store(workspace)?;
                    &fallback
                }
            };
            Reply::Skill(decoded(store.inspect(&name, known)?)?)
        }
        Read::SkillSources => Reply::SkillSources(decoded(
            skill_hub(workspace, "extensions.skills.sources", json!({})).await?["sources"].take(),
        )?),
        Read::SkillSearch { query } => Reply::SkillSearch(decoded(
            skill_hub(
                workspace,
                "extensions.skills.search",
                json!({"query": query}),
            )
            .await?,
        )?),
        Read::SkillUpdates { name } => Reply::SkillUpdates(decoded(
            skill_hub(
                workspace,
                "extensions.skills.updates",
                json!({"name": name}),
            )
            .await?["results"]
                .take(),
        )?),
        Read::SkillLockfile => {
            let value = skill_hub(workspace, "extensions.skills.lockfile", json!({})).await?;
            Reply::SkillLockfile {
                path: decoded(value["path"].clone())?,
                exists: value["exists"] == true,
                entries: value["entries"]
                    .as_u64()
                    .ok_or("Invalid skill lockfile count")? as usize,
            }
        }
        Read::Plugins => {
            let fallback;
            let store = match live_plugins {
                Some(store) => store,
                None => {
                    fallback = plugin_store(workspace)?;
                    &fallback
                }
            };
            Reply::Plugins(plugins(store)?)
        }
        Read::PluginDoctor => {
            let fallback;
            let store = match live_plugins {
                Some(store) => store,
                None => {
                    fallback = plugin_store(workspace)?;
                    &fallback
                }
            };
            Reply::Changed(protocol::ResourceNotice {
                messages: extensions::doctor::run(store)
                    .into_iter()
                    .map(|check| {
                        format!(
                            "{} {}: {}",
                            match check.level {
                                extensions::doctor::Level::Ok => "✔",
                                extensions::doctor::Level::Warn => "⚠",
                                extensions::doctor::Level::Fail => "✕",
                            },
                            check.subject,
                            check.message
                        )
                    })
                    .collect(),
            })
        }
        Read::Marketplaces => Reply::Marketplaces(decoded(
            preferences(workspace, "extensions.marketplace.list", json!({})).await?,
        )?),
        Read::Hooks => {
            let all = plugins(&plugin_store(workspace)?)?;
            Reply::Hooks(protocol::HookCatalogue {
                hooks: decoded(json!(crate::hook_files::list(workspace)))?,
                project: all
                    .plugins
                    .into_iter()
                    .find(|row| row.id == extensions::PROJECT_HOOKS_ID),
            })
        }
    })
}

pub(crate) async fn change(workspace: &Path, action: Change) -> Result<Reply, String> {
    let (method, params) = match action {
        Change::SaveModel(draft) => {
            let protocol: kernel::Protocol = decoded(json!(draft.protocol))?;
            if protocol == kernel::Protocol::GeminiInteractions
                && draft.key.as_ref().is_none_or(|key| key.0.trim().is_empty())
            {
                return Err("Gemini Interactions requires an API key.".into());
            }
            let mut profile = providers::ProviderProfile::openai_chat(
                &draft.base_url,
                &draft.model,
                if draft.key.as_ref().is_none_or(|key| key.0.is_empty()) {
                    providers::AuthKind::None
                } else {
                    providers::AuthKind::for_protocol(protocol)
                },
            );
            profile.protocol = protocol;
            profile.max_ctx = draft.context_limit;
            let value = preferences(
                workspace,
                "settings.model.save",
                json!({"name": draft.name, "profile": profile, "key": draft.key}),
            )
            .await?;
            return Ok(Reply::ModelSaved {
                name: value["name"]
                    .as_str()
                    .ok_or("Missing saved model name")?
                    .into(),
                catalogue: models()?,
            });
        }
        Change::SetDefaultModel { name } => ("settings.model.default", json!({"name": name})),
        Change::RemoveModel { name } => ("settings.model.remove", json!({"name": name})),
        Change::UpdateModelKey { name, key } => {
            config::edit(|cfg| {
                let profile = cfg
                    .models
                    .get_mut(&name)
                    .ok_or_else(|| anyhow::anyhow!("No saved model named {name}"))?;
                config::store_key(&profile.base_url, &key.0)?;
                if !profile.auth.requires_credential() {
                    profile.auth = providers::AuthKind::for_protocol(profile.protocol);
                }
                Ok(())
            })
            .map_err(|error| error.to_string())?;
            return Ok(Reply::ModelSaved {
                name,
                catalogue: models()?,
            });
        }
        Change::Search { provider, key, url } => (
            "settings.search.save",
            json!({"provider": provider, "key": key, "url": url}),
        ),
        Change::PulseFix => ("settings.health.fix", json!({})),
        Change::InstallSkill { source } => (
            "extensions.skill.install",
            json!({"source": local_source(workspace, &source)?}),
        ),
        Change::RemoveSkill { name } => ("extensions.skill.remove", json!({"name": name})),
        Change::ConfigureSkill { name, enabled } => (
            "extensions.skill.configure",
            json!({"name": name, "enabled": enabled}),
        ),
        Change::AddSkillSource { spec, path } => (
            "extensions.skills.sources.add",
            json!({"spec": spec, "path": path}),
        ),
        Change::RemoveSkillSource { key } => {
            ("extensions.skills.sources.remove", json!({"key": key}))
        }
        Change::UpdateSkills { name } => (
            "extensions.skills.updates",
            json!({"name": name, "apply": true}),
        ),
        Change::LockSkills => ("extensions.skills.lock", json!({})),
        Change::SyncSkills => ("extensions.skills.sync", json!({})),
        Change::InstallPlugin { source } => {
            let result = preferences(
                workspace,
                "extensions.install",
                json!({"source": local_source(workspace, &source)?}),
            )
            .await?;
            return Ok(Reply::PluginInstalled {
                id: result["id"]
                    .as_str()
                    .ok_or("Missing installed plugin id")?
                    .into(),
            });
        }
        Change::UpdatePlugin { id } => {
            let store = plugin_store(workspace)?;
            let plan = store.plan_update(&id).map_err(|e| e.to_string())?;
            if plan.up_to_date() {
                return Ok(Reply::Changed(protocol::ResourceNotice {
                    messages: vec![format!("{id} is up to date")],
                }));
            }
            let review = plan.was_enabled && plan.access_changed();
            let description = format!(
                "updated {id} {} → {}{}",
                plan.from_version,
                plan.to_version,
                if review {
                    "; review the new access before enabling it"
                } else {
                    ""
                }
            );
            store.apply_update(plan, false).map_err(|e| e.to_string())?;
            return Ok(Reply::Changed(protocol::ResourceNotice {
                messages: vec![description],
            }));
        }
        Change::RollbackPlugin { id } => ("extensions.rollback", json!({"id": id})),
        Change::EnablePlugin {
            id,
            scope,
            hash,
            grant,
        } => (
            "extensions.enable",
            json!({"id": id, "scope": scope, "hash": hash, "grant": grant}),
        ),
        Change::DisablePlugin { id, scope } => {
            ("extensions.disable", json!({"id": id, "scope": scope}))
        }
        Change::RemovePlugin { id } => ("extensions.remove", json!({"id": id})),
        Change::AddMarketplace { source } => {
            ("extensions.marketplace.add", json!({"source": source}))
        }
        Change::RemoveMarketplace { name } => {
            ("extensions.marketplace.remove", json!({"name": name}))
        }
        Change::RefreshMarketplaces => ("extensions.marketplace.refresh", json!({})),
        Change::AddHook {
            event,
            matcher,
            command,
        } => (
            "extensions.hooks.add",
            json!({"event": event, "matcher": matcher, "command": command}),
        ),
        Change::RemoveHook { event, file } => (
            "extensions.hooks.remove",
            json!({"event": event, "file": file}),
        ),
    };
    let value = preferences(workspace, method, params).await?;
    if method == "extensions.skill.install" {
        return Ok(Reply::SkillInstalled {
            name: decoded(value["name"].clone())?,
            enabled: decoded(value["enabled"].clone())?,
            findings: decoded(value["findings"].clone())?,
        });
    }
    if let Some(rows) = value.get("results") {
        return Ok(Reply::SkillUpdates(decoded(rows.clone())?));
    }
    let messages = if let Some(applied) = value.get("applied").and_then(Value::as_array) {
        applied
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    } else if let Some(errors) = value.get("problems").and_then(Value::as_array) {
        errors
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    } else if let Some(path) = value.get("path").and_then(Value::as_str) {
        vec![format!("saved {path}")]
    } else {
        vec!["saved".into()]
    };
    Ok(Reply::Changed(protocol::ResourceNotice { messages }))
}

pub(crate) async fn folder<L: EventLog>(
    log: &L,
    workspace: &Path,
    method: &str,
    params: Value,
) -> Option<Result<Value, String>> {
    let result = match method {
        "application.resources" => match decoded(params) {
            Ok(request) => read(log, workspace, request, None, None, &HashSet::new())
                .await
                .and_then(encoded),
            Err(error) => Err(error),
        },
        "application.resources.change" => match decoded(params) {
            Ok(request) => change(workspace, request).await.and_then(encoded),
            Err(error) => Err(error),
        },
        "application.models.discover" => match decoded::<protocol::DiscoverModels>(params) {
            Ok(request) => {
                let protocol = decoded(json!(request.protocol));
                match protocol {
                    Ok(protocol) => {
                        let mut profile = providers::ProviderProfile::openai_chat(
                            &request.base_url,
                            "discovery",
                            if request.key.0.is_empty() {
                                providers::AuthKind::None
                            } else {
                                providers::AuthKind::for_protocol(protocol)
                            },
                        );
                        profile.protocol = protocol;
                        providers::openai_compat::list_models_for_profile(&profile, &request.key.0)
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|models| {
                                encoded(
                                    models
                                        .into_iter()
                                        .map(|model| protocol::DiscoveredModel {
                                            id: model.id,
                                            context_length: model.context_length,
                                        })
                                        .collect::<Vec<_>>(),
                                )
                            })
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        },
        _ => return None,
    };
    Some(result)
}
