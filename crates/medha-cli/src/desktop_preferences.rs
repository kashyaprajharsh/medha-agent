//! Desktop forms over the same configuration, credentials and extension store
//! used by the TUI. Replies never include credential values.
use crate::{config, plugins_cmd};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::path::Path;

fn text<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("{key} is required"))
}
fn plugin_store(workspace: &Path) -> Result<extensions::Store> {
    Ok(plugins_cmd::store(
        &config::medha_home()?,
        workspace,
        &config::state_dir(workspace)?,
    ))
}
pub(crate) fn plugins(store: &extensions::Store) -> Result<Value> {
    let discovery = store.discover()?;
    let rows: Vec<Value> = discovery.plugins.iter().map(|plugin| {
        let manifest = &plugin.package.manifest;
        let grant = plugin.requested_grant();
        let components: Vec<Value> = manifest.components.iter().map(|component| json!({ "id": component.id(), "kind": match component { extensions::ExtensionComponent::Skill { .. } => "Skill", extensions::ExtensionComponent::Action { .. } => "Action", extensions::ExtensionComponent::Mcp { .. } => "MCP server", extensions::ExtensionComponent::Hook { .. } => "Hook" } })).collect();
        json!({ "id": manifest.id, "name": manifest.name, "description": manifest.description, "version": manifest.version, "scope": plugin.scope.as_str(), "activation": plugin.activation.as_str(), "hash": plugin.package.content_hash, "permissions": manifest.permissions, "can_rollback": store.can_rollback(&manifest.id), "grant": grant.as_ref().ok(), "blocked": grant.err().map(|e| e.to_string()), "components": components,
            "health": store.component_health(&manifest.id, &plugin.package.content_hash).iter().map(|health| json!({"id": health.component_id, "state": health.state.as_str(), "error": health.last_failure})).collect::<Vec<_>>() })
    }).collect();
    Ok(json!({ "plugins": rows, "notices": discovery.notices() }))
}
fn redact_command(command: &[String]) -> Vec<String> {
    let mut hidden = false;
    command
        .iter()
        .map(|argument| {
            if hidden {
                hidden = false;
                return "••••".into();
            }
            let lower = argument.to_lowercase();
            if lower.contains("token")
                || lower.contains("password")
                || lower.contains("api-key")
                || lower.contains("apikey")
                || lower == "--key"
                || lower == "--bearer"
            {
                if let Some((flag, _)) = argument.split_once('=') {
                    return format!("{flag}=••••");
                }
                hidden = true;
            }
            argument.clone()
        })
        .collect()
}
/// The `/pulse` report: each check, and where the model, endpoint and key come
/// from. Credential values and `MEDHA_*` values are never included.
fn health_view(pulse: &config::Pulse) -> Value {
    let health = |health: config::Health| match health {
        config::Health::Ok => "ok",
        config::Health::Warn => "warn",
        config::Health::Error => "error",
    };
    let active = match &pulse.resolved {
        Ok(Some(resolved)) => json!({
            "profile": resolved.name,
            "model": resolved.provider.model,
            "base_url": resolved.provider.base_url,
            "protocol": resolved.provider.protocol,
            "model_from": resolved.model_source.label(),
            "base_url_from": resolved.base_url_source.label(),
            "key_from": resolved.credential_source.label(),
            "key_present": !resolved.credential.is_empty(),
        }),
        Ok(None) => Value::Null,
        Err(error) => json!({"error": error}),
    };
    json!({
        "checks": pulse.checks.iter().map(|check| json!({
            "health": health(check.health),
            "title": check.title,
            "detail": check.detail,
            "fixable": check.auto_fixable,
        })).collect::<Vec<_>>(),
        "fixable": pulse.has_fixes(),
        "active": active,
        "medha_home": pulse.medha_home,
        "config_path": pulse.config_path,
        "config_exists": pulse.config_exists,
        "medha_env": pulse.medha_env,
        "ignored_env": pulse.ignored_env,
        "project_lock": pulse.project_lock,
        "lock_executor": pulse.lock_executor,
    })
}

