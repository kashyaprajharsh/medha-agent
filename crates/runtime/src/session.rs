//! Builds one chat on an opened workspace: its sandbox, tools, agents, context and kernel.

use crate::approvals::unknown_approvals;
use crate::lease::{Ownership, SessionLease};
use crate::model::Model;
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

mod agents_setup;
mod context_setup;
mod instructions;
mod media;
mod ownership;
mod pricing;
mod prompt;
mod sandbox_setup;
mod services;

struct Setup<'a> {
    lock: &'a lockfile::MedhaLock,
    options: &'a SessionOptions,
    cwd: &'a Path,
    state: &'a Path,
    medha_home: &'a Path,
    notices: &'a dyn Notices,
}

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
    pub fn of(lock: &lockfile::MedhaLock, options: &SessionOptions, folder: &Path) -> Result<Self> {
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
            let raw = lockfile::MedhaLock::load(folder.join("medha.lock"))?.unwrap_or_default();
            if options.require_verify || raw.verify.command.is_none() {
                anyhow::bail!(
                    "required verification needs [verify].command in medha.lock or MEDHA_VERIFY"
                );
            }
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
    crate::session_mcp::validate(&options.session_mcp)?;
    let Model {
        provider,
        name: model_name,
        catalogue_pricing,
        profiles: model_profiles,
        active_profile,
        max_ctx,
        open_setup,
        credential,
    } = model;
    let prompt = options.prompt.clone();
    let cwd = workspace_home.root.clone();
    let state = workspace_home.state.clone();
    let medha_home = workspace_home.home.clone();
    let base_budget = options.budget.apply(lock.budget.to_budget()).for_startup();
    let pricing = pricing::resolve(lock, &model_name, catalogue_pricing, &base_budget, notices);
    let crate::workspace::Store { log, artifacts } = workspace_home.open_store()?;

    let (mut session, resumed, lease) =
        ownership::open(&log, &state, &options.resume, notices).await?;

    let setup = Setup {
        lock,
        options,
        cwd: &cwd,
        state: &state,
        medha_home: &medha_home,
        notices,
    };
    let media::Admitted {
        prompt,
        attached_images,
        auxiliary_vision,
    } = media::admit(&setup, prompt, &model_profiles, artifacts.clone()).await?;

    let surface = surface(&cwd, &prompt);
    let has_task = !options.first_run_setup && !prompt.trim().is_empty();
    let sandbox_setup::Built {
        workspace,
        live_settings,
        verifier_exec,
        sandbox_template,
        sbx_cfg,
        scratch,
    } = sandbox_setup::build(&setup, &surface)?;
    // An idle start reviews instructions in its own capped pool, so flagged
    // files are still judged rather than blocked for the whole session.
    let startup_budget = if has_task {
        base_budget.clone()
    } else {
        base_budget.clone().with_fresh_pool()
    };
    let instructions::Discovered {
        startup_context,
        persona_file,
        progressive_context,
        mut session_plugins,
        skill_store,
    } = kernel::auxiliary::scope(
        startup_budget,
        kernel::Provider::context_identity(provider.as_ref()),
        pricing,
        instructions::discover(&setup, &provider, &workspace),
    )
    .await?;
    let mut registry = ToolRegistry::with_workspace(workspace.clone(), artifacts.clone());
    let lsp_manager = services::language_servers(&setup, &sbx_cfg, &mut registry);
    let services::Mcp {
        mcp_manager,
        configured_mcp,
        session_mcp_ids,
        plugin_mcp_ids,
    } = services::connections(
        &setup,
        &sbx_cfg,
        &model_profiles,
        &mut registry,
        &mut session_plugins,
    )
    .await?;
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
            config::resolve_search_with(&cfg_guard, options.search_env.clone());
    }
    if let Ok(mut slot) = registry.clarify_handle().lock() {
        *slot = Some(surface.asker);
    }
    // Parent and writer worktrees use the same verifier command.

    let agents_setup::Built {
        agent_runner,
        agent_registry,
        agent_budget,
        agent_control,
    } = agents_setup::build(
        &setup,
        &mut registry,
        log.clone(),
        sandbox_template,
        verify_cmd.clone(),
        verify_timeout,
    )
    .await?;
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
    let fallback = catalogue_pricing.map(|(input, output, cached, written)| kernel::Pricing {
        input_per_mtok: input,
        output_per_mtok: output,
        cached_input_per_mtok: cached,
        cache_write_per_mtok: written,
        indicative: true,
    });
    let live_context = crate::project_session::LiveContext::new(
        live_settings.clone(),
        skill_store.clone(),
        known_tools.clone(),
        has_task.then(|| prompt.clone()),
        fallback,
    );

    let context_engine = context_setup::build(
        &setup,
        &model_profiles,
        &provider,
        live_context.clone(),
        memory_store.clone(),
        artifacts.clone(),
    );

    pricing::warn_unknown(lock, &registered_tools, notices);
    let policy: Arc<dyn kernel::Policy> = live_settings.clone();
    let verifier: Arc<dyn kernel::Verifier> = Arc::new(crate::project_session::LiveVerifier::new(
        live_settings.clone(),
        verifier_exec,
        verify_timeout,
    ));

    // Children inherit this already-resolved budget.
    let ui_config = lock.ui.clone();

    let max_parallel_tools = options
        .max_parallel_tools
        .or(lock.budget.max_parallel_tools)
        .unwrap_or(kernel::DEFAULT_MAX_PARALLEL_TOOLS);
    let server_waits = model_profiles
        .lock()
        .ok()
        .and_then(|c| c.agent.server_waits);
    let server_waits = server_waits.unwrap_or(kernel::recovery::WAITS);
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
    // Only the operator's own rate is fixed for the chat; a list price is asked
    // of the model in use at each call, so it follows a change of model.
    .with_pricing(pricing.filter(|price| !price.indicative))
    .with_request_context(live_context.clone())
    .with_secrets(kernel::redact::Secrets::new(
        credential
            .into_iter()
            .chain(kernel::redact::Secrets::in_environment()),
    ))
    .with_max_parallel_tools(max_parallel_tools)
    .with_server_waits(server_waits)
    .with_result_limits(lock.budget.tool_result_bytes, lock.budget.tool_round_bytes)
    .with_hooks(Arc::new(hook_runner))
    .with_image_shrinker(Arc::new(vision::Shrinker))
    .with_session_owner(Arc::new(Ownership::new(state.clone(), lease)));
    kernel
        .egress
        .mutations
        .configure(
            model_profiles
                .lock()
                .map_err(|_| anyhow::anyhow!("model configuration lock failed"))?
                .security
                .clone(),
        )
        .map_err(anyhow::Error::msg)?;
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
    agent_runner.install(Arc::new(
        agents::KernelRunner::new(&kernel, surface.agents).with_lease_directory(state.clone()),
    ));

    let system = prompt::build(
        &setup,
        prompt::Input {
            kernel: &kernel,
            session: &mut session,
            model_profiles: &model_profiles,
            known_tools: &known_tools,
            persona_file: &persona_file,
            startup_context: &startup_context,
            scratch: &scratch,
            live_context: &live_context,
            agent_session: &agent_session,
            agent_control: &agent_control,
            log: &log,
            memory_store: &memory_store,
            autonomy,
            verify_required,
        },
    )
    .await?;
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
        session_mcp_ids,
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
    pub session_mcp_ids: std::collections::HashSet<String>,
    pub plugin_mcp_ids: std::collections::HashSet<String>,
    /// Deleted when dropped, so it lives as long as the chat does.
    pub scratch: Option<sandbox::Scratch>,
}

async fn resolve_resume(
    log: &store::SqliteLog,
    state: &Path,
    resume: &Resume,
    notices: &dyn Notices,
) -> Result<Option<(ulid::Ulid, Vec<Message>, SessionLease)>> {
    let id = match resume {
        Resume::Id(idstr) => ulid::Ulid::from_string(idstr.trim())
            .map_err(|_| anyhow::anyhow!("invalid session id '{idstr}'"))?,
        Resume::Latest => match log
            .list_sessions()?
            .into_iter()
            .find(|s| s.parent.is_none())
        {
            Some(s) => s.id,
            None => {
                notices.say("no prior sessions to continue — starting fresh");
                return Ok(None);
            }
        },
        Resume::None => return Ok(None),
    };
    let lease = SessionLease::acquire(state, id).map_err(|error| error.context(LeaseFailure))?;
    let events = log.checked_events(id).await?;
    if events.is_empty() {
        anyhow::bail!("session {id} has no events (not found)");
    }
    Ok(Some((id, kernel::project_messages(&events), lease)))
}

#[derive(Debug)]
struct LeaseFailure;

impl std::fmt::Display for LeaseFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("could not take ownership of the conversation")
    }
}

impl std::error::Error for LeaseFailure {}
