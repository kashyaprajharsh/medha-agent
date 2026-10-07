//! Frontend forms produce typed commands; receipts, not optimistic UI state,
//! determine whether an application change succeeded.
fn redact_env_pair(pair: &str) -> String {
    match pair.split_once('=') {
        Some((name, _)) if !name.is_empty() => format!("{name}=<redacted>"),
        _ => "<redacted>".into(),
    }
}

fn redact_secrets(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut pending: Option<&'static str> = None;
    for word in line.split_whitespace() {
        match pending.take() {
            Some("env") => {
                out.push(redact_env_pair(word));
                continue;
            }
            Some(_) => {
                out.push("<redacted>".into());
                continue;
            }
            None => {}
        }
        match word.split_once('=') {
            Some((flag, _)) if config::MCP_SECRET_FLAGS.contains(&flag) => {
                out.push(format!("{flag}=<redacted>"));
            }
            Some((flag, rest)) if flag == config::MCP_ENV_FLAG => {
                out.push(format!("{flag}={}", redact_env_pair(rest)));
            }
            _ => {
                pending = if word == config::MCP_ENV_FLAG {
                    Some("env")
                } else if config::MCP_SECRET_FLAGS.contains(&word) {
                    Some("secret")
                } else {
                    None
                };
                out.push(word.to_string());
            }
        }
    }
    out.join(" ")
}

/// `/mcp add <id> --url <https://…> [--oauth | --bearer T]` for a hosted server,
/// or `/mcp add <id> [--key K] [--trust trusted] [--env K=V] [--allow-tool P]
/// [--deny-tool P] [--no-network] [--parallel] -- <command>` for a local one.
fn steer_target(rest: &str, running: &[(String, String)]) -> Result<(String, String), String> {
    let missing = || "nothing to send — /agents steer <agent> <message>".to_string();
    let addressed = |word: &str| running.iter().any(|(name, id)| name == word || id == word);

    let (id, text) = match rest.split_once(char::is_whitespace) {
        Some((first, tail)) if addressed(first) => (first.to_string(), tail.trim().to_string()),
        // A bare agent name is a command someone stopped typing, not a message
        // that happens to read like one. Sent verbatim, the agent spends a turn
        // reading its own name and the sender is told it was delivered.
        None if addressed(rest.trim()) => return Err(missing()),
        // No recognised id in front: the whole thing is the message, which is
        // only unambiguous with exactly one agent running.
        _ if running.len() == 1 => (running[0].1.clone(), rest.trim().to_string()),
        _ => {
            let names: Vec<&str> = running.iter().map(|(name, _)| name.as_str()).collect();
            return Err(format!(
                "several agents are running — say which: /agents steer <agent> <message>  ({})",
                names.join(", ")
            ));
        }
    };
    match text.is_empty() {
        true => Err(missing()),
        false => Ok((id, text)),
    }
}
fn lsp_inventory_line(payload: &serde_json::Value) -> String {
    let Some(available) = payload
        .get("available")
        .and_then(serde_json::Value::as_array)
    else {
        return String::new();
    };
    let installed: Vec<&str> = available
        .iter()
        .filter(|entry| entry["installed"].as_bool() == Some(true))
        .filter_map(|entry| entry["server"].as_str())
        .collect();
    let fetchable: Vec<&str> = available
        .iter()
        .filter(|entry| {
            entry["installed"].as_bool() != Some(true)
                && entry["installable"].as_bool() == Some(true)
        })
        .filter_map(|entry| entry["server"].as_str())
        .collect();
    let mut text = format!(
        "  {}/{} servers available here",
        installed.len(),
        available.len()
    );
    if !installed.is_empty() {
        text.push_str(&format!(": {}", installed.join(", ")));
    }
    if !fetchable.is_empty() {
        text.push_str(&format!(
            "\n  · medha lsp install <id>   ({})",
            fetchable.join(", ")
        ));
    }
    text.push_str("\n  · a file with no server falls back to text search");
    text
}

use super::backend_ui::{After, Draft, Effect, request};
use super::*;
use protocol::{
    ChangeResource as Change, ReadResource as Read, ResourceResult as Resource,
    SessionRead as Inspect,
};

pub(super) fn submit(model: &mut Model) {
    if model.input.ends_with('\\') {
        model.input.pop();
        model.cursor = model.input.len();
        model.insert_char('\n');
        return;
    }
    let line = model.input.trim().to_string();
    if line.is_empty() && model.images.is_empty() {
        return;
    }
    if line.starts_with('/') {
        let name = line.split_whitespace().next().unwrap_or("");
        let plugin = model.remote.as_ref().is_some_and(|peer| {
            peer.plugins
                .commands
                .iter()
                .any(|command| command.name == name)
        });
        if !plugin && is_slash_command(&line) {
            // Secret-bearing commands are redacted before entering history.
            remember(model, redact_secrets(&line));
            model.input.clear();
            model.cursor = 0;
            model.command_draft = Some(line.clone());
            model.command_refused = false;
            dispatch_slash(model, line.trim_start_matches('/'));
            model.command_draft = None;
            if model.command_refused {
                backend_ui::restore_draft(model, line);
            }
            return;
        }
    }
    send(model, line);
}

pub(super) fn send(model: &mut Model, line: String) {
    let Some(peer) = model.remote.as_ref() else {
        return;
    };
    let intent = if model.running {
        protocol::SendIntent::Steer { turn: peer.turn }
    } else {
        protocol::SendIntent::Start
    };
    let target = model.focus.as_ref().map(ToString::to_string);
    let followup = target.as_ref().is_some_and(|path| {
        peer.live
            .agents
            .iter()
            .any(|agent| &agent.path == path && agent.status.is_some())
    });
    send_held(
        model,
        backend_ui::DeferredSend {
            raw: line,
            intent,
            target,
            followup,
        },
    );
}

pub(super) fn send_held(model: &mut Model, held: backend_ui::DeferredSend) {
    let backend_ui::DeferredSend {
        raw: line,
        intent,
        target,
        followup,
    } = held;
    let Some(peer) = model.remote.as_ref() else {
        return;
    };
    if peer.sending
        || peer.recovering
        || model.session_op.is_some()
        || model.force_aborting
        || model.backend_deferred.is_some()
    {
        model.push_notice("The preceding request is still finishing — your draft is preserved.");
        backend_ui::restore_draft(model, line);
        return;
    }
    if model.images.is_loading() {
        if model.input.trim() == line {
            model.input.clear();
            model.cursor = 0;
        }
        model.backend_deferred = Some(backend_ui::DeferredSend {
            raw: line,
            intent,
            target,
            followup,
        });
        return;
    }
    let expanded = match model.resolve_pastes(&line) {
        Ok(expanded) => expanded,
        Err(error) => {
            model.push_notice(error);
            backend_ui::restore_draft(model, line);
            return;
        }
    };
    let paths = input::unstaged_images(model, &expanded);
    if !paths.is_empty() {
        if stage_images(model, input::ImageSource::Paths(paths)) {
            if model.input.trim() == line {
                model.input.clear();
                model.cursor = 0;
            }
            model.backend_deferred = Some(backend_ui::DeferredSend {
                raw: line,
                intent,
                target,
                followup,
            });
        } else {
            backend_ui::restore_draft(model, line);
        }
        return;
    }
    let Some(peer) = model.remote.as_ref() else {
        return;
    };
    if target.is_some() && !model.images.is_empty() {
        model.push_notice(
            "Image attachments can be sent to the conversation; switch back with Esc first.",
        );
        backend_ui::restore_draft(model, line);
        return;
    }
    let attached = crate::attachments::refs::scan(&expanded, model.restore.root());
    let content = input::message_for(model, &expanded, &attached);
    let plugin = content.split_whitespace().next().is_some_and(|name| {
        peer.plugins
            .commands
            .iter()
            .any(|command| command.name == name)
    });
    let wire = protocol::SendMessage {
        content,
        images: model.images.wire(),
        intent: Some(intent),
    };
    // Account for the RPC envelope before scheduling; a rejected large message
    // must preserve both its draft and immutable staged attachments.
    if crate::chat_presentation::size(&wire) > wire::MAX_FRAME.saturating_sub(4096) {
        model.push_notice("This message exceeds the backend frame limit. Split it into smaller messages; your draft and attachments are preserved.");
        backend_ui::restore_draft(model, line);
        return;
    }
    let draft = Draft {
        raw: line.clone(),
        request: wire,
        target,
        followup,
    };
    if request(
        model,
        if plugin {
            Effect::Expand(draft)
        } else {
            Effect::Send(draft)
        },
    ) {
        model.remote.as_mut().expect("viewer").sending = true;
        model.plugin_command_busy = plugin;
        if model.input.trim() == line {
            model.input.clear();
            model.cursor = 0;
        }
        remember(model, line);
        model.history_idx = None;
        model.welcome = false;
    } else {
        backend_ui::restore_draft(model, line);
    }
}

