//! Typed desktop operations over the owning ACP runtime. No arbitrary tool dispatch.
use crate::config;
use kernel::{EventLog, Kernel, Provider, Session};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Both interactive surfaces switch the same provider adapter between turns.
pub(crate) trait ProfileProvider: Provider {
    fn switch_profile(&self, profile: &config::Resolved) -> Result<(), String>;
}

impl ProfileProvider for providers::OpenAiCompat {
    fn switch_profile(&self, profile: &config::Resolved) -> Result<(), String> {
        self.switch_provider_profile(profile.provider.clone(), profile.credential.clone())
            .map_err(|error| error.to_string())
    }
}

pub(crate) fn profiles(cfg: &config::Config) -> Value {
    json!(
        cfg.model_profiles()
            .into_iter()
            .map(|profile| json!({
                "name": profile.name,
                "model": profile.provider.model,
                "protocol": profile.provider.protocol,
                "default": profile.is_default,
                "reasoning_support": profile.provider.reasoning.as_str(),
            }))
            .collect::<Vec<_>>()
    )
}

pub(crate) fn settings<P: Provider>(
    provider: &P,
    session: &Session,
    active: &str,
    model: &str,
    cfg: &config::Config,
) -> Value {
    let reasoning = provider.reasoning();
    json!({
        "profiles": profiles(cfg), "profile": active, "model": model,
        "mode": session.autonomy.as_str(),
        "reasoning": reasoning.enabled.map_or("auto", |on| if on { "on" } else { "off" }),
        "effort": reasoning.effort.map_or("auto", |effort| effort.as_str()),
        "efforts": provider.reasoning_efforts().iter().map(|effort| effort.as_str()).collect::<Vec<_>>(),
        "reasoning_support": provider.reasoning_support().as_str(),
        "streaming": provider.streaming(), "context_limit": provider.context_window(),
    })
}

enum Change {
    Profile(String),
    Mode(kernel::AutonomyLevel),
    Reasoning(kernel::ReasoningConfig),
    Streaming(bool),
}

