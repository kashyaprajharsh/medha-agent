use super::*;

pub(super) struct Built {
    pub agent_runner: Arc<orchestrator::DeferredRunner>,
    pub agent_registry: agents::RegistryHandle,
    pub agent_budget: kernel::BudgetHandle,
    pub agent_control: Option<Arc<orchestrator::AgentControl>>,
}

pub(super) async fn build(
    setup: &Setup<'_>,
    registry: &mut ToolRegistry,
    log: Arc<store::SqliteLog>,
    sandbox_template: agents::SandboxTemplate,
    verify_cmd: Option<String>,
    verify_timeout: std::time::Duration,
) -> Result<Built> {
    let Setup {
        lock, cwd, state, ..
    } = *setup;
    let agent_runner = Arc::new(orchestrator::DeferredRunner::default());
    let agent_registry = agents::WorktreeWorkspaces::registry_handle();
    // Without a repository, writer isolation is unavailable and writes are refused.
    let agent_workspaces = if lock.agents.enabled && lock.agents.write {
        agents::WorktreeWorkspaces::discover(
            cwd,
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
    Ok(Built {
        agent_runner,
        agent_registry,
        agent_budget,
        agent_control,
    })
}