fn remember(model: &mut Model, line: String) {
    if line.len() <= 128 * 1024 && model.history.last() != Some(&line) {
        model.history.push(line);
    }
    while model.history.len() > 128
        || model.history.iter().map(String::len).sum::<usize>() > 1024 * 1024
    {
        model.history.remove(0);
    }
}

pub(super) fn stage_images(model: &mut Model, source: input::ImageSource) -> bool {
    if model.remote.as_ref().is_some_and(|peer| peer.sending) {
        model.push_notice("Wait for this message's receipt before changing its attachments.");
        return false;
    }
    match model.images.begin() {
        Ok(generation) => {
            if request(model, Effect::Images { generation, source }) {
                true
            } else {
                model.images.failed(generation);
                false
            }
        }
        Err(error) => {
            model.push_notice(error);
            false
        }
    }
}

pub(super) fn dispatch_slash(model: &mut Model, command: &str) {
    use input::SlashAction as A;
    match input::classify_slash(command) {
        A::Resume => {
            request(model, Effect::Read(Read::Sessions, After::Notice));
        }
        A::Rewind => {
            if boundary_allowed(model) {
                request(model, Effect::Points);
            }
        }
        A::Clear => {
            if boundary_allowed(model) && request(model, Effect::Clear) {
                model.session_op = Some(SessionOp::Opening);
            }
        }
        A::Attach(path) => {
            let paths = crate::attachments::refs::scan(&path, model.restore.root());
            if paths.is_empty() {
                model.push_notice("usage: /attach <image path> · /attach paste · /attach remove");
            } else {
                stage_images(model, input::ImageSource::Paths(paths));
            }
        }
        A::Paste => {
            stage_images(model, input::ImageSource::Clipboard);
        }
        A::Detach(arg) => {
            if model.remote.as_ref().is_some_and(|peer| peer.sending) {
                model.push_notice("Wait for the message receipt before removing its attachments.");
            } else {
                if let Some(held) = model.backend_deferred.take() {
                    backend_ui::restore_draft(model, held.raw);
                }
                let notice = model.images.detach(&arg);
                model.push_notice(notice);
            }
        }
        A::Lsp => {
            request(model, Effect::Inspect(Inspect::Lsp, After::Notice));
        }
        A::Plugins(args) => backend_plugins::run_command(model, &args),
        A::Hooks(args) => backend_plugins::hooks_command(model, &args),
        A::Mcp => open_mcp_picker(model),
        A::McpStart(id) => start_mcp_server(model, &id),
        A::McpAdd(args) => mcp_add(model, &args),
        A::McpCatalog(query) => search_mcp_catalog(model, &query),
        A::Connect(query) => connect_command(model, &query),
        A::Agents => open_agents_picker(model),
        A::Steer(args) => agents_steer(model, &args),
        A::Followup(args) => agents_followup(model, &args),
        A::Tree => show_agent_tree(model),
        A::Usage => {
            request(model, Effect::Inspect(Inspect::Usage, After::Notice));
        }
        A::Memory(args) => memory_command(model, &args),
        A::SkillPicker => open_skill_picker(model),
        A::LoadSkill(name) => load_skill_by_name(model, &name),
        A::SkillInfo(name) => show_skill_info(model, &name),
        A::RemoveSkill(name) => begin_remove_skill(model, &name),
        A::InstallSkill(source) => install_skill(model, &source),
        A::SkillSources(args) => skill_sources(model, &args),
        A::SearchSkills(query) => search_skills(model, &query),
        A::AddSkill(arg) => add_skill(model, &arg),
        A::UpdateSkills(arg) => update_skills(model, &arg),
        A::LockSkills => lock_skills(model),
        A::SyncSkills => sync_skills(model),
        A::ModelPicker => open_model_picker(model),
        A::AddModel => begin_model_setup(model),
        A::SwitchModel(name) => switch_saved_model(model, &name),
        A::SearchConfig => begin_search_setup(model),
        A::ModePicker => open_mode_picker(model),
        A::SwitchMode(mode) => match kernel::AutonomyLevel::parse(&mode) {
            Ok(level) => set_autonomy(model, level),
            Err(_) => model.push_notice("usage: /mode plan|careful|normal|yolo"),
        },
        A::Other => local_command(model, command),
    }
}

fn boundary_allowed(model: &mut Model) -> bool {
    if model.foreground_owned()
        || model.has_active_agents()
        || model.bg_running() > 0
        || model.session_op.is_some()
        || model.backend_deferred.is_some()
        || model.images.is_loading()
    {
        model.command_refused = true;
        model.push_notice(
            "Finish or stop active turns, agents and shell tasks before changing the conversation.",
        );
        false
    } else {
        true
    }
}

pub(super) fn resume(model: &mut Model, id: String) {
    if !boundary_allowed(model) {
        return;
    }
    let ids = model
        .remote
        .as_ref()
        .and_then(|peer| peer.conversation.parse().ok())
        .zip(id.parse().ok());
    if request(model, Effect::Resume(id))
        && let Some((source, target)) = ids
    {
        model.session_op = Some(SessionOp::Resume { source, target });
    }
}
pub(super) fn rewind(model: &mut Model, event: String, scope: RewindScope) {
    if !boundary_allowed(model) {
        return;
    }
    let source = model
        .remote
        .as_ref()
        .and_then(|peer| peer.conversation.parse().ok());
    let scope = match scope {
        RewindScope::Conversation => protocol::RewindScope::Conversation,
        RewindScope::Code => protocol::RewindScope::Code,
        RewindScope::ConversationAndCode => protocol::RewindScope::Both,
    };
    if request(model, Effect::Rewind(protocol::Rewind { at: event, scope }))
        && let Some(source) = source
    {
        model.session_op = Some(SessionOp::Rewind { source });
    }
}

