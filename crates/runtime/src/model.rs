//! Which model a chat talks to: its profile, its limits and its provider.

use crate::config;
use crate::reasoning::normalize_reasoning_on;
use crate::{Notices, SessionOptions};
use anyhow::Result;
use kernel::Provider;
use providers::ProviderClient;
use std::sync::Arc;

pub struct Model {
    pub provider: Arc<ProviderClient>,
    pub name: String,
    pub catalogue_pricing: Option<(f64, f64, Option<f64>, Option<f64>)>,
    pub profiles: Arc<std::sync::Mutex<config::Config>>,
    pub active_profile: String,
    pub max_ctx: Option<u32>,
    pub open_setup: bool,
    /// The credential the provider was built with, kept out of tool results.
    pub credential: Option<String>,
}

/// One resolution route for startup and live profile changes; deployments can differ.
pub async fn context_window(
    profile: &providers::ProviderProfile,
    credential: &str,
) -> (Option<u32>, &'static str) {
    if let Some(limit) = profile.max_ctx {
        return (Some(limit), "config/env");
    }
    if !profile.base_url.is_empty()
        && matches!(
            profile.protocol,
            kernel::Protocol::OpenAiChat | kernel::Protocol::GeminiInteractions
        )
        && let Ok(models) = providers::discovery::list_models_for_profile(profile, credential).await
        && let Some(limit) = models
            .iter()
            .find(|model| model.id == profile.model)
            .and_then(|model| model.context_length)
    {
        return (Some(limit), "discovered from /v1/models");
    }
    (
        providers::models_dev::profile_context_window(profile).await,
        "models.dev",
    )
}