/// Saved catalogs, read without the network, with each listing marked when it
/// is already installed. `unfetched` counts shipped catalogs not yet fetched.
fn marketplace_view(
    markets: &extensions::sources::Marketplaces,
    store: &extensions::Store,
) -> Result<Value> {
    let installed: std::collections::HashSet<String> = store
        .discover()?
        .plugins
        .into_iter()
        .map(|plugin| plugin.package.manifest.id)
        .collect();
    let rows: Vec<Value> = markets
        .list()?
        .into_iter()
        .map(|market| {
            let plugins: Vec<Value> = market
                .plugins
                .iter()
                .map(|listing| {
                    let id = extensions::sources::listed_id(&market.name, &listing.name);
                    json!({
                        "name": listing.name,
                        "description": listing.description,
                        "category": listing.category,
                        "source": format!("{}@{}", listing.name, market.name),
                        "installed": installed.contains(&id),
                    })
                })
                .collect();
            json!({
                "name": market.name,
                "url": market.source.url,
                "commit": plugins_cmd::short(&market.commit),
                "plugins": plugins,
            })
        })
        .collect();
    Ok(json!({"marketplaces": rows, "unfetched": markets.pending_defaults().len()}))
}

/// Changes only what cannot move a server's stored key: its launch command,
/// URL and environment are part of the key's identity and need a re-add.
fn apply_mcp_update(server: &mut config::McpServer, params: &Value) -> Result<()> {
    if let Some(trust) = params.get("trust").and_then(Value::as_str) {
        if !["trusted", "workspace"].contains(&trust) {
            bail!("trust must be trusted or workspace");
        }
        server.trust = trust.into();
    }
    if let Some(network) = params.get("network") {
        server.network = match network {
            Value::Null => None,
            Value::Bool(allowed) => Some(*allowed),
            _ => bail!("network must be true, false or null"),
        };
    }
    for (key, flag) in [
        ("parallel", &mut server.parallel_calls),
        ("disabled", &mut server.disabled),
    ] {
        if let Some(value) = params.get(key) {
            *flag = value
                .as_bool()
                .ok_or_else(|| anyhow!("{key} must be true or false"))?;
        }
    }
    for (key, list) in [
        ("allow_tools", &mut server.allow_tools),
        ("deny_tools", &mut server.deny_tools),
    ] {
        if let Some(value) = params.get(key) {
            *list = serde_json::from_value::<Vec<String>>(value.clone())
                .map_err(|_| anyhow!("{key} must be a list of tool names"))?
                .into_iter()
                .filter(|name| !name.trim().is_empty())
                .collect();
        }
    }
    Ok(())
}
fn settings(cfg: &config::Config) -> Value {
    let models: Vec<Value> = cfg.model_profiles().iter().map(|model| json!({ "name": model.name, "profile": model.provider, "default": model.is_default, "key_present": config::key_present(&model.provider.base_url) })).collect();
    let mcp: Vec<Value> = cfg.mcp.iter().map(|(id, server)| { use sha2::{Digest, Sha256}; let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(server).expect("MCP definition"))); json!({ "id": id, "hash": hash, "url": server.url, "command": redact_command(&server.command), "executable": server.command.first(), "disabled": server.disabled, "auth": server.auth, "trust": server.trust, "key_present": config::mcp_key_present(id, server), "signed_in": config::mcp_signed_in(id, server), "env_names": server.env.keys().collect::<Vec<_>>(), "allow_tools": server.allow_tools, "deny_tools": server.deny_tools, "network": server.network, "parallel": server.parallel_calls }) }).collect();
    let provider = cfg.search_provider();
    // Keep protocol availability and wire values identical to the TUI picker.
    let protocols: Vec<Value> = crate::tui_tea::MODEL_PROTOCOLS
        .iter()
        .map(|(label, available, protocol)| {
            let auth = providers::AuthKind::for_protocol(*protocol).as_str();
            let presets: Vec<Value> = match protocol {
                kernel::Protocol::OpenAiChat => config::provider_presets()
                    .iter()
                    .map(|(name, url)| json!({"name": name, "url": url, "auth": if name.contains("(local)") { "none" } else { auth }}))
                    .collect(),
                kernel::Protocol::GeminiInteractions => vec![json!({
                    "name": "Google Gemini",
                    "url": "https://generativelanguage.googleapis.com/v1",
                    "auth": auth
                })],
                _ => Vec::new(),
            };
            json!({"value": protocol, "label": label, "available": available,
                "auth": auth, "discovery": available, "providers": presets})
        })
        .collect();
    json!({ "models": models, "protocols": protocols, "presets": config::provider_presets().iter().map(|(name, url)| json!({"name": name, "url": url})).collect::<Vec<_>>(), "search": { "provider": provider.as_str(), "searxng_url": cfg.search.searxng_url, "key_present": config::search_cred_id(provider).is_some_and(config::key_present) }, "mcp": mcp, "keychain": config::prefer_keychain() })
}
fn instruction_path(workspace: &Path, kind: &str) -> Result<std::path::PathBuf> {
    match kind {
        "persona" => Ok(config::medha_home()?.join("PERSONA.md")),
        "global" => Ok(config::medha_home()?.join("MEDHA.md")),
        "project" => Ok(workspace.join("MEDHA.md")),
        "agents" => Ok(workspace.join("AGENTS.md")),
        "claude" => Ok(workspace.join("CLAUDE.md")),
        _ => bail!("Unknown instructions file"),
    }
}
/// One writer at a time per file, across every Medha process. Checking that the
/// file is still what the editor loaded, and the write that check allows, are
/// then one step: of two saves made from the same text, one is told it changed.
///
/// The lock belongs to the file's own folder, not to whoever is saving: two
/// Medhas with different homes, or reaching the folder by different names,
/// still take the same one. Other programs writing the file do not take it.
fn one_writer<T>(target: &Path, write: impl FnOnce() -> Result<T>) -> Result<T> {
    let folder = target
        .parent()
        .ok_or_else(|| anyhow!("Instructions need a folder to be saved in"))?;
    std::fs::create_dir_all(folder)?;
    #[cfg(unix)]
    let lock = std::fs::File::open(folder)?;
    // A folder cannot be opened to be locked there, so the lock is a file named after it.
    #[cfg(not(unix))]
    let lock = {
        use sha2::{Digest, Sha256};
        let named: String = Sha256::digest(folder.canonicalize()?.as_os_str().as_encoded_bytes())
            .iter()
            .take(12)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let locks = std::env::temp_dir().join("medha-locks");
        std::fs::create_dir_all(&locks)?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(locks.join(format!("{named}.lock")))?
    };
    lock.lock()?;
    write()
}