fn change(params: &Value, reasoning: kernel::ReasoningConfig) -> Result<Change, String> {
    let object = params
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or("change exactly one session setting at a time")?;
    let (key, value) = object.iter().next().ok_or("setting required")?;
    let text = || {
        value
            .as_str()
            .ok_or_else(|| format!("{key} must be a string"))
    };
    match key.as_str() {
        "profile" => Ok(Change::Profile(text()?.to_owned())),
        "mode" => kernel::AutonomyLevel::parse(text()?).map(Change::Mode),
        "effort" => kernel::ReasoningConfig::from_effort_text(text()?).map(Change::Reasoning),
        "reasoning" => match text()? {
            "auto" => Ok(Change::Reasoning(kernel::ReasoningConfig::default())),
            "off" => Ok(Change::Reasoning(kernel::ReasoningConfig {
                enabled: Some(false),
                effort: None,
            })),
            "on" => Ok(Change::Reasoning(kernel::ReasoningConfig {
                enabled: Some(true),
                ..reasoning
            })),
            _ => Err("reasoning must be auto, on, or off".into()),
        },
        "streaming" => value
            .as_bool()
            .map(Change::Streaming)
            .ok_or_else(|| "streaming must be a boolean".into()),
        _ => Err("unknown session setting".into()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle<P: ProfileProvider, L: EventLog + 'static>(
    method: &str,
    params: &Value,
    kernel: &Arc<Kernel<P, L>>,
    session: &mut Session,
    model: &mut String,
    active: &mut String,
    cfg: &Arc<Mutex<config::Config>>,
    running: bool,
    agents: Option<&Arc<orchestrator::AgentControl>>,
) -> Option<Result<Value, String>> {
    if !matches!(
        method,
        "session.settings" | "session.configure" | "agent.control" | "patch.list" | "patch.apply"
    ) {
        return None;
    }
    if matches!(method, "session.settings" | "session.configure") {
        match config::load() {
            Ok(Some(saved)) => match cfg.lock() {
                Ok(mut current) => *current = saved,
                Err(_) => return Some(Err("Model configuration unavailable".into())),
            },
            Err(error) => return Some(Err(error.to_string())),
            _ => {}
        }
    }
    Some(
        async {
            match method {
                "session.settings" => {
                    let cfg = cfg.lock().map_err(|_| "model configuration unavailable")?;
                    Ok(settings(
                        kernel.provider.as_ref(),
                        session,
                        active,
                        model,
                        &cfg,
                    ))
                }
                "session.configure" => {
                    if running
                        || agents.is_some_and(|control| !control.active().is_empty())
                        || !kernel.executor.background_tasks().is_empty()
                    {
                        return Err(
                            "Finish or stop active work before changing session settings.".into(),
                        );
                    }
                    let reasoning = if params["reasoning"] == "on" {
                        crate::reasoning_on_config(kernel.provider.as_ref())
                    } else {
                        kernel.provider.reasoning()
                    };
                    match change(params, reasoning)? {
                        Change::Profile(name) => {
                            let resolved = {
                                let cfg =
                                    cfg.lock().map_err(|_| "model configuration unavailable")?;
                                config::resolve_model(&cfg, &name)
                                    .map_err(|error| error.to_string())?
                            };
                            kernel.provider.switch_profile(&resolved)?;
                            *model = resolved.provider.model;
                            *active = resolved.name;
                        }
                        Change::Mode(mode) => session.autonomy = mode,
                        Change::Reasoning(reasoning) => kernel
                            .provider
                            .set_reasoning(reasoning)
                            .map_err(|error| error.to_string())?,
                        Change::Streaming(on) => kernel.provider.set_streaming(on),
                    }
                    let cfg = cfg.lock().map_err(|_| "model configuration unavailable")?;
                    Ok(settings(
                        kernel.provider.as_ref(),
                        session,
                        active,
                        model,
                        &cfg,
                    ))
                }
                "agent.control" => {
                    let control = agents.ok_or("Agents are not enabled in this session.")?;
                    let reference = params["agent"].as_str().ok_or("agent reference required")?;
                    let root = orchestrator::AgentPath::root();
                    match params["action"].as_str().ok_or("agent action required")? {
                        "stop" => {
                            control
                                .cancel(&root, reference)
                                .map_err(|error| error.to_string())?;
                        }
                        "steer" => {
                            let text = message(params)?;
                            control
                                .steer(&root, reference, text)
                                .map_err(|error| error.to_string())?;
                        }
                        "followup" => {
                            let text = message(params)?;
                            let mut budget = control.root_budget().with_fresh_pool();
                            budget.max_turns = Some(budget.max_turns.unwrap_or(30).min(30));
                            control
                                .followup(
                                    &orchestrator::Caller::root(session.id),
                                    reference,
                                    text,
                                    Arc::clone(&kernel.executor),
                                    budget,
                                )
                                .await
                                .map_err(|error| error.to_string())?;
                        }
                        _ => return Err("unknown agent action".into()),
                    }
                    Ok(json!({ "accepted": true }))
                }
                "patch.list" => {
                    let Some(control) = agents else {
                        return Ok(json!({ "patches": [] }));
                    };
                    let patches = control.outstanding().await.into_iter().map(|pending| json!({
                    "id": pending.dispatch, "agent": pending.agent, "session": pending.session,
                    "files": pending.patch.files, "diff": pending.patch.diff,
                    "verification": pending.patch.verification,
                })).collect::<Vec<_>>();
                    Ok(json!({ "patches": patches }))
                }
                "patch.apply" => {
                    if running || agents.is_some_and(|control| !control.active().is_empty()) {
                        return Err("Finish or stop active work before applying a patch.".into());
                    }
                    let control = agents.ok_or("Agents are not enabled.")?;
                    let id = params["patch_id"]
                        .as_str()
                        .ok_or("exact patch_id required")?;
                    let pending = control
                        .pending(id)
                        .await
                        .ok_or("This patch is no longer pending.")?;
                    if pending.dispatch != id {
                        return Err("Use the exact patch ID from Changes.".into());
                    }
                    control
                        .merge(&pending.patch, false)
                        .await
                        .map_err(|error| error.to_string())?;
                    control.forget(id).await;
                    Ok(json!({ "applied": id }))
                }
                _ => unreachable!(),
            }
        }
        .await,
    )
}

fn message(params: &Value) -> Result<&str, String> {
    params["text"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| "message required".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_changes_reject_unknown_and_ambiguous_requests() {
        for request in [
            json!({}),
            json!({"mode":"normal", "profile":"other"}),
            json!({"shell":"ls"}),
            json!({"streaming":"yes"}),
            json!({"mode":"oops"}),
        ] {
            assert!(change(&request, kernel::ReasoningConfig::default()).is_err());
        }
    }
    #[test]
    fn reasoning_off_clears_effort_and_auto_preserves_provider_defaults() {
        let current = kernel::ReasoningConfig::from_effort_text("high").unwrap();
        let Change::Reasoning(off) = change(&json!({"reasoning":"off"}), current.clone()).unwrap()
        else {
            panic!()
        };
        assert_eq!(off.enabled, Some(false));
        assert_eq!(off.effort, None);
        let Change::Reasoning(auto) = change(&json!({"effort":"auto"}), current).unwrap() else {
            panic!()
        };
        assert_eq!(auto, kernel::ReasoningConfig::default());
    }
    #[test]
    fn selecting_effort_enables_reasoning_from_auto_or_off() {
        for previous in [
            kernel::ReasoningConfig::default(),
            kernel::ReasoningConfig {
                enabled: Some(false),
                effort: None,
            },
        ] {
            let Change::Reasoning(next) = change(&json!({"effort":"medium"}), previous).unwrap()
            else {
                panic!("expected reasoning change")
            };
            assert_eq!(next.enabled, Some(true));
            assert_eq!(next.effort, Some(kernel::ReasoningEffort::Medium));
        }
    }
    #[test]
    fn profile_list_exposes_no_connection_or_secret_fields() {
        let mut cfg = config::Config::default();
        cfg.models.insert(
            "local".into(),
            providers::ProviderProfile::openai_chat(
                "http://localhost:8000/v1",
                "local",
                providers::AuthKind::None,
            ),
        );
        let row = &profiles(&cfg)[0];
        assert!(row.get("base_url").is_none());
        assert!(row.get("credential").is_none());
    }
}
