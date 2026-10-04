//! Builds one chat on an opened workspace: its sandbox, tools, agents, context and kernel.

use crate::approvals::{approve_list_from, unknown_approvals};
use crate::model::Model;
use crate::verify::CommandVerifier;
use crate::{
    Notices, Resume, SessionOptions, Surface, Workspace, agents, attachments, config,
    plugin_session, skill_judge, vision,
};
use anyhow::{Context, Result};
use kernel::{EventLog, Kernel, Message, Session};
use providers::OpenAiCompat;
use sandbox::WorkspaceSandbox;
use std::path::Path;
use std::sync::Arc;
use tools::ToolRegistry;

pub struct Start<'a> {
    pub lock: &'a lockfile::MedhaLock,
    pub options: &'a SessionOptions,
    pub workspace: &'a Workspace,
    pub model: Model,
    pub autonomy: kernel::AutonomyLevel,
    pub verify_command: Option<String>,
    pub verify_required: bool,
    pub verify_timeout: std::time::Duration,
    pub notices: &'a dyn Notices,
}

/// What a chat's options and its folder's lock settle between them, before a model is chosen.
pub struct Choices {
    pub autonomy: kernel::AutonomyLevel,
    pub verify_command: Option<String>,
    pub verify_required: bool,
    pub verify_timeout: std::time::Duration,
}

impl Choices {
    pub fn of(lock: &lockfile::MedhaLock, options: &SessionOptions) -> Result<Self> {
        let autonomy = match options.autonomy {
            Some(mode) => mode,
            None => {
                kernel::AutonomyLevel::parse(&lock.policy.autonomy).map_err(anyhow::Error::msg)?
            }
        };
        let verify_command = options
            .verify_command
            .clone()
            .or_else(|| lock.verify.command.clone());
        let verify_required = options.require_verify || lock.verify.required;
        if verify_required && verify_command.is_none() {
            anyhow::bail!(
                "required verification needs [verify].command in medha.lock or MEDHA_VERIFY"
            );
        }
        let verify_timeout = std::time::Duration::from_secs(
            lock.verify
                .timeout_s
                .unwrap_or(lock.agents.verify_timeout_secs),
        );
        Ok(Self {
            autonomy,
            verify_command,
            verify_required,
            verify_timeout,
        })
    }
}