fn handle_sync(method: &str, params: &Value, workspace: &Path) -> Result<Value> {
    match method {
        "settings.list" => Ok(settings(&config::load()?.unwrap_or_default())),
        "settings.model.save" => {
            let profile: config::ProviderConfig =
                serde_json::from_value(params["profile"].clone())?;
            profile.validate().map_err(anyhow::Error::msg)?;
            let key = params["key"].as_str().filter(|key| !key.trim().is_empty());
            let given = params["name"]
                .as_str()
                .filter(|name| !name.trim().is_empty());
            let mut saved = String::new();
            config::edit(|cfg| {
                // A model added without a name is named as the TUI names one.
                let name = given.map_or_else(
                    || config::derive_profile_name(cfg, &profile.model),
                    str::to_owned,
                );
                if cfg.models.contains_key(&name) {
                    cfg.models.insert(name.clone(), profile.clone());
                    if params["default"] == true {
                        cfg.set_default_model(&name)?;
                    }
                } else {
                    cfg.add_model(name.clone(), profile.clone(), params["default"] == true)?;
                }
                if let Some(key) = key {
                    config::store_key(&profile.base_url, key)?;
                }
                saved = name;
                Ok(())
            })?;
            Ok(json!({ "saved": true, "name": saved }))
        }
        "settings.model.default" => {
            config::edit(|cfg| cfg.set_default_model(text(params, "name")?))?;
            Ok(json!({"saved": true}))
        }
        "settings.model.remove" => {
            config::edit(|cfg| cfg.remove_model(text(params, "name")?).map(|_| ()))?;
            Ok(json!({"saved": true}))
        }
        "settings.key.remove" => {
            let cfg = config::load()?.unwrap_or_default();
            let profile = cfg
                .model_profile(text(params, "name")?)
                .ok_or_else(|| anyhow!("Model not found"))?;
            config::remove_key(&profile.base_url)?;
            Ok(json!({"removed": true}))
        }
        "settings.search.save" => {
            let provider = {
                let id = text(params, "provider")?;
                if !["duckduckgo", "tavily", "brave", "searxng"].contains(&id) {
                    bail!("Unknown search provider");
                }
                tools::SearchProvider::from_id(id)
            };
            let url = params["url"]
                .as_str()
                .filter(|url| !url.trim().is_empty())
                .map(str::to_owned);
            if provider == tools::SearchProvider::Searxng {
                let parsed = url::Url::parse(
                    url.as_deref()
                        .ok_or_else(|| anyhow!("SearXNG URL required"))?,
                )?;
                if !matches!(parsed.scheme(), "http" | "https")
                    || parsed.host_str().is_none()
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                {
                    bail!("Use an HTTP or HTTPS server URL without credentials");
                }
            }
            config::edit(|cfg| {
                if let Some(key) = params["key"].as_str().filter(|key| !key.trim().is_empty()) {
                    config::store_key(
                        config::search_cred_id(provider).ok_or_else(|| {
                            anyhow!("This search provider does not use an API key")
                        })?,
                        key,
                    )?;
                }
                cfg.set_search(provider, url);
                Ok(())
            })?;
            Ok(json!({"saved": true}))
        }
        "settings.mcp.save" => {
            let args: Vec<String> = serde_json::from_value(params["args"].clone())?;
            let parsed = config::parse_mcp_add_args(args)?;
            config::edit(|cfg| {
                if let Some(key) = &parsed.key {
                    config::store_mcp_key(&parsed.id, &parsed.server, key)?;
                }
                cfg.mcp.insert(parsed.id, parsed.server);
                Ok(())
            })?;
            Ok(json!({"saved": true}))
        }
        "settings.keys" => Ok(crate::desktop_keys::list(
            &config::load()?.unwrap_or_default(),
        )),
        "settings.keys.store" => {
            let cfg = config::load()?.unwrap_or_default();
            config::move_keys(&cfg, params["keychain"] == true)?;
            Ok(json!({"saved": true}))
        }
        "settings.keys.set" | "settings.keys.remove" => {
            let cfg = config::load()?.unwrap_or_default();
            if method == "settings.keys.set" {
                crate::desktop_keys::set(&cfg, params)?;
            } else {
                crate::desktop_keys::remove(&cfg, params)?;
            }
            Ok(json!({"saved": true}))
        }
        "settings.tools" => {
            let lock = lockfile::MedhaLock::load(workspace.join("medha.lock"))?;
            let env = std::env::var("MEDHA_TOOLS").ok();
            let (preset, from) = match (&env, &lock) {
                (Some(preset), _) => (preset.clone(), "MEDHA_TOOLS environment variable"),
                (None, Some(lock)) => (lock.tools.preset.clone(), "this project’s medha.lock"),
                (None, None) => ("full".to_owned(), "default"),
            };
            Ok(json!({
                "preset": preset,
                "from": from,
                "env_override": env.is_some(),
                "minimal": lockfile::MINIMAL_TOOLS,
            }))
        }
        "settings.tools.save" => {
            let path = workspace.join("medha.lock");
            if path.is_symlink() {
                bail!("medha.lock is a symbolic link; change it in your editor");
            }
            let current = match std::fs::read_to_string(&path) {
                Ok(current) => current,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(error.into()),
            };
            let edited = crate::lock_edit::set_tools_preset(&current, text(params, "preset")?)?;
            let temporary = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
            std::fs::write(&temporary, edited)?;
            std::fs::rename(&temporary, &path)?;
            Ok(json!({"saved": true}))
        }
        "settings.health" => {
            let cfg = config::load()?;
            Ok(health_view(&config::pulse(
                cfg.as_ref(),
                None,
                None,
                Some(workspace),
            )))
        }
        "settings.health.fix" => {
            let mut applied = Vec::new();
            config::edit(|cfg| {
                applied = config::apply_safe_fixes(cfg);
                Ok(())
            })?;
            Ok(json!({"applied": applied}))
        }
        "settings.mcp.update" => {
            let id = text(params, "id")?;
            config::edit(|cfg| {
                let server = cfg
                    .mcp
                    .get_mut(id)
                    .ok_or_else(|| anyhow!("MCP server not found"))?;
                apply_mcp_update(server, params)
            })?;
            Ok(json!({"saved": true}))
        }
        "settings.mcp.signout" => {
            let id = text(params, "id")?;
            let cfg = config::load()?.unwrap_or_default();
            let server = cfg
                .mcp
                .get(id)
                .ok_or_else(|| anyhow!("MCP server not found"))?;
            config::mcp_sign_out(id, server);
            Ok(json!({"signed_out": true}))
        }
        "settings.mcp.remove" => {
            config::edit(|cfg| {
                let id = text(params, "id")?;
                let server = cfg
                    .mcp
                    .remove(id)
                    .ok_or_else(|| anyhow!("MCP server not found"))?;
                config::delete_mcp_key(id, &server);
                Ok(())
            })?;
            Ok(json!({"removed": true}))
        }
        "instructions.list" => {
            let mut rows = Vec::new();
            for (kind, title) in [
                ("persona", "Persona"),
                ("global", "Your instructions"),
                ("project", "Project instructions"),
                ("agents", "Agent instructions"),
                ("claude", "Claude-compatible instructions"),
            ] {
                let path = instruction_path(workspace, kind)?;
                if path.is_symlink() {
                    bail!(
                        "Instructions {} points to another file; edit it in your editor",
                        path.display()
                    );
                }
                let exists = path.exists();
                let content = if exists {
                    let size = std::fs::metadata(&path)?.len();
                    if size > 128 * 1024 {
                        bail!("Instructions {} exceeds the editor limit", path.display());
                    }
                    std::fs::read_to_string(&path)?
                } else {
                    String::new()
                };
                rows.push(json!({ "kind": kind, "title": title, "path": path, "exists": exists, "content": content }));
            }
            Ok(json!({"files": rows}))
        }
        "instructions.save" => {
            let path = instruction_path(workspace, text(params, "kind")?)?;
            let content = params["content"]
                .as_str()
                .ok_or_else(|| anyhow!("Instructions must be text"))?;
            if content.len() > 128 * 1024 {
                bail!("Keep instructions below 128 KB");
            }
            if path.is_symlink() {
                bail!("This instructions file is a symbolic link; edit it in your editor");
            }
            let before = params["before"]
                .as_str()
                .ok_or_else(|| anyhow!("Original instructions are required"))?;
            one_writer(&path, || {
                let current = match std::fs::read_to_string(&path) {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(error) => return Err(error.into()),
                };
                if current != before {
                    bail!("Instructions changed elsewhere. Reload before saving.");
                }
                let temporary = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(&temporary, content)?;
                std::fs::rename(&temporary, &path)?;
                Ok(json!({"saved": true}))
            })
        }
        "extensions.list" => {
            let store = plugin_store(workspace)?;
            let mut result = plugins(&store)?;
            let skills = tools::SkillStore::new(
                workspace.join(".medha/skills"),
                Some(config::user_skills_dir()?),
            );
            let mut discovered = crate::plugin_session::SessionPlugins::discover(store);
            discovered.add_skills(&skills);
            let mut catalog = skills.list(&std::collections::HashSet::new());
            if let Some(rows) = catalog["skills"].as_array_mut() {
                for row in rows {
                    row["available"] = Value::Null;
                }
            }
            result["skills"] = catalog["skills"].clone();
            result["skill_errors"] = catalog["errors"].clone();
            Ok(result)
        }
        "extensions.skill.configure" => {
            let skills = tools::SkillStore::new(
                workspace.join(".medha/skills"),
                Some(config::user_skills_dir()?),
            );
            skills
                .set_enabled(
                    text(params, "name")?,
                    params["enabled"]
                        .as_bool()
                        .ok_or_else(|| anyhow!("enabled must be a boolean"))?,
                )
                .map_err(anyhow::Error::msg)?;
            Ok(json!({"saved": true}))
        }
        "extensions.install" => {
            let source = text(params, "source")?;
            let source = if let Some(rest) = source.strip_prefix("~/") {
                config::medha_home()?
                    .parent()
                    .ok_or_else(|| anyhow!("Home is unavailable"))?
                    .join(rest)
                    .to_string_lossy()
                    .into_owned()
            } else {
                source.into()
            };
            let package = plugin_store(workspace)?
                .install_from(&source, &plugins_cmd::marketplaces(&config::medha_home()?))?;
            Ok(json!({ "id": package.manifest.id, "installed": true, "enabled": false }))
        }
        "extensions.marketplace.list" => {
            let markets = plugins_cmd::marketplaces(&config::medha_home()?);
            marketplace_view(&markets, &plugin_store(workspace)?)
        }
        "extensions.marketplace.add" => {
            let spec = text(params, "source")?;
            let extensions::sources::Spec::Git(source) = extensions::sources::parse(spec) else {
                bail!("A marketplace is a repository: owner/repo or a git URL");
            };
            let markets = plugins_cmd::marketplaces(&config::medha_home()?);
            let added = markets.add(source)?;
            Ok(json!({"name": added.name, "plugins": added.plugins.len()}))
        }
        "extensions.marketplace.refresh" => {
            // The first refresh also fetches the catalogs Medha ships with.
            let markets = plugins_cmd::marketplaces(&config::medha_home()?);
            let mut problems: Vec<String> = markets
                .add_defaults()
                .into_iter()
                .filter_map(Result::err)
                .map(|error| error.to_string())
                .collect();
            for market in markets.list()? {
                if let Err(error) = markets.refresh(&market.name) {
                    problems.push(format!("{}: {error}", market.name));
                }
            }
            Ok(json!({"problems": problems}))
        }
        "extensions.marketplace.remove" => {
            plugins_cmd::marketplaces(&config::medha_home()?).remove(text(params, "name")?)?;
            Ok(json!({"removed": true}))
        }
        "extensions.hooks.list" => {
            let store = plugin_store(workspace)?;
            let package = store
                .discover()?
                .plugins
                .into_iter()
                .find(|plugin| plugin.package.manifest.id == extensions::PROJECT_HOOKS_ID);
            let state = package.map(|plugin| {
                let grant = plugin.requested_grant();
                json!({
                    "id": plugin.package.manifest.id,
                    "scope": plugin.scope.as_str(),
                    "activation": plugin.activation.as_str(),
                    "hash": plugin.package.content_hash,
                    "grant": grant.as_ref().ok(),
                    "blocked": grant.err().map(|error| error.to_string()),
                })
            });
            Ok(json!({
                "events": crate::hook_files::EVENTS.iter().map(|(name, description, tools)| json!({"name": name, "description": description, "tools": tools})).collect::<Vec<_>>(),
                "tools": crate::hook_files::TOOLS.iter().map(|(matcher, label)| json!({"matcher": matcher, "label": label})).collect::<Vec<_>>(),
                "hooks": crate::hook_files::list(workspace),
                "project": state,
            }))
        }
        "extensions.hooks.add" => {
            let command = text(params, "command")?;
            let matcher = params["matcher"].as_str().unwrap_or("*");
            let path = crate::hook_files::add(workspace, text(params, "event")?, matcher, command)
                .map_err(|error| anyhow!(error))?;
            Ok(json!({"path": path}))
        }
        "extensions.hooks.remove" => {
            crate::hook_files::remove(workspace, text(params, "event")?, text(params, "file")?)
                .map_err(|error| anyhow!(error))?;
            Ok(json!({"removed": true}))
        }
        "extensions.doctor" => {
            let checks = extensions::doctor::run(&plugin_store(workspace)?);
            Ok(json!({"checks": checks.iter().map(|check| json!({
                "level": match check.level {
                    extensions::doctor::Level::Ok => "ok",
                    extensions::doctor::Level::Warn => "warn",
                    extensions::doctor::Level::Fail => "fail",
                },
                "subject": check.subject,
                "message": check.message,
            })).collect::<Vec<_>>()}))
        }
        "extensions.remove" => {
            plugin_store(workspace)?.remove(text(params, "id")?)?;
            Ok(json!({"removed": true}))
        }
        "extensions.rollback" => {
            plugin_store(workspace)?.rollback(text(params, "id")?)?;
            Ok(json!({"saved": true}))
        }
        "extensions.skill.remove" => {
            tools::SkillStore::new(
                workspace.join(".medha/skills"),
                Some(config::user_skills_dir()?),
            )
            .remove_user(text(params, "name")?)
            .map_err(anyhow::Error::msg)?;
            Ok(json!({"removed": true}))
        }
        "extensions.enable" | "extensions.disable" => {
            let store = plugin_store(workspace)?;
            let id = text(params, "id")?;
            let scope: extensions::Scope = serde_json::from_value(params["scope"].clone())?;
            if method == "extensions.disable" {
                store.disable(id, Some(scope))?;
            } else {
                let grant: extensions::Grant = serde_json::from_value(params["grant"].clone())?;
                store.enable_reviewed(id, Some(scope), text(params, "hash")?, &grant)?;
            }
            Ok(json!({"saved": true}))
        }
        _ => bail!("Unknown settings action"),
    }
}

