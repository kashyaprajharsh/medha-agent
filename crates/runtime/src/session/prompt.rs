use super::*;

pub(super) struct Input<'a> {
    pub kernel: &'a Kernel<OpenAiCompat, store::SqliteLog>,
    pub session: &'a mut Session,
    pub model_profiles: &'a Arc<std::sync::Mutex<config::Config>>,
    pub known_tools: &'a std::collections::HashSet<String>,
    pub persona_file: &'a Option<context::ctxfiles::CtxFile>,
    pub startup_context: &'a [context::ctxfiles::CtxFile],
    pub scratch: &'a Option<sandbox::Scratch>,
    pub live_context: &'a crate::project_session::LiveContext,
    pub agent_session: &'a tools::SessionHandle,
    pub agent_control: &'a Option<Arc<orchestrator::AgentControl>>,
    pub log: &'a store::SqliteLog,
    pub memory_store: &'a memory::MemoryProjection,
    pub autonomy: kernel::AutonomyLevel,
    pub verify_required: bool,
}

pub(super) async fn build(setup: &Setup<'_>, input: Input<'_>) -> Result<String> {
    let Setup {
        lock, cwd, notices, ..
    } = *setup;
    let Input {
        kernel,
        session,
        model_profiles,
        known_tools,
        persona_file,
        startup_context,
        scratch,
        live_context,
        agent_session,
        agent_control,
        log,
        memory_store,
        autonomy,
        verify_required,
    } = input;
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
    let mut system = context::identity::system_prompt_for_tools(persona, known_tools);
    // What follows changes per chat or per day; this start is the same for all.
    kernel.provider.stable_system_start(&system);
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
    let project_context = context::ctxfiles::render_startup(startup_context);
    if !project_context.is_empty() {
        system.push_str("\n\n");
        system.push_str(&project_context);
    }
    live_context.bind_project_slot(context::identity::ProjectSlot::after(&system));
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
        let k3 = memory::recall::compile_k3_snapshot(
            memory_store,
            lock.memory.k3_budget_tokens,
            now,
            lock.memory.stale_after_days,
        )?;
        system = live_context.replace_memory(&k3.text);
        if !k3.text.is_empty() {
            log.append(kernel::Event::context_file(
                session,
                "memory recall index",
                &k3.text,
                false,
                k3.trust,
            ))
            .await?;
        }
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
            session,
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
    Ok(system)
}
