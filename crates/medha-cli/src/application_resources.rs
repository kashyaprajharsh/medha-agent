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
                provider_id: profile.provider_id,
                auth: Some(decoded(json!(profile.auth))?),
                connection: protocol::ModelConnection {
                    headers: profile.headers,
                    ca_cert_file: profile.ca_cert_file,
                    proxy: profile.proxy,
                },
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
                extra_keys: runtime::key_pool::slots(&cfg, &profile.base_url)
                    .into_iter()
                    .map(|(slot, _)| slot)
                    .collect(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let catalogue = provider_catalogue(&providers::catalogue::snapshot())?;
    Ok(protocol::ModelCatalogue {
        profiles,
        fallback: cfg.fallback.clone(),
        providers: catalogue.providers,
        revision: catalogue.revision,
        search: decoded(json!(cfg.search_provider().as_str()))?,
        searxng_url: cfg.search.searxng_url,
    })
}

/// The address of the server a saved model talks to: what its keys are kept under.
fn server_of(model: &str) -> Result<String, String> {
    let cfg = config::load()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let profile = cfg.model_profile(model);
    profile
        .map(|profile| profile.base_url.clone())
        .ok_or_else(|| format!("No saved model named {model}"))
}

fn provider_catalogue(
    snapshot: &providers::catalogue::Snapshot,
) -> Result<protocol::ProviderCatalogue, String> {
    Ok(protocol::ProviderCatalogue {
        revision: snapshot.revision.clone(),
        providers: decoded(
            serde_json::to_value(&snapshot.providers)
                .map_err(|_| "Could not encode provider catalogue.")?,
        )?,
    })
}

fn draft_profile(draft: &protocol::ModelDraft) -> Result<providers::ProviderProfile, String> {
    let protocol: kernel::Protocol = decoded(json!(draft.protocol))?;
    let auth = match draft.auth {
        Some(auth) => decoded(json!(auth))?,
        None if protocol == kernel::Protocol::GeminiInteractions
            || draft
                .key
                .as_ref()
                .is_some_and(|key| !key.0.trim().is_empty()) =>
        {
            providers::AuthKind::for_protocol(protocol)
        }
        None => providers::AuthKind::None,
    };
    let mut profile = providers::ProviderProfile::openai_chat(&draft.base_url, &draft.model, auth);
    profile.provider_id = draft.provider_id.clone();
    profile.protocol = protocol;
    profile.headers = draft.connection.headers.clone();
    profile.ca_cert_file = draft.connection.ca_cert_file.clone();
    profile.proxy = draft.connection.proxy.clone();
    profile.max_ctx = draft.context_limit;
    profile.validate()?;
    if !providers::implemented(protocol) {
        return Err("This model protocol is not implemented yet.".into());
    }
    Ok(profile)
}

async fn test_model(draft: protocol::ModelDraft) -> Result<Value, String> {
    let profile = draft_profile(&draft)?;
    let credential =
        config::deployment_credential(&profile, draft.key.as_ref().map(|key| key.0.as_str()))
            .map_err(|error| error.to_string())?;
    encoded(providers::model_test::test(&profile, &credential).await)
}