pub(super) fn handle_model_setup_key(model: &mut Model, key: KeyEvent) -> bool {
    if model.model_setup.is_none()
        || matches!(
            model.picker.as_ref().map(|picker| &picker.kind),
            Some(
                PickerKind::ModelProtocol
                    | PickerKind::ProviderPreset
                    | PickerKind::ModelDiscovery(_)
            )
        )
    {
        return false;
    }
    if model.model_setup.as_ref().is_some_and(|setup| {
        matches!(
            setup.step,
            ModelSetupStep::Discovering | ModelSetupStep::Saving | ModelSetupStep::Activating
        )
    }) && key.code != KeyCode::Esc
    {
        return true;
    }
    match key.code {
        KeyCode::Esc => {
            model.form_generation = model.form_generation.wrapping_add(1);
            model.model_setup = None;
            model.push_notice("model setup cancelled — /model reopens it");
        }
        KeyCode::Backspace => model.backspace(),
        KeyCode::Left => model.move_left(),
        KeyCode::Right => model.move_right(),
        KeyCode::Enter => advance_model_setup(model),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => model.insert_char(c),
        _ => {}
    }
    true
}
pub(super) fn begin_model_setup(model: &mut Model) {
    if !boundary_allowed(model) {
        return;
    }
    model.form_generation = model.form_generation.wrapping_add(1);
    model.search_setup = None;
    model.mcp_credential = None;
    model.model_setup = Some(ModelSetup::new());
    model.picker = Some(Picker::new(PickerKind::ModelProtocol));
}
pub(super) fn begin_model_key_update(model: &mut Model, profile: config::ModelProfile) {
    if !boundary_allowed(model) {
        return;
    }
    model.form_generation = model.form_generation.wrapping_add(1);
    model.search_setup = None;
    model.mcp_credential = None;
    model.model_setup = Some(ModelSetup::update_key(
        profile.name,
        profile.provider.protocol,
        profile.provider.base_url,
    ));
}
fn advance_model_setup(model: &mut Model) {
    let value = model.edited().0.trim().to_string();
    let Some(setup) = model.model_setup.as_mut() else {
        return;
    };
    let effect = match setup.step {
        ModelSetupStep::BaseUrl if !value.is_empty() => {
            setup.base_url = value.trim_end_matches('/').into();
            setup.step = ModelSetupStep::ApiKey;
            None
        }
        ModelSetupStep::ApiKey => {
            if let ModelSetupMode::UpdateKey { profile } = &setup.mode {
                if value.is_empty() {
                    model.push_notice("An API key is required.");
                    return;
                }
                Some(Effect::Change(
                    Change::UpdateModelKey {
                        name: profile.clone(),
                        key: protocol::Secret(value),
                    },
                    After::KeySaved(profile.clone()),
                ))
            } else {
                if setup.protocol == kernel::Protocol::GeminiInteractions && value.is_empty() {
                    model.push_notice("A Gemini API key is required.");
                    return;
                }
                setup.api_key = value;
                setup.step = ModelSetupStep::Discovering;
                Some(Effect::Discover(protocol::DiscoverModels {
                    protocol: model_protocol(setup.protocol),
                    base_url: setup.base_url.clone(),
                    key: protocol::Secret(setup.api_key.clone()),
                }))
            }
        }
        ModelSetupStep::ModelId if !value.is_empty() => {
            setup.model = value;
            setup.step = ModelSetupStep::ContextWindow;
            None
        }
        ModelSetupStep::ContextWindow => {
            let tokens = if value.is_empty() {
                None
            } else {
                match value.parse::<u32>() {
                    Ok(tokens) if tokens > 0 => Some(tokens),
                    _ => {
                        model.push_notice(
                            "Context window must be a positive whole number, or blank.",
                        );
                        return;
                    }
                }
            };
            setup.max_ctx = setup.max_ctx.or(tokens);
            finish_model_setup(model);
            return;
        }
        ModelSetupStep::Discovering | ModelSetupStep::Saving | ModelSetupStep::Activating => return,
        _ => {
            model.push_notice("This field is required.");
            return;
        }
    };
    if let Some(effect) = effect
        && !request(model, effect)
    {
        if let Some(setup) = &mut model.model_setup {
            setup.step = ModelSetupStep::ApiKey;
        }
        return;
    }
    if let Some(setup) = &mut model.model_setup
        && matches!(setup.mode, ModelSetupMode::UpdateKey { .. })
    {
        setup.step = ModelSetupStep::Saving;
    } else {
        model.clear_edited();
    }
}
pub(super) fn finish_model_setup(model: &mut Model) {
    let Some(setup) = model.model_setup.as_ref() else {
        return;
    };
    if matches!(
        setup.step,
        ModelSetupStep::Saving | ModelSetupStep::Activating
    ) {
        return;
    }
    let draft = protocol::ModelDraft {
        name: None,
        protocol: model_protocol(setup.protocol),
        base_url: setup.base_url.clone(),
        model: setup.model.clone(),
        context_limit: setup.max_ctx,
        key: (!setup.api_key.is_empty()).then(|| protocol::Secret(setup.api_key.clone())),
    };
    if request(
        model,
        Effect::Change(Change::SaveModel(draft), After::Models),
    ) {
        model.model_setup.as_mut().expect("setup").step = ModelSetupStep::Saving;
    }
}
fn model_protocol(protocol: kernel::Protocol) -> protocol::ModelProtocol {
    match protocol {
        kernel::Protocol::OpenAiChat => protocol::ModelProtocol::OpenAiChat,
        kernel::Protocol::GeminiInteractions => protocol::ModelProtocol::GeminiInteractions,
        kernel::Protocol::AnthropicMessages => protocol::ModelProtocol::AnthropicMessages,
        kernel::Protocol::OpenAiResponses => protocol::ModelProtocol::OpenAiResponses,
    }
}
fn profiles(catalogue: &protocol::ModelCatalogue) -> Vec<config::ModelProfile> {
    catalogue
        .profiles
        .iter()
        .map(|profile| {
            let mut provider = providers::ProviderProfile::openai_chat(
                &profile.base_url,
                &profile.model,
                if profile.requires_key {
                    providers::AuthKind::Bearer
                } else {
                    providers::AuthKind::None
                },
            );
            provider.protocol = match profile.protocol {
                protocol::ModelProtocol::OpenAiChat => kernel::Protocol::OpenAiChat,
                protocol::ModelProtocol::GeminiInteractions => kernel::Protocol::GeminiInteractions,
                protocol::ModelProtocol::AnthropicMessages => kernel::Protocol::AnthropicMessages,
                protocol::ModelProtocol::OpenAiResponses => kernel::Protocol::OpenAiResponses,
            };
            provider.max_ctx = profile.context_limit;
            config::ModelProfile {
                name: profile.name.clone(),
                provider,
                is_default: profile.default,
            }
        })
        .collect()
}
pub(super) fn open_model_picker(model: &mut Model) {
    request(model, Effect::Read(Read::Models, After::Models));
}
pub(super) fn switch_saved_model(model: &mut Model, name: &str) {
    if boundary_allowed(model) {
        request(
            model,
            Effect::SavedProfile(protocol::ActivateSavedProfile {
                profile: name.into(),
            }),
        );
    }
}
pub(super) fn remove_saved_model(model: &mut Model, name: &str) {
    if name == model.active_profile {
        model.push_notice("Switch to another model before removing the active profile.");
        return;
    }
    request(
        model,
        Effect::Change(Change::RemoveModel { name: name.into() }, After::Models),
    );
}
pub(super) fn set_default_model(model: &mut Model, name: &str) {
    request(
        model,
        Effect::Change(Change::SetDefaultModel { name: name.into() }, After::Models),
    );
}
pub(super) fn open_mode_picker(model: &mut Model) {
    let selected = AUTONOMY_MODES
        .iter()
        .position(|(level, _)| *level == model.autonomy)
        .unwrap_or_default();
    model.picker = Some(Picker::with_selected(PickerKind::AutonomyMode, selected));
}
pub(super) fn set_autonomy(model: &mut Model, level: kernel::AutonomyLevel) {
    if !boundary_allowed(model) {
        return;
    }
    let mode = match level {
        kernel::AutonomyLevel::Plan => protocol::Mode::Plan,
        kernel::AutonomyLevel::Careful => protocol::Mode::Careful,
        kernel::AutonomyLevel::Normal => protocol::Mode::Normal,
        kernel::AutonomyLevel::Yolo => protocol::Mode::Yolo,
    };
    request(model, Effect::Configure(protocol::Configure::Mode(mode)));
}
pub(super) fn begin_search_setup(model: &mut Model) {
    if !boundary_allowed(model) {
        return;
    }
    model.form_generation = model.form_generation.wrapping_add(1);
    model.model_setup = None;
    model.mcp_credential = None;
    model.search_setup = Some(SearchSetup::new());
    model.picker = Some(Picker::new(PickerKind::SearchProvider));
}
pub(super) fn handle_search_setup_key(model: &mut Model, key: KeyEvent) -> bool {
    if model.search_setup.is_none()
        || matches!(
            model.picker.as_ref().map(|p| &p.kind),
            Some(PickerKind::SearchProvider)
        )
    {
        return false;
    }
    if model
        .search_setup
        .as_ref()
        .is_some_and(|setup| setup.step == SearchSetupStep::Saving)
        && key.code != KeyCode::Esc
    {
        return true;
    }
    match key.code {
        KeyCode::Esc => {
            model.form_generation = model.form_generation.wrapping_add(1);
            model.search_setup = None;
        }
        KeyCode::Backspace => model.backspace(),
        KeyCode::Left => model.move_left(),
        KeyCode::Right => model.move_right(),
        KeyCode::Enter => {
            let provider = model.search_setup.as_ref().expect("form").provider;
            if model.edited().0.trim().is_empty() {
                model.push_notice("An API key or instance URL is required.");
            } else {
                let url = (provider == tools::SearchProvider::Searxng)
                    .then(|| model.edited().0.trim().to_string());
                commit_search(model, provider, url);
            }
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => model.insert_char(c),
        _ => {}
    }
    true
}
pub(super) fn commit_search(
    model: &mut Model,
    provider: tools::SearchProvider,
    url: Option<String>,
) {
    if model
        .search_setup
        .as_ref()
        .is_some_and(|setup| setup.step == SearchSetupStep::Saving)
    {
        return;
    }
    let key = matches!(
        provider,
        tools::SearchProvider::Tavily | tools::SearchProvider::Brave
    )
    .then(|| protocol::Secret(model.edited().0.trim().into()));
    let provider = match provider {
        tools::SearchProvider::DuckDuckGo => protocol::SearchProvider::Duckduckgo,
        tools::SearchProvider::Tavily => protocol::SearchProvider::Tavily,
        tools::SearchProvider::Brave => protocol::SearchProvider::Brave,
        tools::SearchProvider::Searxng => protocol::SearchProvider::Searxng,
    };
    if request(
        model,
        Effect::Change(Change::Search { provider, key, url }, After::SearchSaved),
    ) && let Some(setup) = &mut model.search_setup
    {
        setup.step = SearchSetupStep::Saving;
    }
}

pub(super) fn after_reload(model: &mut Model, after: After) {
    let read = match after {
        After::Skills => Read::Skills,
        After::Sources => Read::SkillSources,
        After::Models => Read::Models,
        After::Notice => Read::Models,
        After::DiscoverPlugins => Read::Marketplaces,
        After::PluginsAt(_) => Read::Plugins,
        _ => Read::Plugins,
    };
    request(model, Effect::Read(read, after));
}
pub(super) fn resource_reply(model: &mut Model, resource: Resource, after: After) {
    match resource {
        Resource::Changed(notice) => {
            for message in notice.messages { model.push_notice(message); }
            match after {
                After::KeySaved(profile) => {
                    if (profile == model.active_profile || model.active_profile == "override")
                        && boundary_allowed(model)
                        && request(model, Effect::SavedProfile(protocol::ActivateSavedProfile { profile: profile.clone() }))
                    {
                        if let Some(setup) = &mut model.model_setup { setup.step = ModelSetupStep::Activating; }
                    } else {
                        model.model_setup = None;
                        model.push_notice(format!("Key saved for '{profile}'. Use /model to activate it after current work finishes."));
                    }
                },
                After::SearchSaved => { model.search_setup = None; request(model, Effect::Reload(After::Notice)); },
                After::Reload(next) => { request(model, Effect::Reload(*next)); },
                After::Models | After::Skills | After::Sources | After::Plugins | After::PluginsAt(_) | After::DiscoverPlugins => after_reload(model, after),
                _ => {},
            }
        },
        Resource::Models(catalogue) => { let rows = profiles(&catalogue); model.remote.as_mut().expect("viewer").models = catalogue;
            if matches!(after, After::Models) { let selected = rows.iter().position(|profile| profile.name == model.active_profile).unwrap_or_default();
                model.picker = Some(Picker::with_selected(PickerKind::Model { profiles: rows, active: model.active_profile.clone() }, selected)); } },
        Resource::ModelSaved { name, catalogue } => {
            model.remote.as_mut().expect("viewer").models = catalogue;
            if boundary_allowed(model) && request(model, Effect::SavedProfile(protocol::ActivateSavedProfile { profile: name.clone() })) {
                if let Some(setup) = &mut model.model_setup { setup.step = ModelSetupStep::Activating; }
            } else {
                model.model_setup = None;
                model.push_notice(format!("Model '{name}' is saved. Use /model to activate it when current work has finished."));
            }
        },
        Resource::Pulse(notice) => model.upsert_notice("pulse", notice.messages.join("\n")),
        Resource::Sessions(sessions) => { let sessions = sessions.into_iter().filter_map(|session| session.id.parse().ok().map(|id| kernel::SessionMeta { id, title: session.title.unwrap_or_default(),
            started_ts: session.started, last_ts: session.updated, events: session.events })).collect(); model.picker = Some(Picker::new(PickerKind::Session(sessions))); },
        Resource::Skills(catalogue) => {
            let text = catalogue.skills.iter().map(|skill| format!("{}  {} — {}{}", if skill.enabled && skill.available { "✔" } else { "○" }, skill.name, skill.description,
                if !skill.enabled { " (disabled)" } else if !skill.available { " (missing tools)" } else { "" })).chain(catalogue.errors.iter().map(|error| format!("! {error}"))).collect::<Vec<_>>().join("\n");
            let rows = catalogue.skills.iter().filter(|skill| skill.enabled).map(|skill| (skill.name.clone(), skill.description.clone())).collect();
            model.remote.as_mut().expect("viewer").skills = Some(catalogue);
            if matches!(after, After::SkillList) { model.upsert_notice("skills", format!("skills\n{text}")); }
            else { model.picker = Some(Picker::new(PickerKind::Skill(rows))); }
        },
        Resource::Skill(skill) => model.push_notice(format!("{} — {}\nscope: {} · version: {}\npath: {}\nrequired tools: {}\nmissing tools: {}\nsource: {}\nbundled files:\n{}",
            skill.name, skill.description, skill.scope, skill.version, skill.path.display(), skill.required_tools.join(", "), skill.missing_tools.join(", "),
            skill.source.unwrap_or_else(|| "local".into()), skill.bundled_files.join("\n"))),
        Resource::SkillSources(sources) => { let rows = sources.into_iter().map(|source| (source.repo, source.path, !source.built_in)).collect(); model.picker = Some(Picker::new(PickerKind::SkillSources(rows))); },
        Resource::SkillSearch(search) => { for error in search.errors { model.push_notice(format!("skill catalog: {error}")); }
            let rows = search.hits;
            model.picker = Some(Picker::new(PickerKind::SkillSearch(rows))); },
        Resource::SkillUpdates(updates) => { for update in updates { model.push_notice(format!("{}: {}{}", update.name, update.status, update.detail.map(|detail| format!(" — {detail}")).unwrap_or_default())); }
            prefill_command(model, "/skill update --all", "Enter to apply all available updates, or replace --all with a skill name."); },
        Resource::SkillLockfile { path, exists, entries } => model.push_notice(format!("{}: {} ({entries} skills)", path.display(), if exists { "saved" } else { "not created" })),
        Resource::SkillUsed { name, description, message: _ } => model.push_notice(format!("✔ loaded skill {name} — {description}")),
        Resource::SkillInstalled { name, enabled, findings } => { model.push_notice(format!("Installed {name}{}", if enabled { "" } else { " (disabled; review before enabling)" }));
            for finding in findings { model.push_notice(finding); } request(model, Effect::Reload(After::Skills)); },
        Resource::Plugins(catalogue) => backend_plugins::catalogue(model, catalogue, after),
        Resource::Marketplaces(markets) => backend_plugins::markets(model, markets),
        Resource::Hooks(hooks) => backend_plugins::hooks(model, hooks),
        Resource::PluginInstalled { id } => { request(model, Effect::Read(Read::Plugins, After::PluginEnable(id))); },
    }
}

pub(super) fn open_skill_picker(model: &mut Model) {
    request(model, Effect::Read(Read::Skills, After::Skills));
}
pub(super) fn load_skill_by_name(model: &mut Model, name: &str) {
    request(
        model,
        Effect::UseSkill(protocol::UseSkill { name: name.into() }),
    );
}
pub(super) fn search_skills(model: &mut Model, query: &str) {
    request(
        model,
        Effect::Read(
            Read::SkillSearch {
                query: query.into(),
            },
            After::Skills,
        ),
    );
}
pub(super) fn install_skill(model: &mut Model, source: &str) {
    request(
        model,
        Effect::Change(
            Change::InstallSkill {
                source: source.into(),
            },
            After::Reload(Box::new(After::Skills)),
        ),
    );
}
pub(super) fn add_skill(model: &mut Model, source: &str) {
    if looks_like_source(source) {
        install_skill(model, source);
    } else {
        search_skills(model, source);
    }
}
fn looks_like_source(source: &str) -> bool {
    source.starts_with("http")
        || source.starts_with('.')
        || source.starts_with('/')
        || source.starts_with('~')
        || source.contains('/')
        || source.contains('\\')
}
pub(super) fn show_skill_info(model: &mut Model, name: &str) {
    request(
        model,
        Effect::Read(Read::Skill { name: name.into() }, After::Notice),
    );
}
pub(super) fn begin_remove_skill(model: &mut Model, name: &str) {
    model.picker = Some(Picker::new(PickerKind::RemoveSkill(name.into())));
}
pub(super) fn remove_user_skill(model: &mut Model, name: &str) {
    request(
        model,
        Effect::Change(
            Change::RemoveSkill { name: name.into() },
            After::Reload(Box::new(After::Skills)),
        ),
    );
}
pub(super) fn open_sources_picker(model: &mut Model) {
    request(model, Effect::Read(Read::SkillSources, After::Sources));
}
pub(super) fn remove_source(model: &mut Model, key: &str) {
    request(
        model,
        Effect::Change(
            Change::RemoveSkillSource { key: key.into() },
            After::Sources,
        ),
    );
}
pub(super) fn skill_sources(model: &mut Model, args: &str) {
    if let Some(spec) = args.strip_prefix("add ") {
        let mut words = spec.split_whitespace();
        let spec = words.next().unwrap_or("").into();
        let path = words.next().map(Into::into);
        request(
            model,
            Effect::Change(Change::AddSkillSource { spec, path }, After::Sources),
        );
    } else if let Some(key) = args.strip_prefix("remove ") {
        remove_source(model, key.trim());
    } else {
        open_sources_picker(model);
    }
}
pub(super) fn update_skills(model: &mut Model, arg: &str) {
    if arg.is_empty() {
        request(
            model,
            Effect::Read(Read::SkillUpdates { name: None }, After::Notice),
        );
    } else {
        request(
            model,
            Effect::Change(
                Change::UpdateSkills {
                    name: (arg != "--all").then(|| arg.into()),
                },
                After::Reload(Box::new(After::Skills)),
            ),
        );
    }
}
pub(super) fn lock_skills(model: &mut Model) {
    request(model, Effect::Change(Change::LockSkills, After::Notice));
}
pub(super) fn sync_skills(model: &mut Model) {
    request(
        model,
        Effect::Change(Change::SyncSkills, After::Reload(Box::new(After::Skills))),
    );
}
pub(super) fn hub_notice(model: &mut Model, text: impl std::fmt::Display) {
    model.push_notice(text.to_string());
}
pub(super) fn prefill_command(model: &mut Model, command: &str, hint: &str) {
    if model.input.is_empty() {
        model.input = command.into();
        model.cursor = model.input.len();
    } else {
        model.push_notice(format!(
            "Suggested command: {command}\nYour existing draft is preserved."
        ));
    }
    model.push_notice(hint);
}

fn reasoning_panel(model: &mut Model) {
    let levels = model
        .remote
        .as_ref()
        .and_then(|peer| peer.settings.as_ref())
        .map(|settings| {
            settings
                .efforts
                .iter()
                .filter_map(|effort| backend_ui::effort(*effort))
                .collect()
        })
        .unwrap_or_default();
    model.picker = Some(Picker::new(PickerKind::Reasoning(ReasoningPanelState {
        enabled: model.reasoning.enabled,
        effort: model.reasoning.effort,
        show: model.show_thinking,
        support: model.reasoning_support,
        last_turn_received: model.last_turn_reasoning_received,
        choosing_effort: false,
        levels,
    })));
}
fn effort_wire(effort: Option<kernel::ReasoningEffort>) -> protocol::Effort {
    match effort {
        None => protocol::Effort::Auto,
        Some(kernel::ReasoningEffort::None) => protocol::Effort::None,
        Some(kernel::ReasoningEffort::Minimal) => protocol::Effort::Minimal,
        Some(kernel::ReasoningEffort::Low) => protocol::Effort::Low,
        Some(kernel::ReasoningEffort::Medium) => protocol::Effort::Medium,
        Some(kernel::ReasoningEffort::High) => protocol::Effort::High,
        Some(kernel::ReasoningEffort::XHigh) => protocol::Effort::XHigh,
        Some(kernel::ReasoningEffort::Max) => protocol::Effort::Max,
        Some(kernel::ReasoningEffort::Ultra) => protocol::Effort::Ultra,
    }
}
pub(super) fn handle_reasoning_picker_key(model: &mut Model, key: KeyEvent) -> bool {
    let Some(Picker {
        kind: PickerKind::Reasoning(state),
        selected,
    }) = &model.picker
    else {
        return false;
    };
    let (state, selected) = (state.clone(), *selected);
    let count = if state.choosing_effort {
        state.levels.len() + 1
    } else {
        3
    };
    let chosen = match key.code {
        KeyCode::Enter | KeyCode::Right => Some(selected),
        KeyCode::Char(c) if state.choosing_effort => c
            .to_digit(10)
            .and_then(|n| (n > 0 && n as usize <= count).then_some(n as usize - 1)),
        _ => None,
    };
    if let Some(chosen) = chosen {
        if state.choosing_effort {
            let effort = chosen
                .checked_sub(1)
                .and_then(|index| state.levels.get(index))
                .copied();
            request(
                model,
                Effect::Configure(protocol::Configure::Effort(effort_wire(effort))),
            );
        } else if chosen == 2 {
            let mut state = state;
            state.choosing_effort = true;
            let selected = state
                .effort
                .and_then(|effort| state.levels.iter().position(|level| *level == effort))
                .map_or(0, |index| index + 1);
            model.picker = Some(Picker::with_selected(
                PickerKind::Reasoning(state),
                selected,
            ));
        } else if chosen == 1 {
            model.show_thinking = !model.show_thinking;
            model.invalidate_all_renders();
            reasoning_panel(model);
            if let Some(picker) = &mut model.picker {
                picker.selected = 1;
            }
        } else {
            let next = match model.reasoning.enabled {
                None => protocol::Reasoning::On,
                Some(true) => protocol::Reasoning::Off,
                Some(false) => protocol::Reasoning::Auto,
            };
            request(
                model,
                Effect::Configure(protocol::Configure::Reasoning(next)),
            );
        }
    } else {
        match key.code {
            KeyCode::Up => {
                if let Some(picker) = &mut model.picker {
                    picker.selected = (selected + count - 1) % count;
                }
            }
            KeyCode::Down => {
                if let Some(picker) = &mut model.picker {
                    picker.selected = (selected + 1) % count;
                }
            }
            KeyCode::Home => {
                if let Some(picker) = &mut model.picker {
                    picker.selected = 0;
                }
            }
            KeyCode::End => {
                if let Some(picker) = &mut model.picker {
                    picker.selected = count - 1;
                }
            }
            KeyCode::Esc | KeyCode::Left if state.choosing_effort => reasoning_panel(model),
            KeyCode::Esc | KeyCode::Left => model.picker = None,
            _ => {}
        }
    }
    model.dirty = true;
    true
}
fn reasoning_command(model: &mut Model, args: &str) {
    match args {
        "" => reasoning_panel(model),
        "status" => model.push_notice(model.reasoning_status_block()),
        "show" | "hide" => {
            model.show_thinking = args == "show";
            model.invalidate_all_renders();
        }
        "on" | "off" | "auto" | "default" => {
            let choice = match args {
                "on" => protocol::Reasoning::On,
                "off" => protocol::Reasoning::Off,
                _ => protocol::Reasoning::Auto,
            };
            request(
                model,
                Effect::Configure(protocol::Configure::Reasoning(choice)),
            );
        }
        _ if args.starts_with("effort ") => {
            match kernel::ReasoningConfig::from_effort_text(args.trim_start_matches("effort ")) {
                Ok(choice) => {
                    request(
                        model,
                        Effect::Configure(protocol::Configure::Effort(effort_wire(choice.effort))),
                    );
                }
                Err(error) => model.push_notice(error),
            }
        }
        _ => model.push_notice("usage: /reasoning [on|off|auto|show|hide|status|effort LEVEL]"),
    }
}
fn local_command(model: &mut Model, command: &str) {
    let (name, args) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    let args = args.trim();
    match name {
        "reconnect" => { if boundary_allowed(model) && request(model, Effect::Reconnect) { model.session_op = Some(SessionOp::Opening); } },
        "exit" | "quit" => model.should_quit = true,
        "help" => model.push_notice(format!("{}\n\n/reconnect  Rejoin the conversation after a backend connection ends\nEsc interrupts · Ctrl-D quits · Tab switches agents · ↑/↓ scroll or recall input · mouse selects and copies",
            COMMANDS.iter().map(|(name, description)| format!("{name}  {description}")).collect::<Vec<_>>().join("\n"))),
        "reasoning" | "think" => reasoning_command(model, args), "effort" => reasoning_command(model, &format!("effort {args}")),
        "thinking" => { model.show_thinking = !model.show_thinking; model.invalidate_all_renders(); },
        "detail" => { model.full_transparency = !model.full_transparency; model.invalidate_all_renders(); },
        "stream" => { let on = match args { "" => !model.streaming, "on" => true, "off" => false, "status" => { model.push_notice(format!("streaming: {}", model.streaming)); return; },
            _ => { model.push_notice("usage: /stream [on|off|status]"); return; } }; request(model, Effect::Configure(protocol::Configure::Streaming(on))); },
        "theme" => input::apply_theme_command(model, args),
        "tasks" => { request(model, Effect::Inspect(Inspect::Live, After::Tasks)); },
        "skills" => { request(model, Effect::Read(Read::Skills, After::SkillList)); },
        "pulse" if args == "fix" => { request(model, Effect::Change(Change::PulseFix, After::Notice)); },
        "pulse" => { request(model, Effect::Read(Read::Pulse, After::Notice)); },
        "status" => { let settings = model.remote.as_ref().and_then(|peer| peer.settings.as_ref());
            model.push_notice(format!("model: {} ({}) · {}\nimage input: {} ({})\n{}\ncache: {:?}; unknown cache reports: {}",
                model.model, model.active_profile, model.protocol.as_str(), settings.and_then(|settings| settings.image_input.as_deref()).unwrap_or("unknown"),
                settings.and_then(|settings| settings.image_support.as_deref()).unwrap_or("unknown"), model.reasoning_status_block(), model.cache, model.cache_unreported_attempts)); },
        _ => model.push_notice(format!("unknown command: /{command}")),
    }
}

fn memory_command(model: &mut Model, name: &str) {
    if !model.memory_enabled {
        model.push_notice("memory: unavailable");
        return;
    }
    if name.is_empty() {
        request(model, Effect::Inspect(Inspect::Memories, After::Memory));
    } else {
        request(
            model,
            Effect::Inspect(Inspect::Memories, After::AgentTranscript(name.into())),
        );
    }
}
pub(super) fn spawn_memory_provenance(model: &mut Model, entry: memory::MemoryEntry) {
    request(
        model,
        Effect::Inspect(
            Inspect::MemoryEvidence {
                scope: memory_scope(entry.scope),
                name: entry.name,
            },
            After::Notice,
        ),
    );
}
fn memory_scope(scope: memory::Scope) -> protocol::ResourceScope {
    match scope {
        memory::Scope::Project => protocol::ResourceScope::Project,
        memory::Scope::User => protocol::ResourceScope::User,
    }
}
pub(super) fn handle_memory_picker_key(model: &mut Model, key: &KeyEvent) -> bool {
    let Some(Picker {
        kind: PickerKind::Memory(entries),
        selected,
    }) = &model.picker
    else {
        return false;
    };
    let Some(entry) = entries.get(*selected) else {
        return false;
    };
    let action = match key.code {
        KeyCode::Char('p') => protocol::SessionChange::PinMemory {
            scope: memory_scope(entry.scope),
            name: entry.name.clone(),
            pinned: !entry.pinned,
        },
        KeyCode::Char('f') => protocol::SessionChange::ForgetMemory {
            scope: memory_scope(entry.scope),
            name: entry.name.clone(),
        },
        _ => return false,
    };
    request(model, Effect::SessionChange(action, After::Memory));
    true
}
pub(super) fn session_reply(model: &mut Model, reply: protocol::SessionResult, after: After) {
    match reply {
        protocol::SessionResult::Bootstrap(_) => {}
        protocol::SessionResult::Text(text) => model.push_notice(text),
        protocol::SessionResult::Lsp(value) => model.push_notice(lsp_inventory_line(&value)),
        protocol::SessionResult::Live(live) => {
            refresh_live(model, live);
            match after {
            After::Agents => { request(model, Effect::Inspect(Inspect::AgentPatches, After::Agents)); },
            After::Tasks => model.upsert_notice("shell tasks", format!("shell tasks:\n{}\ninspect with task.output; stop with task.control op=kill", model.bg_tasks.iter().map(|task| format!("{} [{}] {}", task.id, if task.running { "running" } else { "done" }, task.command)).collect::<Vec<_>>().join("\n"))),
            _ => {},
        }
        }
        protocol::SessionResult::Memories(entries) => {
            let entries = entries
                .into_iter()
                .map(|entry| serde_json::to_value(entry).and_then(serde_json::from_value))
                .collect::<Result<Vec<memory::MemoryEntry>, _>>();
            let entries = match entries {
                Ok(entries) => entries,
                Err(error) => {
                    model.push_notice(format!("The backend returned invalid memory data: {error}"));
                    return;
                }
            };
            if let After::AgentTranscript(name) = after {
                match entries.into_iter().find(|entry| entry.name == name) {
                    Some(entry) => spawn_memory_provenance(model, entry),
                    None => model.push_notice(format!("Memory '{name}' not found.")),
                }
            } else {
                model.picker = Some(Picker::new(PickerKind::Memory(entries)));
            }
        }
        protocol::SessionResult::MemoryEvidence(evidence) => model.push_notice(format!(
            "{} — {}\n{}\ntrust: {} · confidence: {} · pinned: {}\nprovenance: {}\n{}",
            evidence.entry.name,
            evidence.entry.description,
            evidence.entry.claim,
            evidence.entry.trust,
            evidence.entry.confidence,
            evidence.entry.pinned,
            evidence.entry.provenance.join(", "),
            evidence
                .excerpt
                .unwrap_or_else(|| "No retained evidence excerpt.".into())
        )),
        protocol::SessionResult::Transcript { items, omitted } => {
            if let After::AgentTranscript(session) = after
                && let Some(path) = model
                    .remote
                    .as_ref()
                    .and_then(|peer| {
                        peer.live
                            .agents
                            .iter()
                            .find(|agent| agent.session == session)
                    })
                    .and_then(|agent| orchestrator::AgentPath::parse(&agent.path).ok())
            {
                let mut rows: VecDeque<_> = items
                    .into_iter()
                    .map(|item| Entry::new(backend_ui::item(item)))
                    .collect();
                if omitted > 0 {
                    rows.push_front(Entry::new(Item::Notice(format!(
                        "{omitted} older entries remain in saved history."
                    ))));
                }
                if model.focus.as_ref() == Some(&path) {
                    model.items = rows;
                } else {
                    model.agent_panes.insert(path.clone(), rows);
                }
                model.focus_pane(Some(path));
                model.picker = None;
                model.invalidate_all_renders();
            }
        }
        protocol::SessionResult::Patches(patches) => {
            if let After::Patch(id) = &after {
                match patches.iter().find(|patch| &patch.id == id) { Some(patch) => model.push_notice(format!("{} · {} file(s)\n{}\nverification: {}\nApply with a; A explicitly overrides failed verification.",
                patch.agent, patch.files.len(), patch.diff, patch.verification.as_ref().map(|verification| if verification.passed { "passed" } else { "failed" }).unwrap_or("not configured"))), None => model.push_notice("That exact patch is no longer pending.") }
            }
            model.remote.as_mut().expect("viewer").patches = patches;
            if matches!(after, After::Agents) {
                show_agents(model);
            }
        }
        protocol::SessionResult::Changed => match after {
            After::Memory => {
                request(model, Effect::Inspect(Inspect::Memories, After::Memory));
            }
            After::Agents => open_agents_picker(model),
            _ => model.push_notice("Change saved."),
        },
    }
}

fn projected_agent(state: &protocol::AgentState) -> Option<orchestrator::Agent> {
    let path = orchestrator::AgentPath::parse(&state.path).ok()?;
    let status = state.status.as_ref().map(|status| match status {
        protocol::AgentStatus::Completed => orchestrator::AgentStatus::Completed,
        protocol::AgentStatus::Exhausted => orchestrator::AgentStatus::Exhausted,
        protocol::AgentStatus::Failed => orchestrator::AgentStatus::Failed,
        protocol::AgentStatus::Cancelled => orchestrator::AgentStatus::Cancelled,
    });
    Some(orchestrator::Agent {
        path,
        session: state.session.clone(),
        objective: state.objective.clone(),
        started_ms: state.started_ms,
        state: status
            .map(orchestrator::State::Settled)
            .unwrap_or(orchestrator::State::Running),
        write: state.write,
        tools: None,
    })
}
pub(super) fn refresh_roster(model: &mut Model, roster: Vec<protocol::RosterAgent>) {
    let peer = model.remote.as_ref().expect("viewer");
    let live = protocol::LiveState {
        tasks: peer.live.tasks.clone(),
        pending_patches: peer.live.pending_patches,
        agents: roster
            .into_iter()
            .map(|agent| protocol::AgentState {
                path: agent.path,
                session: agent.session,
                objective: agent.objective,
                started_ms: agent.started_ms,
                write: agent.write,
                status: serde_json::from_value(serde_json::Value::String(agent.status)).ok(),
                phase: agent.doing.and_then(|phase| match phase {
                    protocol::AgentDoing::Thinking => Some(protocol::AgentPhase::Generating),
                    protocol::AgentDoing::Tool { tool, target, .. } => {
                        tool.map(|tool| protocol::AgentPhase::InTool { tool, target })
                    }
                    protocol::AgentDoing::Waiting { action } => {
                        Some(protocol::AgentPhase::AwaitingApproval { action })
                    }
                    protocol::AgentDoing::Idle => Some(protocol::AgentPhase::Idle),
                    protocol::AgentDoing::Finished => Some(protocol::AgentPhase::Settled),
                }),
                tool_calls: agent.tool_calls.unwrap_or(0),
                tokens: agent.tokens.unwrap_or(0),
            })
            .collect(),
    };
    // A task step can precede the next roster notification. Only the direct
    // live-state query may prune those panes against the actual registry.
    project_live(model, live, false);
}

fn refresh_live(model: &mut Model, live: protocol::LiveState) {
    project_live(model, live, true);
}

fn project_live(model: &mut Model, live: protocol::LiveState, prune: bool) {
    let selected = model.switch_selection();
    let previous = std::mem::take(&mut model.agent_runs);
    model.bg_tasks = live
        .tasks
        .iter()
        .map(|task| kernel::BackgroundTask {
            id: task.id.clone(),
            command: task.command.clone(),
            running: task.running,
        })
        .collect();
    let roster: Vec<_> = live.agents.iter().filter_map(projected_agent).collect();
    for state in &live.agents {
        let Ok(path) = orchestrator::AgentPath::parse(&state.path) else {
            continue;
        };
        if let Some(phase) = &state.phase {
            let phase = match phase {
                protocol::AgentPhase::Generating => kernel::Phase::Generating,
                protocol::AgentPhase::InTool { tool, target } => kernel::Phase::InTool {
                    tool: tool.clone(),
                    target: target.clone(),
                },
                protocol::AgentPhase::AwaitingApproval { action } => {
                    kernel::Phase::AwaitingApproval {
                        action: action.clone(),
                    }
                }
                protocol::AgentPhase::Idle => kernel::Phase::Idle,
                protocol::AgentPhase::Settled => kernel::Phase::Settled,
            };
            let since = model
                .agent_progress
                .get(&path)
                .filter(|progress| progress.phase == phase)
                .map(|progress| progress.since)
                .unwrap_or_else(Instant::now);
            model.agent_progress.insert(
                path,
                kernel::Progress {
                    phase,
                    since,
                    tool_calls: state.tool_calls.min(u32::MAX as u64) as u32,
                    tokens: state.tokens,
                    ..Default::default()
                },
            );
        }
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_millis() as u64);
    for old in previous {
        if let Some(agent) = roster.iter().find(|agent| agent.session == old.session)
            && let orchestrator::State::Settled(status) = agent.state
        {
            let progress = model.agent_progress.get(&agent.path);
            model.agents_done.push(AgentDoneRow {
                name: agent.path.name().into(),
                status,
                tool_calls: progress.map_or(0, |p| p.tool_calls),
                tokens: progress.map_or(0, |p| p.tokens),
                seconds: now_ms.saturating_sub(agent.started_ms) / 1000,
            });
        }
    }
    model.agent_runs = roster
        .iter()
        .filter(|agent| agent.is_running())
        .cloned()
        .collect();
    if model.agent_runs.is_empty() && !model.agents_done.is_empty() {
        let rows = std::mem::take(&mut model.agents_done);
        model.push_main_item(Item::AgentsDone(rows));
    }
    let known = roster.into_iter().map(|agent| agent.path).collect();
    if prune {
        model.retain_known_agent_panes(&known);
        model.agent_progress.retain(|path, _| known.contains(path));
    }
    model.trim_agent_views();
    model.reconcile_switch_cursor(selected);
    model.remote.as_mut().expect("viewer").live = live;
}
fn show_agents(model: &mut Model) {
    let peer = model.remote.as_ref().expect("viewer");
    let mut rows = branched(
        peer.live
            .agents
            .iter()
            .filter_map(projected_agent)
            .filter(orchestrator::Agent::is_running)
            .collect(),
        &model.agent_progress,
    );
    rows.extend(
        peer.patches
            .iter()
            .map(|patch| AgentRow::Patch {
                agent: patch.agent.clone(),
                dispatch: patch.id.clone(),
                files: patch.files.len(),
                verified: patch
                    .verification
                    .as_ref()
                    .map(|verification| verification.passed),
            })
            .collect::<Vec<_>>(),
    );
    rows.extend(
        peer.live
            .agents
            .iter()
            .filter_map(projected_agent)
            .filter(|agent| {
                !agent.is_running()
                    && !peer
                        .patches
                        .iter()
                        .any(|patch| patch.session == agent.session)
            })
            .rev()
            .map(|agent| AgentRow::Agent {
                progress: model.agent_progress.get(&agent.path).cloned(),
                agent,
                branch: String::new(),
            }),
    );
    model.picker = Some(Picker::new(PickerKind::Agents(rows)));
}
pub(super) fn open_agents_picker(model: &mut Model) {
    request(model, Effect::Inspect(Inspect::Live, After::Agents));
}
pub(super) fn branched(
    mut agents: Vec<orchestrator::Agent>,
    progress: &std::collections::HashMap<orchestrator::AgentPath, kernel::Progress>,
) -> Vec<AgentRow> {
    agents.sort_by(|a, b| a.path.cmp(&b.path));
    let depths: Vec<u32> = agents.iter().map(|agent| agent.path.depth()).collect();
    agents
        .iter()
        .enumerate()
        .map(|(index, agent)| {
            let depth = depths[index];
            // The last child of a parent is whichever has no later sibling
            // before the tree pops back above this depth.
            let has_sibling_below = depths[index + 1..]
                .iter()
                .take_while(|below| **below >= depth)
                .any(|below| *below == depth);
            let branch = match depth {
                0 | 1 => String::new(),
                _ => format!(
                    "{}{} ",
                    "  ".repeat(depth as usize - 2),
                    if has_sibling_below { "├" } else { "└" }
                ),
            };
            AgentRow::Agent {
                progress: progress.get(&agent.path).cloned(),
                agent: agent.clone(),
                branch,
            }
        })
        .collect()
}

pub(super) fn agents_view_patch(model: &mut Model, id: &str) {
    request(
        model,
        Effect::Inspect(Inspect::AgentPatches, After::Patch(id.into())),
    );
}
pub(super) fn agents_apply_patch(model: &mut Model, id: &str, override_verification: bool) {
    request(
        model,
        Effect::SessionChange(
            protocol::SessionChange::ApplyPatch {
                id: id.into(),
                override_verification,
            },
            After::Agents,
        ),
    );
}
pub(super) fn agents_view_transcript(model: &mut Model, session: &str) {
    request(
        model,
        Effect::Inspect(
            Inspect::AgentTranscript {
                session: session.into(),
            },
            After::AgentTranscript(session.into()),
        ),
    );
}
pub(super) fn agents_stop(model: &mut Model, agent: &str) {
    request(
        model,
        Effect::Agent(
            protocol::AgentCommand::Stop {
                agent: agent.into(),
            },
            None,
        ),
    );
}
pub(super) fn agents_stop_path(model: &mut Model, path: &orchestrator::AgentPath) {
    agents_stop(model, &path.to_string());
}
pub(super) fn stop_every_agent(model: &mut Model) {
    request(
        model,
        Effect::SessionChange(protocol::SessionChange::StopAgents, After::Agents),
    );
}
pub(super) fn show_agent_tree(model: &mut Model) {
    let agents = model
        .remote
        .as_ref()
        .map(|peer| {
            peer.live
                .agents
                .iter()
                .map(|agent| {
                    format!(
                        "{} [{}] {}",
                        agent.path,
                        if agent.status.is_none() {
                            "running"
                        } else {
                            "settled"
                        },
                        agent.objective
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    model.push_notice(format!("agents:\n{agents}"));
}
pub(super) fn agents_steer(model: &mut Model, args: &str) {
    let running: Vec<_> = model
        .agent_runs
        .iter()
        .filter(|agent| agent.is_running())
        .map(|agent| (agent.path.to_string(), agent.session.clone()))
        .collect();
    match steer_target(args, &running) {
        Ok((agent, text)) => {
            request(
                model,
                Effect::Agent(
                    protocol::AgentCommand::Steer { agent, text },
                    Some(format!("/agents steer {args}")),
                ),
            );
        }
        Err(error) => model.push_notice(error),
    }
}
pub(super) fn agents_followup(model: &mut Model, args: &str) {
    match args
        .split_once(char::is_whitespace)
        .filter(|(_, text)| !text.trim().is_empty())
    {
        Some((agent, text)) => {
            request(
                model,
                Effect::Agent(
                    protocol::AgentCommand::Followup {
                        agent: agent.into(),
                        text: text.trim().into(),
                    },
                    Some(format!("/agents followup {args}")),
                ),
            );
        }
        None => model.push_notice("usage: /agents followup <agent> <message>"),
    }
}

pub(super) fn open_mcp_picker(model: &mut Model) {
    request(model, Effect::Mcp(protocol::McpCommand::List, After::Mcp));
}
pub(super) fn start_mcp_server(model: &mut Model, id: &str) {
    let hash = model
        .remote
        .as_ref()
        .and_then(|peer| peer.mcp.iter().find(|server| server.id == id))
        .map(|server| server.hash.clone());
    if let Some(hash) = hash {
        request(
            model,
            Effect::Mcp(
                protocol::McpCommand::Connect {
                    id: id.into(),
                    hash,
                },
                After::Mcp,
            ),
        );
    } else {
        model.push_notice("Open /mcp and review this server before connecting it.");
        open_mcp_picker(model);
    }
}
pub(super) fn mcp_add(model: &mut Model, definition: &str) {
    request(
        model,
        Effect::Mcp(
            protocol::McpCommand::Add {
                definition: protocol::Secret(definition.into()),
            },
            After::Mcp,
        ),
    );
}
pub(super) fn mcp_set_disabled(model: &mut Model, id: &str, disabled: bool) {
    request(
        model,
        Effect::Mcp(
            protocol::McpCommand::Disable {
                id: id.into(),
                disabled,
            },
            After::Mcp,
        ),
    );
}
pub(super) fn mcp_set_tool(model: &mut Model, id: &str, tool: &str, exposed: bool) {
    request(
        model,
        Effect::Mcp(
            protocol::McpCommand::Expose {
                id: id.into(),
                tool: tool.into(),
                exposed,
            },
            After::Mcp,
        ),
    );
}
pub(super) fn open_mcp_tools(model: &mut Model, id: &str) {
    request(
        model,
        Effect::Mcp(protocol::McpCommand::Tools { id: id.into() }, After::Mcp),
    );
}
pub(super) fn mcp_remove(model: &mut Model, id: &str) {
    request(
        model,
        Effect::Mcp(protocol::McpCommand::Remove { id: id.into() }, After::Mcp),
    );
}
pub(super) fn mcp_choose_auth(model: &mut Model, id: &str, url: &str, choice: usize) {
    match choice {
        1 => {
            model.form_generation = model.form_generation.wrapping_add(1);
            model.model_setup = None;
            model.search_setup = None;
            model.mcp_credential = Some(McpCredential {
                id: id.into(),
                saving: false,
                editor: Editor::default(),
            });
            model.push_notice(format!(
                "API token for '{id}' ({url}); input is masked and stored by the backend."
            ));
        }
        _ => {
            request(
                model,
                Effect::Mcp(
                    protocol::McpCommand::Authenticate {
                        id: id.into(),
                        oauth: choice == 0,
                    },
                    After::Mcp,
                ),
            );
        }
    }
}
pub(super) fn handle_mcp_credential_key(model: &mut Model, key: KeyEvent) -> bool {
    let Some(form) = model.mcp_credential.as_ref() else {
        return false;
    };
    if key.code == KeyCode::Esc {
        model.form_generation = model.form_generation.wrapping_add(1);
        model.mcp_credential = None;
        return true;
    }
    if form.saving {
        return true;
    }
    match key.code {
        KeyCode::Enter if !form.editor.text.trim().is_empty() => {
            let command = protocol::McpCommand::Credential {
                id: form.id.clone(),
                key: protocol::Secret(form.editor.text.trim().into()),
            };
            if request(model, Effect::Mcp(command, After::Mcp)) {
                model.mcp_credential.as_mut().expect("form").saving = true;
            }
        }
        KeyCode::Backspace => model.backspace(),
        KeyCode::Left => model.move_left(),
        KeyCode::Right => model.move_right(),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => model.insert_char(c),
        _ => {}
    }
    true
}
pub(super) fn connect_command(model: &mut Model, query: &str) {
    request(
        model,
        Effect::Mcp(
            protocol::McpCommand::Connectors {
                query: query.into(),
            },
            After::Mcp,
        ),
    );
}
pub(super) fn connect_connector(model: &mut Model, id: &str) {
    request(
        model,
        Effect::Mcp(
            protocol::McpCommand::ConnectConnector { id: id.into() },
            After::Mcp,
        ),
    );
}
pub(super) fn search_mcp_catalog(model: &mut Model, query: &str) {
    request(
        model,
        Effect::Mcp(
            protocol::McpCommand::Catalogue {
                query: query.into(),
            },
            After::Mcp,
        ),
    );
}
pub(super) fn mcp_reply(model: &mut Model, reply: protocol::McpResult, after: After) {
    match reply {
        protocol::McpResult::Servers(servers) => {
            let rows = servers
                .iter()
                .map(|server| McpRow {
                    id: server.id.clone(),
                    command: server
                        .url
                        .clone()
                        .or_else(|| server.command.clone())
                        .unwrap_or_default(),
                    disabled: server.disabled,
                })
                .collect();
            model.remote.as_mut().expect("viewer").mcp = servers;
            model.picker = Some(Picker::new(PickerKind::Mcp(rows)));
        }
        protocol::McpResult::Tools { id, tools } => {
            model.picker = Some(Picker::new(PickerKind::McpTools {
                id,
                tools: tools
                    .into_iter()
                    .map(|tool| (tool.name, tool.exposed))
                    .collect(),
            }))
        }
        protocol::McpResult::Catalogue(picks) => {
            model.picker = Some(Picker::new(PickerKind::McpCatalog(
                picks
                    .into_iter()
                    .map(|pick| CatalogPick {
                        label: pick.label,
                        line: pick.command,
                        cursor: pick.cursor,
                    })
                    .collect(),
            )))
        }
        protocol::McpResult::Connectors(picks) => {
            if picks.len() == 1 {
                connect_connector(model, &picks[0].id);
            } else {
                model.picker = Some(Picker::new(PickerKind::Connectors(
                    picks
                        .into_iter()
                        .map(|pick| ConnectorPick {
                            id: pick.id,
                            label: format!(
                                "{} — {}{}",
                                pick.name,
                                pick.description,
                                if pick.configured { " (configured)" } else { "" }
                            ),
                        })
                        .collect(),
                )));
            }
        }
        protocol::McpResult::Authorization { id, url } => {
            model.push_notice(format!("Sign in to '{id}' in the browser:\n{url}"))
        }
        protocol::McpResult::Status(value) => {
            if value["state"] == "needs_token" {
                if let Some(id) = value["server"].as_str() {
                    let url = model
                        .remote
                        .as_ref()
                        .and_then(|peer| peer.mcp.iter().find(|server| server.id == id))
                        .and_then(|server| server.url.clone())
                        .unwrap_or_default();
                    model.picker = Some(Picker::new(PickerKind::McpAuth { id: id.into(), url }));
                }
            } else {
                model.push_notice(value.to_string());
                if matches!(after, After::Mcp) {
                    open_mcp_picker(model);
                }
            }
        }
        protocol::McpResult::Changed => {
            if matches!(
                model.picker.as_ref().map(|picker| &picker.kind),
                Some(PickerKind::McpTools { .. })
            ) {
                if let Some(Picker {
                    kind: PickerKind::McpTools { id, .. },
                    ..
                }) = &model.picker
                {
                    let id = id.clone();
                    open_mcp_tools(model, &id);
                }
            } else {
                open_mcp_picker(model);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_token_never_reaches_command_history() {
        let recalled = redact_secrets("/mcp add gh https://x/mcp --bearer sk-live-abc123");
        assert!(!recalled.contains("sk-live-abc123"), "{recalled}");
        assert_eq!(
            recalled, "/mcp add gh https://x/mcp --bearer <redacted>",
            "the command stays recallable, only the value goes"
        );
        assert_eq!(redact_secrets("/mcp list"), "/mcp list");
        assert_eq!(redact_secrets("/mcp add gh --key"), "/mcp add gh --key");
        // `--env NAME=VALUE` is how an MCP server is handed a token.
        let env = redact_secrets("/mcp add gh --env GITHUB_TOKEN=ghp_live -- npx server");
        assert!(!env.contains("ghp_live"), "{env}");
        assert_eq!(
            env, "/mcp add gh --env GITHUB_TOKEN=<redacted> -- npx server",
            "the variable name stays readable, the value does not"
        );
        let joined = redact_secrets("/mcp add gh --env=GITHUB_TOKEN=ghp_live");
        assert!(!joined.contains("ghp_live"), "{joined}");
        assert_eq!(
            redact_secrets("/mcp add gh --bearer=sk-live-abc"),
            "/mcp add gh --bearer=<redacted>"
        );
    }

    #[test]
    fn add_auto_detects_link_or_path_vs_search_term() {
        assert!(looks_like_source(
            "https://github.com/anthropics/skills/tree/main/skills/pdf"
        ));
        assert!(looks_like_source("github.com/owner/repo"));
        assert!(looks_like_source("/tmp/my-skill"));
        assert!(looks_like_source("~/skills/foo"));
        assert!(looks_like_source("./local"));
        assert!(!looks_like_source("pdf"));
        assert!(!looks_like_source("excel spreadsheet"));
    }
    fn one() -> Vec<(String, String)> {
        vec![("tokio-audit".into(), "01SESSION".into())]
    }
    fn two() -> Vec<(String, String)> {
        vec![
            ("tokio-audit".into(), "01AAA".into()),
            ("lexer-survey".into(), "01BBB".into()),
        ]
    }

    #[test]
    fn a_bare_agent_name_is_a_half_typed_command_not_a_message() {
        assert!(steer_target("tokio-audit", &one()).is_err());
        assert!(steer_target("01SESSION", &one()).is_err());
        assert!(steer_target("tokio-audit   ", &one()).is_err());
    }

    #[test]
    fn a_named_agent_takes_the_rest_as_the_message() {
        let (id, text) = steer_target("tokio-audit skip the tests", &two()).unwrap();
        assert_eq!(id, "tokio-audit");
        assert_eq!(text, "skip the tests");
    }

    #[test]
    fn with_one_agent_running_the_whole_line_is_the_message() {
        let (id, text) = steer_target("skip the tests", &one()).unwrap();
        assert_eq!(id, "01SESSION", "addressed by session, not guessed by name");
        assert_eq!(text, "skip the tests");
    }

    #[test]
    fn an_unnamed_message_with_several_running_asks_which() {
        let refused = steer_target("skip the tests", &two()).unwrap_err();
        assert!(refused.contains("say which"), "{refused}");
        assert!(refused.contains("tokio-audit"), "names the candidates");
    }

    #[test]
    fn an_empty_message_reaches_nobody() {
        assert!(steer_target("", &one()).is_err());
        assert!(steer_target("tokio-audit    ", &two()).is_err());
    }
}