pub(crate) async fn handle(method: &str, params: &Value, workspace: &Path) -> Result<Value> {
    if let Some(result) = crate::desktop_skills::handle(method, params, workspace).await {
        return result;
    }
    match method {
        "extensions.mcp.registry" => {
            crate::desktop_mcp_registry::search(
                params["query"].as_str().unwrap_or_default(),
                params["cursor"].as_str(),
                if params["source"] == "full" {
                    crate::desktop_mcp_registry::Source::Full
                } else {
                    crate::desktop_mcp_registry::Source::Featured
                },
            )
            .await
        }
        "settings.model.discover" => {
            let profile: config::ProviderConfig =
                serde_json::from_value(params["profile"].clone())?;
            let credential = params["key"]
                .as_str()
                .filter(|key| !key.is_empty())
                .map(str::to_owned)
                .or_else(|| config::load_key(&profile.base_url))
                .unwrap_or_default();
            let models =
                providers::openai_compat::list_models_for_profile(&profile, &credential).await?;
            Ok(
                json!({"models": models.iter().map(|model| json!({"id": model.id, "context_length": model.context_length})).collect::<Vec<_>>() }),
            )
        }
        // The size a session would fall back to when neither config nor server gives one.
        "settings.model.context" => Ok(json!({
            "published": providers::models_dev::context_window(text(params, "model")?).await
        })),
        "extensions.skill.install" => {
            let skills = tools::SkillStore::new(
                workspace.join(".medha/skills"),
                Some(config::user_skills_dir()?),
            );
            let source = text(params, "source")?;
            let source = if let Some(rest) = source.strip_prefix("~/") {
                config::medha_home()?
                    .parent()
                    .ok_or_else(|| anyhow!("Home unavailable"))?
                    .join(rest)
                    .to_string_lossy()
                    .into_owned()
            } else {
                source.into()
            };
            let report = skills
                .install_from(&source)
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
        "extensions.update.preview" | "extensions.update.apply" => {
            let store = plugin_store(workspace)?;
            let plan = store.plan_update(text(params, "id")?)?;
            if method.ends_with("preview") {
                return Ok(
                    json!({"id": plan.id, "from_version": plan.from_version, "to_version": plan.to_version, "from_commit": plan.from_commit, "to_commit": plan.to_commit, "up_to_date": plan.up_to_date(), "access_changed": plan.access_changed(), "access_before": plan.access_before, "access_after": plan.access_after.as_ref().ok(), "blocked": plan.access_after.as_ref().err()}),
                );
            }
            if params["from_commit"] != plan.from_commit || params["to_commit"] != plan.to_commit {
                bail!("The plugin changed since review. Check for updates again.");
            }
            store.apply_update(plan, false)?;
            Ok(json!({"saved": true}))
        }
        _ => handle_sync(method, params, workspace),
    }
}

#[cfg(test)]
#[path = "desktop_preferences_tests.rs"]
mod tests;