pub async fn resolve(
    lock: &lockfile::MedhaLock,
    options: &SessionOptions,
    notices: &dyn Notices,
) -> Result<Model> {
    // Headless callers fail instead of hanging on first-run setup.
    let cfg = config::load()?;
    let mut resolved = match config::resolve_with(
        cfg.as_ref(),
        options.base_url.clone(),
        options.model.clone(),
        &options.model_env,
    )? {
        Some(r) => r,
        None if options.may_start_unconfigured => config::Resolved {
            name: String::new(),
            provider: providers::ProviderProfile::openai_chat(
                String::new(),
                String::new(),
                providers::AuthKind::None,
            ),
            credential: String::new(),
            model_source: config::Source::Config,
            base_url_source: config::Source::Config,
            credential_source: config::CredSource::KeychainOrNone,
        },
        None => anyhow::bail!(
            "no model configured. Run `medha` to add one in the TUI, \
             or set MEDHA_BASE_URL / MEDHA_MODEL / MEDHA_API_KEY."
        ),
    };
    let open_setup = options.first_run_setup || resolved.provider.base_url.is_empty();

    // Resolve exact model capability metadata before constructing the provider.
    // A saved profile override wins over models.dev; unknown remains visible so
    // a future media pipeline can choose a safe fallback instead of guessing.
    if !open_setup {
        let capability_resolution =
            providers::models_dev::resolve_profile_capabilities(&resolved.provider).await;
        if resolved.provider.capabilities.is_none() {
            resolved.provider.capabilities = capability_resolution.capabilities;
        }
        providers::models_dev::resolve_reasoning_efforts(&mut resolved.provider).await;
        let capability_summary = resolved
            .provider
            .capabilities
            .as_ref()
            .map(|capabilities| {
                format!(
                    "user images {}, tool-result images {}, image output {}, attachments {}, tool calls {}, reasoning {}",
                    capabilities.user_image_state().as_str(),
                    capabilities.tool_result_image_state().as_str(),
                    capabilities.output_state("image").as_str(),
                    capabilities.attachment_state().as_str(),
                    capabilities.tool_call_state().as_str(),
                    capabilities.reasoning_state().as_str(),
                )
            })
            .unwrap_or_else(|| "capabilities unknown".to_string());
        notices.say(&format!(
            "model capabilities: {capability_summary} ({})",
            capability_resolution.source.as_str()
        ));
        notices.say(&format!(
            "Medha image input: {}",
            if resolved.provider.protocol.carries_images() {
                "images available (requires a vision-capable model and endpoint)"
            } else {
                "not implemented for this protocol"
            }
        ));
    }

    if !options.attach.is_empty() {
        match resolved.provider.image_input {
            kernel::ImageInputMode::Text => notices
                .say("image input mode is text; routing attachments through auxiliary vision"),
            kernel::ImageInputMode::Native => {
                notices.say("image input mode is native; sending attachments to the selected model")
            }
            kernel::ImageInputMode::Auto => {
                let state = resolved
                    .provider
                    .capabilities
                    .as_ref()
                    .map_or(providers::CapabilityState::Unknown, |caps| {
                        caps.user_image_state()
                    });
                if state == providers::CapabilityState::Unknown {
                    notices.say(
                        "image support is unknown; trying native input once, with auxiliary-vision fallback",
                    );
                }
            }
        }
    }

    let model_name = resolved.provider.model.clone();
    let catalogue_pricing = providers::models_dev::profile_pricing(&resolved.provider).await;
    let model_profiles = Arc::new(std::sync::Mutex::new(cfg.unwrap_or_default()));
    let active_profile = resolved.name.clone();

    // Context limits resolve from config, server metadata, then models.dev.
    let (max_ctx, ctx_source) = context_window(&resolved.provider, &resolved.credential).await;
    match max_ctx {
        _ if model_name.is_empty() => {} // nothing configured yet — stay quiet
        Some(n) => {
            notices.say(&format!(
                "context window: {n} tokens ({ctx_source}) — compaction enabled"
            ));
            match resolved.provider.max_output_tokens {
                Some(output) => notices.say(&format!(
                    "  output reservation: {output} tokens; input ceiling: {} tokens",
                    u64::from(n).saturating_sub(output)
                )),
                None => notices.say(
                    "  output allowance: server default (unknown); context ceiling is enforced with proactive compaction",
                ),
            }
            if ctx_source == "models.dev" {
                notices.say(&format!(
                    "  note: that's {model_name}'s spec maximum from models.dev — your deployment \
                     may serve less (e.g. a reduced KV-cache limit). If requests get rejected for \
                     context length, set MEDHA_MAX_CTX to the real value your endpoint serves."
                ));
            }
        }
        None => notices.say(&format!(
            "note: context window unknown for '{model_name}' (not reported by the server \
             or listed in the model catalogue), so compaction waits for the server to refuse a \
             request. Set it in Settings → Models or with MEDHA_MAX_CTX=<tokens>."
        )),
    }

    if !resolved.provider.base_url.is_empty() && !providers::implemented(resolved.provider.protocol)
    {
        anyhow::bail!(
            "protocol '{}' is configured but its native adapter is not implemented yet",
            resolved.provider.protocol.as_str()
        );
    }
    let mut runtime_profile = resolved.provider;
    runtime_profile.max_ctx = max_ctx;
    let credential = Some(resolved.credential.to_string()).filter(|key| !key.is_empty());
    let provider = if open_setup && runtime_profile.base_url.is_empty() {
        // Keep empty endpoints out of normal profile validation.
        ProviderClient::unconfigured()
    } else {
        ProviderClient::from_profile(runtime_profile, resolved.credential)
            .map_err(|error| anyhow::anyhow!(error))?
    };
    // What servers show about their models is kept between sessions.
    let provider = match config::medha_home() {
        Ok(home) => provider.remembering(home.join("learned_models.json")),
        Err(_) => provider,
    };
    // One the user listed that cannot be used is said now, not when it is needed.
    for unusable in stand_ins(&model_profiles.lock().unwrap(), &active_profile).1 {
        notices.say(&unusable);
    }
    // The list is read from the saved settings each time a model fails, so a
    // change made while this chat is open applies to it.
    let (opened_with, own) = (model_profiles.clone(), active_profile.clone());
    let provider = provider.with_fallbacks(move || {
        let saved = config::load().ok().flatten();
        let saved = saved.unwrap_or_else(|| opened_with.lock().unwrap().clone());
        stand_ins(&saved, &own).0
    });
    // A key given for this run alone has no saved ones to turn to. Saved keys
    // are read when one fails, so one added while this chat is open is used.
    let from_store = !matches!(resolved.credential_source, config::CredSource::Env);
    let provider = Arc::new(
        provider.with_keys(move |server| match (from_store, config::load()) {
            (true, Ok(Some(saved))) => crate::key_pool::keys(&saved, server),
            _ => Vec::new(),
        }),
    );

    let reasoning = options
        .reasoning
        .clone()
        .unwrap_or(lock.reasoning.to_config().map_err(anyhow::Error::msg)?);
    let reasoning = normalize_reasoning_on(provider.as_ref(), reasoning);
    if let Err(error) = provider.set_reasoning(reasoning.clone()) {
        if options.reasoning.is_some() {
            return Err(anyhow::anyhow!(
                "reasoning setting was not applied: {error}"
            ));
        }
        notices.say(&format!(
            "note: saved reasoning setting was not applied: {error}"
        ));
    } else if reasoning != kernel::ReasoningConfig::default()
        && provider.reasoning_support() == kernel::ReasoningSupport::Unknown
    {
        notices.say(
            "note: reasoning effort was requested, but this profile marks model support as unverified",
        );
    }
    if let Some(stream) = lock.reasoning.stream {
        provider.set_streaming(stream);
    }
    Ok(Model {
        provider,
        name: model_name,
        catalogue_pricing,
        profiles: model_profiles,
        active_profile,
        max_ctx,
        open_setup,
        credential,
    })
}

/// The saved models that stand in for `own`, in the order to try them, and
/// what to say about each one listed that cannot be used.
fn stand_ins(
    saved: &config::Config,
    own: &str,
) -> (Vec<(providers::ProviderProfile, String)>, Vec<String>) {
    let (mut usable, mut unusable) = (Vec::new(), Vec::new());
    for name in saved.fallback.iter().filter(|name| *name != own) {
        match config::resolve_model_as(saved, name, None) {
            Ok(found) => usable.push((found.provider, found.credential.to_string())),
            Err(error) => unusable.push(format!(
                "stand-in model '{name}' cannot be used and is skipped: {error:#}"
            )),
        }
    }
    (usable, unusable)
}