fn skill_store(workspace: &Path) -> Result<tools::SkillStore, String> {
    Ok(tools::SkillStore::new(
        workspace.join(".medha/skills"),
        Some(config::user_skills_dir().map_err(|error| error.to_string())?),
    )
    .with_project_source(runtime::project_trust::skill_source()))
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
        Read::ProjectTrust => {
            let review = runtime::project_trust::ProjectTrust::for_folder(workspace)
                .map_err(|error| error.to_string())?
                .map(|trust| trust.review(workspace))
                .transpose()
                .map_err(|error| error.to_string())?;
            Reply::ProjectTrust(
                review
                    .map(|review| {
                        decoded(serde_json::to_value(review).expect("trust review serializes"))
                    })
                    .transpose()?,
            )
        }
        Read::Models => Reply::Models(models()?),
        Read::Pulse => {
            let cfg = config::load().map_err(|e| e.to_string())?;
            Reply::Pulse(protocol::ResourceNotice {
                messages: vec![config::pulse(cfg.as_ref(), None, None, Some(workspace)).render()],
            })
        }
        // Sub-agents are counted on the chat that started them, not resumed on their own.
        Read::Sessions => {
            let sessions = log.sessions().await;
            let mut agents = std::collections::HashMap::<ulid::Ulid, u32>::new();
            for parent in sessions.iter().filter_map(|session| session.parent) {
                *agents.entry(parent).or_default() += 1;
            }
            Reply::Sessions(
                sessions
                    .into_iter()
                    .filter(|session| session.parent.is_none())
                    .map(|session| protocol::SessionSummary {
                        id: session.id.to_string(),
                        started: session.started_ts,
                        updated: session.last_ts,
                        title: Some(session.title),
                        model: None,
                        events: session.events,
                        agents: agents.get(&session.id).copied().unwrap_or_default(),
                    })
                    .collect(),
            )
        }
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
        Change::TrustProject {
            root,
            folder,
            fingerprint,
        } => {
            let trust = runtime::project_trust::ProjectTrust::for_folder(workspace)
                .map_err(|error| error.to_string())?
                .ok_or("The home folder is not a project.")?;
            trust
                .accept_for(workspace, &root, &folder, fingerprint)
                .map_err(|error| error.to_string())?;
            return Ok(Reply::Changed(protocol::ResourceNotice {
                messages: vec![
                    "Project trusted. Its approved settings apply from the next message.".into(),
                ],
            }));
        }
        Change::RevokeProjectTrust => {
            runtime::project_trust::revoke_live(workspace)
                .await
                .map_err(|error| error.to_string())?;
            return Ok(Reply::Changed(protocol::ResourceNotice { messages: vec!["Project trust revoked. Local language servers stopped; other processes stop theirs at next use. Open chats follow the change from their next message.".into()] }));
        }
        Change::SaveModel(draft) => {
            let profile = draft_profile(&draft)?;
            config::deployment_credential(&profile, draft.key.as_ref().map(|key| key.0.as_str()))
                .map_err(|error| error.to_string())?;
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
        Change::SetFallback { names } => ("settings.fallback.save", json!({"names": names})),
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
        // Extra keys go through the same settings methods the desktop's Keys page uses.
        Change::AddModelKey { name, key } => (
            "settings.keys.set",
            json!({"group": "model", "id": server_of(&name)?, "key": key, "add": true}),
        ),
        Change::RemoveModelKey { name, slot } => (
            "settings.keys.remove",
            json!({"group": "model", "id": server_of(&name)?, "slot": slot}),
        ),
        Change::SetModelCertificate { name, file } => (
            "settings.model.certificate",
            json!({"name": name, "file": file}),
        ),
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
    artifacts: &std::sync::Arc<dyn kernel::ArtifactStore>,
    workspace: &Path,
    method: &str,
    params: Value,
) -> Option<Result<Value, String>> {
    let result = match method {
        "application.providers" => match decoded::<protocol::ReadProviders>(params) {
            Ok(_) => provider_catalogue(&providers::catalogue::snapshot()).and_then(encoded),
            Err(error) => Err(error),
        },
        "application.providers.refresh" => match decoded::<protocol::RefreshProviders>(params) {
            Ok(_) => providers::catalogue::refresh()
                .await
                .and_then(|snapshot| provider_catalogue(&snapshot))
                .and_then(encoded),
            Err(error) => Err(error),
        },
        "application.providers.resolve" => match decoded::<protocol::ResolveDeployment>(params) {
            Ok(request) => {
                let snapshot = providers::catalogue::snapshot();
                (|| {
                    let provider = snapshot
                        .providers
                        .iter()
                        .find(|provider| provider.id == request.provider_id)
                        .ok_or("Unknown provider.")?;
                    if !provider.available {
                        return Err(provider
                            .unavailable_reason
                            .clone()
                            .unwrap_or_else(|| "Provider unavailable.".into()));
                    }
                    let base_url = provider.resolve_endpoint(&request.parameters)?;
                    encoded(protocol::ModelDeployment {
                        provider_id: provider.id.clone(),
                        protocol: decoded(json!(
                            provider.protocol.ok_or("Provider protocol unavailable.")?
                        ))?,
                        auth: decoded(json!(provider.auth))?,
                        base_url,
                    })
                })()
            }
            Err(error) => Err(error),
        },
        "application.models.test" => match decoded::<protocol::TestModel>(params) {
            Ok(request) => test_model(request.draft).await,
            Err(error) => Err(error),
        },
        "application.image" => match decoded::<protocol::ReadWorkspaceImage>(params) {
            Ok(request) => {
                crate::chat_history::read_image(artifacts, request.hash, request.thumbnail)
                    .await
                    .and_then(encoded)
            }
            Err(error) => Err(error),
        },
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
                let profile = draft_profile(&protocol::ModelDraft {
                    provider_id: request.provider_id,
                    auth: request.auth,
                    connection: request.connection,
                    name: None,
                    protocol: request.protocol,
                    base_url: request.base_url,
                    model: "discovery".into(),
                    context_limit: None,
                    key: Some(request.key.clone()),
                });
                match profile {
                    Ok(profile) => {
                        let credential =
                            match config::deployment_credential(&profile, Some(&request.key.0)) {
                                Ok(key) => key,
                                Err(error) => return Some(Err(error.to_string())),
                            };
                        providers::discovery::list_models_for_profile(&profile, &credential)
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