/// `surface` is asked once the prompt is settled, because a surface may depend on it.
pub async fn start(
    input: Start<'_>,
    surface: impl FnOnce(&Path, &str) -> Surface,
) -> Result<Started> {
    let Start {
        lock,
        options,
        workspace: workspace_home,
        model,
        autonomy,
        verify_command: verify_cmd,
        verify_required,
        verify_timeout,
        notices,
    } = input;
    let Model {
        provider,
        name: model_name,
        profiles: model_profiles,
        active_profile,
        max_ctx,
        open_setup,
    } = model;
    let prompt = options.prompt.clone();
    let cwd = workspace_home.root.clone();
    let state = workspace_home.state.clone();
    let medha_home = workspace_home.home.clone();
    let logs_dir = state.join("logs");
    let crate::workspace::Store { log, artifacts } = workspace_home.open_store()?;

    // A model with no image input is not a dead end when an auxiliary vision
    // profile is configured: the kernel has it describe the image instead.
    let configured_vision = {
        let configured = model_profiles.lock().unwrap().clone();
        configured
            .auxiliary
            .vision
            .clone()
            .filter(|name| !name.trim().is_empty())
            .map(|name| (name, configured))
    };
    let auxiliary_vision = configured_vision.and_then(|(name, configured)| {
        match vision::connect(&configured, &name, options.model_env.api_key.as_deref()) {
            Ok(provider) => {
                notices.say(&format!(
                    "auxiliary vision: '{name}' reads images this model cannot"
                ));
                Some(Arc::new(vision::AuxiliaryVision::new(provider)))
            }
            Err(error) => {
                notices.say(&format!(
                    "auxiliary vision profile '{name}' unavailable: {error:#}"
                ));
                None
            }
        }
    });

    // `--attach` is explicit; a path written into the prompt attaches too, so
    // `medha "explain ./shot.png"` behaves the way it reads.
    let mut attach_paths = options.attach.clone();
    let scanned = attachments::refs::scan(&prompt, &cwd);
    for path in &scanned {
        if !attach_paths.contains(path) {
            attach_paths.push(path.clone());
        }
    }
    // An unreachable path carries no meaning once its image is attached.
    let prompt = match attachments::refs::strip_unreachable(&prompt, &scanned, &cwd) {
        stripped if stripped.trim().is_empty() && !attach_paths.is_empty() => {
            attachments::IMAGE_ONLY_PROMPT.to_string()
        }
        stripped => stripped,
    };
    let attached_images: Vec<_> = attachments::ingest(attach_paths, artifacts.clone())
        .await?
        .into_iter()
        .inspect(|image| {
            notices.say(&format!(
                "attached {}{}",
                image.summary(),
                image
                    .note
                    .as_deref()
                    .map(|note| format!(" ({note})"))
                    .unwrap_or_default()
            ))
        })
        .map(|image| image.part)
        .collect();

    let surface = surface(&cwd, &prompt);
    let has_task = !options.first_run_setup && !prompt.trim().is_empty();
    let lock_path = cwd.join("medha.lock");
    // Machine-local grants never inherit authority from repository config.
    let trust_path = state.join("trust.lock");
    if !lock.permissions.trusted_paths.is_empty() {
        notices.say(&format!(
            "warning: ignoring {} repository-provided permission grant(s) in {}; \
             out-of-workspace paths require explicit path-specific approval",
            lock.permissions.trusted_paths.len(),
            lock_path.display()
        ));
    }
    let audit_path = logs_dir.join("audit.log");
    let legacy_audit = cwd.join("medha_audit.log");
    if legacy_audit.exists() && !audit_path.exists() {
        std::fs::rename(&legacy_audit, &audit_path).ok();
    }
    let mut sbx_cfg = lock.sandbox.to_config();
    // One per chat: its commands, language servers, local MCP servers and hooks.
    sbx_cfg.home = sandbox::HomeScope::private();
    if let Some(backend) = options.sandbox {
        sbx_cfg.backend = backend;
    }
    // Misconfigured optional backends fall back to native isolation.
    match sbx_cfg.backend {
        sandbox::BackendKind::Container => {
            let runtime = sbx_cfg.runtime.clone().unwrap_or_else(|| "docker".into());
            if sbx_cfg.image.as_deref().unwrap_or("").is_empty() {
                notices.say(
                    "warning: [sandbox] backend=container needs an `image` — falling back to the native jail.",
                );
                sbx_cfg.backend = sandbox::BackendKind::Native;
            } else if !sandbox::program_on_path(&runtime) {
                notices.say(&format!(
                    "warning: container runtime '{runtime}' not found on PATH — falling back to the native jail."
                ));
                sbx_cfg.backend = sandbox::BackendKind::Native;
            }
        }
        sandbox::BackendKind::Ssh => {
            if sbx_cfg.host.as_deref().unwrap_or("").is_empty() {
                notices.say(
                    "warning: [sandbox] backend=ssh needs a `host` — falling back to the native jail.",
                );
                sbx_cfg.backend = sandbox::BackendKind::Native;
            } else if !sandbox::program_on_path("ssh") {
                notices.say("warning: `ssh` not found on PATH — falling back to the native jail.");
                sbx_cfg.backend = sandbox::BackendKind::Native;
            }
        }
        _ => {}
    }
    let jail = match sbx_cfg.backend {
        sandbox::BackendKind::Container => policy::Jail::Active,
        sandbox::BackendKind::Native if sandbox::native_backend_available() => policy::Jail::Active,
        sandbox::BackendKind::Native => policy::Jail::Missing,
        sandbox::BackendKind::Host | sandbox::BackendKind::Ssh => policy::Jail::Off,
    };
    if sbx_cfg.backend == sandbox::BackendKind::Native && !sandbox::native_backend_available() {
        if sandbox::native_sandbox_supported() {
            notices.say(
                "warning: the OS sandbox works on this machine but Medha's own profile failed \
                 to apply — this is a Medha bug, please report it. Shell commands run on the \
                 host; the scanner + approval gate still apply.",
            );
        } else {
            notices.say(
                "warning: OS-native sandbox unavailable here (needs macOS Seatbelt, or Linux \
                 Landlock on kernel ≥5.13) — shell commands run on the host; the scanner + \
                 approval gate still apply. Set [sandbox] backend = \"host\" to silence.",
            );
        }
    }
    // Approved roots update native profiles without exposing whole tool homes.
    let approved = sandbox::ApprovedRoots::default();
    // Shared network grant: the exec backend enforces it, the permission manager
    // flips and persists it. One handle so a session/persistent grant reaches both.
    let net_grant = sandbox::NetworkGrant::default();
    let extra_writable = lock.sandbox.extra_writable_paths();
    let exec_backend = sandbox::select_backend(
        &sbx_cfg,
        extra_writable,
        approved.clone(),
        net_grant.clone(),
    );
    let verifier_exec = Arc::clone(&exec_backend);

    // Writers re-root this template in their isolated worktrees.
    let sandbox_template = agents::SandboxTemplate {
        trust: trust_path.clone(),
        audit: audit_path.clone(),
        gate: surface.gate.clone(),
        exec: Arc::clone(&exec_backend),
        snapshots: state.join("snapshots"),
        readable: vec![config::user_skills_dir()?],
        approved: approved.clone(),
        net_grant: net_grant.clone(),
    };
    let mut workspace = WorkspaceSandbox::new_with_state_root(
        cwd.clone(),
        trust_path,
        audit_path,
        Some(surface.gate.clone()),
        approved,
        medha_home.clone(),
    )?
    .with_exec_backend(exec_backend)
    .with_network_grant(net_grant)?
    // Bundled user-skill files are trusted configuration, not workspace data.
    .with_readable_roots(&[config::user_skills_dir()?])
    .with_snapshots_dir(state.join("snapshots"));
    // Held to the end of the run; a temp directory that refuses it just means no scratch.
    let scratch = sandbox::Scratch::create().ok();
    if let Some(scratch) = &scratch {
        workspace = workspace.with_scratch(scratch.path());
    }
    let workspace = Arc::new(workspace);
    // Ambiguous skill content receives both deterministic and model review.
    let security_judge = Arc::new(skill_judge::LlmJudge::new(provider.clone()));
    let context_file_loader = context::ctxfiles::ContextFileLoader::new()
        .with_judge(security_judge.clone())
        .with_limits(
            lock.context_files.max_chars,
            lock.context_files
                .max_chars
                .min(context::ctxfiles::PROGRESSIVE_MAX_CHARS),
        );
    let startup_context = if lock.context_files.enabled {
        context_file_loader
            .discover_startup(&cwd, &medha_home)
            .await
    } else {
        Vec::new()
    };
    let persona_file = context_file_loader.load_persona(&medha_home).await?;
    let progressive_context =
        (lock.context_files.enabled && lock.context_files.progressive_discovery).then(|| {
            Arc::new(
                context::ctxfiles::ProgressiveContextFiles::new(context_file_loader, cwd.clone())
                    .with_authorizer(workspace.clone()),
            )
        });
    let mut session_plugins = plugin_session::SessionPlugins::discover(crate::plugin_store::store(
        &medha_home,
        &cwd,
        &state,
    ));
    let skill_store = {
        let skills = tools::SkillStore::new(
            workspace.root().join(".medha").join("skills"),
            Some(config::user_skills_dir()?),
        )
        .with_judge(security_judge);
        session_plugins.add_skills(&skills);
        Arc::new(skills)
    };
    let mut registry = ToolRegistry::with_workspace(workspace.clone(), artifacts.clone());
    let lsp_manager = if lock.lsp.enabled {
        let mut lsp_config = lsp::Config {
            enabled: true,
            startup_timeout: std::time::Duration::from_millis(lock.lsp.startup_timeout_ms),
            request_timeout: std::time::Duration::from_millis(lock.lsp.request_timeout_ms),
            diagnostics_timeout: std::time::Duration::from_millis(lock.lsp.diagnostics_timeout_ms),
            diagnostic_settle: std::time::Duration::from_millis(lock.lsp.diagnostic_settle_ms),
            idle_timeout: std::time::Duration::from_millis(lock.lsp.idle_timeout_ms),
            restart_backoff: std::time::Duration::from_millis(lock.lsp.restart_backoff_ms),
            max_restart_attempts: lock.lsp.max_restart_attempts,
            max_servers: lock.lsp.max_servers,
            max_results: lock.lsp.max_results,
            max_text_chars: lock.lsp.max_text_chars,
            max_open_documents: lock.lsp.max_open_documents,
            install_timeout: std::time::Duration::from_millis(lock.lsp.install_timeout_ms),
            write_timeout: std::time::Duration::from_millis(lock.lsp.write_timeout_ms),
            max_frame_bytes: lock.lsp.max_frame_bytes,
            allow_network: lock.lsp.allow_network,
            home: sbx_cfg.home.clone(),
            ..lsp::Config::default()
        };
        for configured in &lock.lsp.servers {
            let id = configured.id.trim();
            if id.is_empty() || configured.trust != "workspace" {
                notices.say(&format!(
                    "note: ignored invalid LSP server '{}' (id and trust = \"workspace\" are required)",
                    configured.id
                ));
                continue;
            }
            let settings = toml_table_to_json(&configured.settings);
            // A commandless entry only tunes a built-in server of the same id.
            if configured.command.is_empty() && configured.languages.is_empty() {
                match lsp_config.servers.iter_mut().find(|server| server.id == id) {
                    Some(server) => server.settings = settings,
                    None => notices.say(&format!(
                        "note: ignored LSP settings for unknown server '{id}' (add a command to define it)"
                    )),
                }
                continue;
            }
            if configured.command.is_empty() || configured.languages.is_empty() {
                notices.say(&format!(
                    "note: ignored invalid LSP server '{id}' (command and languages are required to define one)"
                ));
                continue;
            }
            let adapter = lsp::ServerAdapter {
                id: id.to_string(),
                command: configured.command.clone(),
                languages: lsp::language_mappings(&configured.languages),
                root_markers: configured.root_markers.clone(),
                requires_approval: true,
                settings,
            };
            lsp_config
                .servers
                .retain(|existing| existing.id != adapter.id);
            lsp_config.servers.push(adapter);
        }
        let manager = Arc::new(lsp::LspManager::new(cwd.clone(), lsp_config));
        registry.register_lsp(manager.clone());
        Some(manager)
    } else {
        None
    };
    // MCP definitions are portable; credentials remain in the user store.
    let mut mcp_servers: Vec<mcp::ServerConfig> = model_profiles
        .lock()
        .ok()
        .map(|cfg| {
            cfg.mcp
                .iter()
                // Remote servers intentionally have no command.
                .filter(|(id, server)| {
                    let reachable = !server.command.is_empty() || !server.url.is_empty();
                    !id.trim().is_empty() && reachable
                })
                .map(|(id, server)| config::resolve_mcp_server(id, server))
                .collect()
        })
        .unwrap_or_default();
    let configured_mcp: std::collections::HashSet<String> =
        mcp_servers.iter().map(|server| server.id.clone()).collect();
    let shared_mcp: std::collections::HashSet<String> = model_profiles
        .lock()
        .map(|cfg| {
            cfg.mcp
                .iter()
                .filter(|(_, server)| crate::mcp_shared::is_shared(server))
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default();
    let plugin_mcp = session_plugins.mcp_servers(&configured_mcp);
    let plugin_mcp_ids = plugin_mcp.iter().map(|server| server.id.clone()).collect();
    mcp_servers.extend(plugin_mcp);
    // An idle manager allows live additions without a restart.
    let mcp_manager = {
        let mcp_config = |servers: Vec<mcp::ServerConfig>| mcp::Config {
            enabled: true,
            servers,
            startup_timeout: std::time::Duration::from_millis(lock.mcp.startup_timeout_ms),
            request_timeout: std::time::Duration::from_millis(lock.mcp.request_timeout_ms),
            max_text_chars: lock.mcp.max_text_chars,
            allow_network: lock.mcp.allow_network,
            health_interval: std::time::Duration::from_millis(lock.mcp.health_interval_ms),
            max_reconnects: lock.mcp.max_reconnects,
            park_probe: std::time::Duration::from_millis(lock.mcp.park_probe_ms),
            auth_timeout: std::time::Duration::from_millis(lock.mcp.auth_timeout_ms),
            http_timeout: std::time::Duration::from_millis(lock.mcp.http_timeout_ms),
            tokens: Some(Arc::new(config::McpTokens)),
            cache: Some(medha_home.join("mcp-cache")),
            home: sbx_cfg.home.clone(),
        };
        // With a shared host, the user's remote servers are the host's to run;
        // without one answering, this chat runs them as before.
        let mut manager = None;
        if let Some(endpoint) = crate::mcp_shared::endpoint() {
            let own: Vec<_> = mcp_servers
                .iter()
                .filter(|server| !shared_mcp.contains(&server.id))
                .cloned()
                .collect();
            let attached = mcp::McpManager::new(cwd.clone(), mcp_config(own.clone()));
            // A host that is still starting gets a moment to bind.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let joined = loop {
                match tokio::time::timeout_at(
                    deadline.into(),
                    attached.attach_hub(endpoint.clone()),
                )
                .await
                {
                    Ok(Ok(())) => break true,
                    Ok(Err(_)) if std::time::Instant::now() < deadline => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    _ => break false,
                }
            };
            if joined {
                let shared: Vec<_> = mcp_servers
                    .iter()
                    .filter(|server| shared_mcp.contains(&server.id))
                    .cloned()
                    .collect();
                tokio::spawn({
                    let attached = attached.clone();
                    async move { attached.share(shared).await }
                });
                mcp_servers = own;
                manager = Some(attached);
            } else {
                notices.say(
                    "note: Medha's connection host did not answer; this chat connects its own MCP servers",
                );
            }
        }
        let had_servers = !mcp_servers.is_empty();
        let manager = Arc::new(
            manager.unwrap_or_else(|| mcp::McpManager::new(cwd.clone(), mcp_config(mcp_servers))),
        );
        registry.register_mcp(manager.clone());
        if had_servers {
            tokio::spawn({
                let manager = manager.clone();
                async move { manager.connect_startup().await }
            });
        }
        Some(manager)
    };
    let memory_store = Arc::new(memory::MemoryProjection::open(
        state.join("memory.db"),
        medha_home.join("memory.db"),
    )?);
    let k3_budget_tokens = lock.memory.k3_budget_tokens;
    let stale_after_days = lock.memory.stale_after_days;
    if lock.memory.enabled {
        registry.register_memory_configured(
            memory_store.clone(),
            k3_budget_tokens,
            stale_after_days,
        );
    }
    registry.register_session_search(log.clone(), artifacts.clone());
    let search_handle = registry.search_handle();
    if let Ok(cfg_guard) = model_profiles.lock() {
        *search_handle.lock().expect("search settings lock") =
            config::resolve_search_with(&cfg_guard, options.search_env);
    }
    if let Ok(mut slot) = registry.clarify_handle().lock() {
        *slot = Some(surface.asker);
    }
    // Parent and writer worktrees use the same verifier command.

    let agent_runner = Arc::new(orchestrator::DeferredRunner::default());
    let agent_registry = agents::WorktreeWorkspaces::registry_handle();
    // Without a repository, writer isolation is unavailable and writes are refused.
    let agent_workspaces = if lock.agents.enabled && lock.agents.write {
        agents::WorktreeWorkspaces::discover(
            &cwd,
            state.join("worktrees"),
            Arc::clone(&agent_registry),
            sandbox_template,
            verify_cmd.clone(),
            verify_timeout,
            lock.agents.max_patch_bytes,
        )
        .await
        .map(|workspaces| Arc::new(workspaces) as Arc<dyn orchestrator::Workspaces>)
    } else {
        None
    };
    let agent_budget: kernel::BudgetHandle = Arc::new(std::sync::Mutex::new(None));
    let agent_log_outbox = if lock.agents.enabled {
        Some(Arc::new(
            agents::LogOutbox::new(log.clone(), state.join("agent-process-leases"))
                .context("creating the agent process lease")?,
        ))
    } else {
        None
    };
    let agent_control = lock.agents.enabled.then(|| {
        let log_outbox = agent_log_outbox
            .as_ref()
            .expect("enabled agents have a process lease")
            .clone();
        let mut control = orchestrator::AgentControl::new(
            agent_runner.clone(),
            tokio_util::sync::CancellationToken::new(),
        )
        .with_limits(lock.agents.max_active, lock.agents.max_depth)
        .with_wait_bounds(orchestrator::WaitBounds {
            min: std::time::Duration::from_secs(lock.agents.min_wait_secs),
            default: std::time::Duration::from_secs(lock.agents.default_wait_secs),
            max: std::time::Duration::from_secs(lock.agents.max_wait_secs),
        })
        .with_transcript_tail(lock.agents.transcript_tail)
        .with_cancel_grace(std::time::Duration::from_secs(
            lock.agents.cancel_grace_secs,
        ))
        .with_outbox(log_outbox.clone())
        .with_transcripts(log_outbox)
        .with_owner(registry.agent_session_handle())
        .with_budget(Arc::clone(&agent_budget));
        if let Some(workspaces) = agent_workspaces {
            control = control.with_workspaces(workspaces);
        }
        let control = Arc::new(control);
        registry.register_agents(control.clone(), lock.agents.max_turns);
        control
    });
    // Freeze the skill capability catalogue only after every static tool has
    // been registered. Registering it earlier made valid requirements such as
    // memory.write, sessions.search, and agent.spawn look unavailable.
    registry.register_skills(skill_store.clone());
    let registered_tools = registry.tool_names();
    let agent_parent = registry.agent_parent_handle();
    let agent_session = registry.agent_session_handle();
    let executor = Arc::new(registry);
    // Writer worktrees need the concrete registry, not its erased executor.
    if let Ok(mut slot) = agent_registry.lock() {
        *slot = Some(Arc::downgrade(&executor));
    }
    // `[tools] preset = "minimal"`, or MEDHA_TOOLS. Narrowing happens once, at
    // the boundary: children inherit from the narrowed executor, so a preset
    // cannot be widened by delegating.
    let preset = lockfile::ToolsConfig {
        preset: options
            .tools_preset
            .clone()
            .unwrap_or(lock.tools.preset.clone()),
    };
    preset.validate().map_err(anyhow::Error::msg)?;
    let executor: Arc<dyn kernel::Executor> = match preset.exposed() {
        Some(exposed) => Arc::new(orchestrator::NarrowedExecutor::new(
            executor,
            Some(&exposed),
        )),
        None => executor,
    };
    let known_tools: std::collections::HashSet<String> =
        executor.specs().into_iter().map(|spec| spec.name).collect();

    let configured_compressor = {
        let configured = model_profiles.lock().unwrap().clone();
        configured
            .auxiliary
            .compression
            .clone()
            .filter(|name| !name.trim().is_empty())
            .map(|name| (name, configured))
    };
    let (summary_provider, replay_summary) = match configured_compressor {
        Some((name, configured)) => {
            let auxiliary =
                config::resolve_model_as(&configured, &name, options.model_env.api_key.as_deref())
                    .and_then(|resolved| {
                        providers::OpenAiCompat::from_profile(
                            resolved.provider,
                            resolved.credential,
                        )
                        .map_err(anyhow::Error::from)
                    });
            match auxiliary {
                Ok(auxiliary) => (Arc::new(auxiliary), false),
                Err(error) => {
                    tracing::warn!(%error, "auxiliary compression profile unavailable; using chat route");
                    (provider.clone(), true)
                }
            }
        }
        None => (provider.clone(), true),
    };
    let recall_store = memory_store.clone();
    let memory_enabled = lock.memory.enabled;
    let context_engine = Arc::new(
        context::PipelineEngine::new(lock.context.to_policy())
            .with_summarizer(Arc::new(
                context::LlmSummarizer::new(summary_provider).with_replay(replay_summary),
            ))
            .with_artifacts(artifacts.clone())
            .with_full_compaction_refresh(Arc::new(move |system| {
                if !memory_enabled {
                    return system.to_string();
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs_f64())
                    .unwrap_or(0.0);
                match memory::recall::compile_k3_configured(
                    &recall_store,
                    k3_budget_tokens,
                    now,
                    stale_after_days,
                ) {
                    Ok(block) => memory::recall::replace_k3(system, &block),
                    Err(_) => system.to_string(),
                }
            })),
    );

    let stale = unknown_approvals(&lock.policy.approve, &registered_tools);
    if !stale.is_empty() {
        notices.say(&format!(
            "warning: [policy].approve names {} tool(s) that do not exist: {} — \
             they gate nothing; update medha.lock",
            stale.len(),
            stale.join(", ")
        ));
    }
    let policy = Arc::new(
        policy::DefaultPolicy::requiring_approval(approve_list_from(
            lock.policy.approve.clone(),
            options.approve.as_deref().unwrap_or_default(),
        ))
        .with_workspace(workspace.root())
        .with_scratch(
            scratch
                .as_ref()
                .map_or(std::path::Path::new(""), |s| s.path()),
        )
        .with_memory_write_approval(&lock.memory.write_approval)
        .with_jail(jail),
    );

    let verifier: Arc<dyn kernel::Verifier> = match verify_cmd.clone() {
        Some(cmd) => Arc::new(CommandVerifier {
            command: cmd,
            dir: cwd.clone(),
            limit: verify_timeout,
            required: verify_required,
            exec: Arc::clone(&verifier_exec),
        }),
        None => Arc::new(kernel::NoVerify),
    };

    // Children inherit this already-resolved budget.
    let base_budget = options.budget.apply(lock.budget.to_budget());
    let ui_config = lock.ui.clone();

    // models.dev prices are advisory for self-hosted routes.
    let pricing = match (lock.pricing.input_per_mtok, lock.pricing.output_per_mtok) {
        (Some(i), Some(o)) => Some(kernel::Pricing {
            input_per_mtok: i,
            output_per_mtok: o,
            // Configured rates are the operator's own; nothing is inferred for
            // cached reads, so they bill at the input rate unless stated.
            cached_input_per_mtok: lock.pricing.cached_input_per_mtok,
            indicative: false,
        }),
        _ => providers::models_dev::pricing(&model_name)
            .await
            .map(|(input, output, cached)| kernel::Pricing {
                input_per_mtok: input,
                output_per_mtok: output,
                cached_input_per_mtok: cached,
                indicative: true,
            }),
    };
    match &pricing {
        Some(p) if p.indicative => notices.say(&format!(
            "cost meter: {model_name} list price from models.dev (${:.2}/M in, ${:.2}/M out) — \
             indicative only; set [pricing] in medha.lock for your real rate",
            p.input_per_mtok, p.output_per_mtok
        )),
        Some(_) => {}
        None => {
            if base_budget.max_cost_usd.is_some() {
                notices.say(&format!(
                    "warning: max_cost_usd is set but no pricing is known for '{model_name}' — \
                     model requests will be refused. Set [pricing] input_per_mtok / \
                     output_per_mtok in medha.lock."
                ));
            }
        }
    }

    let max_parallel_tools = options
        .max_parallel_tools
        .or(lock.budget.max_parallel_tools)
        .unwrap_or(kernel::DEFAULT_MAX_PARALLEL_TOOLS);
    let hook_runner = session_plugins.hook_runner(&cwd, &sbx_cfg.home);
    for diagnostic in session_plugins
        .warnings()
        .iter()
        .chain(hook_runner.diagnostics())
    {
        notices.say(&format!("warning: plugin: {diagnostic}"));
    }
    let mut kernel = Kernel::new(
        provider,
        log.clone(),
        executor,
        context_engine,
        artifacts,
        policy,
        surface.gate,
        verifier,
    )
    .with_pricing(pricing)
    .with_max_parallel_tools(max_parallel_tools)
    .with_hooks(Arc::new(hook_runner));
    if let Some(auxiliary) = auxiliary_vision {
        kernel = kernel.with_vision(auxiliary);
    }
    if let Some(progressive_context) = progressive_context {
        kernel = kernel.with_progressive_context(progressive_context);
    }
    let kernel = Arc::new(kernel);
    // A weak back-reference avoids retaining the entire tool graph.
    if let Ok(mut slot) = agent_parent.lock() {
        *slot = Some(Arc::downgrade(&kernel.executor));
    }
    agent_runner.install(Arc::new(agents::KernelRunner::new(&kernel, surface.agents)));

    let configured_persona = model_profiles
        .lock()
        .ok()
        .and_then(|c| c.agent.identity.clone());
    let persona = persona_file
        .as_ref()
        .filter(|file| !file.blocked())
        .map(|file| file.content.as_str())
        .or(configured_persona.as_deref());
    if let Some(file) = persona_file.as_ref().filter(|file| file.blocked()) {
        notices.say(&file.content);
    }
    let mut system = context::identity::system_prompt_for_tools(persona, &known_tools);
    // Give time-sensitive requests an explicit clock and workspace.
    let today = chrono::Local::now().format("%A, %-d %B %Y").to_string();
    let scratch_line = scratch.as_ref().map_or_else(String::new, |scratch| {
        format!(
            "\n- Scratch folder: {} (yours, empty, deleted when this session ends). Put \
             throwaway files, test repositories and experiments here, never in the workspace \
             or /tmp; file tools and shell commands can use it without asking.",
            scratch.path().display()
        )
    });
    system.push_str(&format!(
        "\n\nEnvironment:\n- Today's date: {today}\n- Workspace: {}{scratch_line}\n\nFor anything \
         time-sensitive (news, prices, \"latest\"/\"recent\"/\"today\"), use the current \
         date above — do not assume an older year in your searches or answers.",
        cwd.display()
    ));
    let project_context = context::ctxfiles::render_startup(&startup_context);
    if !project_context.is_empty() {
        system.push_str("\n\n");
        system.push_str(&project_context);
    }
    let skills_manifest = skill_store.manifest(
        &known_tools,
        if has_task {
            Some(prompt.as_str())
        } else {
            None
        },
    );
    if !skills_manifest.is_empty() {
        system.push_str("\n\n");
        system.push_str(&skills_manifest);
    }
    // Resumed turns append to the original session.
    let (mut session, resumed) = match resolve_resume(&log, &options.resume, notices).await {
        Ok(Some((id, msgs))) => {
            notices.say(&format!(
                "resumed session {id} ({} prior messages)",
                msgs.len()
            ));
            (
                Session {
                    id,
                    done: false,
                    autonomy: kernel::AutonomyLevel::Careful,
                },
                msgs,
            )
        }
        Ok(None) => (Session::new(), Vec::new()),
        Err(e) => {
            notices.say(&format!("resume failed: {e} — starting a fresh session"));
            (Session::new(), Vec::new())
        }
    };
    if let Ok(mut slot) = agent_session.lock() {
        *slot = Some(session.id);
    }
    // Settle durable dispatches left without a terminal event after a crash.
    if let Some(control) = &agent_control {
        match control.reap_abandoned(session.id).await {
            0 => {}
            n => notices.say(&format!(
                "note: {n} background agent(s) from a previous run never recorded a result — \
                 reported as unknown; their transcripts are still readable"
            )),
        }
        // A writer's diff outlives the process that produced it: it sits outside
        // the repository until a human accepts it, and the outbox still owes it
        // after a restart. The status line counts only what this process is
        // holding, which is nothing yet — so without saying it here, finished work
        // waits silently and is found by remembering to go looking.
        match control.outstanding().await.len() {
            0 => {}
            n => notices.say(&format!(
                "note: {n} agent patch(es) from an earlier run are still waiting for you — \
                 review them with /agents"
            )),
        }
    }
    if lock.memory.enabled {
        let session_events = log.events(session.id).await;
        let forked = session_events
            .first()
            .is_some_and(|event| event.provenance.source == "fork");
        if forked {
            memory_store.rebuild_project(session_events.into_iter())?;
        } else {
            memory_store.rebuild_project(
                log.all_events()?
                    .into_iter()
                    .filter(|event| event.provenance.source != "fork"),
            )?;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64())
            .unwrap_or(0.0);
        let k3 = memory::recall::compile_k3_configured(
            &memory_store,
            k3_budget_tokens,
            now,
            stale_after_days,
        )?;
        system = memory::recall::replace_k3(&system, &k3);
    }
    session.autonomy = autonomy;
    if autonomy == kernel::AutonomyLevel::Plan {
        notices.say("Plan mode: read-only investigation; /mode careful enables implementation.");
    }
    if verify_required {
        notices.say(
            "Required verification: completion must pass the configured check (skipped in Plan mode).",
        );
    }
    for file in startup_context.iter().chain(persona_file.iter()) {
        log.append(kernel::Event::context_file(
            &session,
            &file.path.display().to_string(),
            &file.content,
            file.blocked(),
            if file.global {
                kernel::TrustLabel::User
            } else {
                kernel::TrustLabel::Workspace
            },
        ))
        .await?;
    }
    Ok(Started {
        kernel,
        session,
        system,
        resumed,
        prompt,
        attached_images,
        log,
        model_name,
        model_profiles,
        active_profile,
        max_ctx,
        open_setup,
        base_budget,
        agent_budget,
        agent_control,
        ui_config,
        workspace,
        skill_store,
        memory_store,
        k3_budget_tokens,
        stale_after_days,
        known_tools,
        search_handle,
        lsp_manager,
        mcp_manager,
        session_plugins,
        configured_mcp,
        plugin_mcp_ids,
        scratch,
    })
}

/// One chat, built and ready for a surface to drive.
pub struct Started {
    pub kernel: Arc<Kernel<OpenAiCompat, store::SqliteLog>>,
    pub session: Session,
    pub system: String,
    pub resumed: Vec<Message>,
    pub prompt: String,
    pub attached_images: Vec<kernel::MediaPart>,
    pub log: Arc<store::SqliteLog>,
    pub model_name: String,
    pub model_profiles: Arc<std::sync::Mutex<config::Config>>,
    pub active_profile: String,
    pub max_ctx: Option<u32>,
    pub open_setup: bool,
    pub base_budget: kernel::Budget,
    pub agent_budget: kernel::BudgetHandle,
    pub agent_control: Option<Arc<orchestrator::AgentControl>>,
    pub ui_config: lockfile::UiConfig,
    pub workspace: Arc<WorkspaceSandbox>,
    pub skill_store: Arc<tools::SkillStore>,
    pub memory_store: Arc<memory::MemoryProjection>,
    pub k3_budget_tokens: u32,
    pub stale_after_days: u32,
    pub known_tools: std::collections::HashSet<String>,
    pub search_handle: tools::SearchHandle,
    pub lsp_manager: Option<Arc<lsp::LspManager>>,
    pub mcp_manager: Option<Arc<mcp::McpManager>>,
    pub session_plugins: plugin_session::SessionPlugins,
    pub configured_mcp: std::collections::HashSet<String>,
    pub plugin_mcp_ids: std::collections::HashSet<String>,
    /// Deleted when dropped, so it lives as long as the chat does.
    pub scratch: Option<sandbox::Scratch>,
}

async fn resolve_resume(
    log: &store::SqliteLog,
    resume: &Resume,
    notices: &dyn Notices,
) -> Result<Option<(ulid::Ulid, Vec<Message>)>> {
    let id = match resume {
        Resume::Id(idstr) => ulid::Ulid::from_string(idstr.trim())
            .map_err(|_| anyhow::anyhow!("invalid session id '{idstr}'"))?,
        Resume::Latest => match log.list_sessions()?.into_iter().next() {
            Some(s) => s.id,
            None => {
                notices.say("no prior sessions to continue — starting fresh");
                return Ok(None);
            }
        },
        Resume::None => return Ok(None),
    };
    let events = log.checked_events(id).await?;
    if events.is_empty() {
        anyhow::bail!("session {id} has no events (not found)");
    }
    Ok(Some((id, kernel::project_messages(&events))))
}

fn toml_table_to_json(table: &toml::Table) -> serde_json::Value {
    if table.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::to_value(table).unwrap_or(serde_json::Value::Null)
    }
}
