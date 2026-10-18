use super::*;

pub(super) fn build(
    setup: &Setup<'_>,
    model_profiles: &Arc<std::sync::Mutex<config::Config>>,
    provider: &Arc<OpenAiCompat>,
    live_context: Arc<crate::project_session::LiveContext>,
    memory_store: Arc<memory::MemoryProjection>,
    artifacts: Arc<dyn kernel::ArtifactStore>,
) -> Arc<context::PipelineEngine> {
    let Setup { lock, options, .. } = *setup;
    let k3_budget_tokens = lock.memory.k3_budget_tokens;
    let stale_after_days = lock.memory.stale_after_days;
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
            let auxiliary = config::resolve_model_as(&configured, &name, Some(&options.model_env))
                .and_then(|resolved| {
                    providers::OpenAiCompat::from_profile(resolved.provider, resolved.credential)
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
    let recall_context = live_context.clone();
    Arc::new(
        context::PipelineEngine::new(lock.context.to_policy())
            .with_summarizer(Arc::new(
                context::LlmSummarizer::new(summary_provider)
                    .with_replay(replay_summary)
                    .with_prompt(Arc::new({
                        let live = live_context.clone();
                        move || live.summary_prompt()
                    })),
            ))
            .with_artifacts(artifacts.clone())
            .with_trusted_compaction_refresh(Arc::new(move |system| {
                if !memory_enabled {
                    return (system.to_string(), None);
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs_f64())
                    .unwrap_or(0.0);
                match memory::recall::compile_k3_snapshot(
                    &recall_store,
                    k3_budget_tokens,
                    now,
                    stale_after_days,
                ) {
                    Ok(snapshot) => (
                        recall_context.replace_memory(&snapshot.text),
                        Some(snapshot.trust),
                    ),
                    Err(_) => (system.to_string(), None),
                }
            })),
    )
}
