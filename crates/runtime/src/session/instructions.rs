use super::*;

pub(super) struct Discovered {
    pub startup_context: Vec<context::ctxfiles::CtxFile>,
    pub persona_file: Option<context::ctxfiles::CtxFile>,
    pub progressive_context: Option<Arc<context::ctxfiles::ProgressiveContextFiles>>,
    pub session_plugins: plugin_session::SessionPlugins,
    pub skill_store: Arc<tools::SkillStore>,
}

pub(super) async fn discover(
    setup: &Setup<'_>,
    provider: &Arc<OpenAiCompat>,
    workspace: &Arc<WorkspaceSandbox>,
) -> Result<Discovered> {
    let Setup {
        lock,
        cwd,
        state,
        medha_home,
        ..
    } = *setup;
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
        context_file_loader.discover_startup(cwd, medha_home).await
    } else {
        Vec::new()
    };
    let persona_file = context_file_loader.load_persona(medha_home).await?;
    let progressive_context =
        (lock.context_files.enabled && lock.context_files.progressive_discovery).then(|| {
            Arc::new(
                context::ctxfiles::ProgressiveContextFiles::new(
                    context_file_loader,
                    cwd.to_path_buf(),
                )
                .with_authorizer(workspace.clone()),
            )
        });
    let mut session_plugins = plugin_session::SessionPlugins::discover(crate::plugin_store::store(
        medha_home, cwd, state,
    ));
    let skill_store = {
        let skills = tools::SkillStore::new(
            workspace.root().join(".medha").join("skills"),
            Some(config::user_skills_dir()?),
        )
        .with_project_source(crate::project_trust::skill_source())
        .with_judge(security_judge);
        session_plugins.add_skills(&skills);
        Arc::new(skills)
    };
    Ok(Discovered {
        startup_context,
        persona_file,
        progressive_context,
        session_plugins,
        skill_store,
    })
}
