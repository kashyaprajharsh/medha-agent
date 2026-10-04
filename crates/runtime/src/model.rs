//! Which model a chat talks to: its profile, its limits and its provider.

use crate::config;
use crate::reasoning::normalize_reasoning_on;
use crate::{Notices, SessionOptions};
use anyhow::Result;
use kernel::Provider;
use providers::OpenAiCompat;
use std::sync::Arc;

pub struct Model {
    pub provider: Arc<OpenAiCompat>,
    pub name: String,
    pub profiles: Arc<std::sync::Mutex<config::Config>>,
    pub active_profile: String,
    pub max_ctx: Option<u32>,
    pub open_setup: bool,
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
        let capability_resolution = providers::models_dev::resolve_capabilities(
            &resolved.provider.model,
            resolved.provider.capabilities.as_ref(),
        )
        .await;
        if resolved.provider.capabilities.is_none() {
            resolved.provider.capabilities = capability_resolution.capabilities;
        }
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
    let model_profiles = Arc::new(std::sync::Mutex::new(cfg.unwrap_or_default()));
    let active_profile = resolved.name.clone();

    // Context limits resolve from config, server metadata, then models.dev.
    let (mut max_ctx, mut ctx_source) = (resolved.provider.max_ctx, "config/env");
    if max_ctx.is_none()
        && !resolved.provider.base_url.is_empty()
        && matches!(
            resolved.provider.protocol,
            kernel::Protocol::OpenAiChat | kernel::Protocol::GeminiInteractions
        )
        && let Ok(models) = providers::openai_compat::list_models_for_profile(
            &resolved.provider,
            &resolved.credential,
        )
        .await
        && let Some(c) = models
            .iter()
            .find(|m| m.id == model_name)
            .and_then(|m| m.context_length)
    {
        max_ctx = Some(c);
        ctx_source = "discovered from /v1/models";
    }
    if max_ctx.is_none()
        && !model_name.is_empty()
        && let Some(c) = providers::models_dev::context_window(&model_name).await
    {
        max_ctx = Some(c);
        ctx_source = "models.dev";
    }
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
             or found on models.dev) — compaction disabled. Set MEDHA_MAX_CTX=<tokens> to enable it."
        )),
    }

    if !resolved.provider.base_url.is_empty()
        && !matches!(
            resolved.provider.protocol,
            kernel::Protocol::OpenAiChat | kernel::Protocol::GeminiInteractions
        )
    {
        anyhow::bail!(
            "protocol '{}' is configured but its native adapter is not implemented yet",
            resolved.provider.protocol.as_str()
        );
    }
    let mut runtime_profile = resolved.provider;
    runtime_profile.max_ctx = max_ctx;
    let provider = if open_setup && runtime_profile.base_url.is_empty() {
        // Keep empty endpoints out of normal profile validation.
        OpenAiCompat::unconfigured()
    } else {
        OpenAiCompat::from_profile(runtime_profile, resolved.credential)
            .map_err(|error| anyhow::anyhow!(error))?
    };
    let provider = Arc::new(provider);

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
        profiles: model_profiles,
        active_profile,
        max_ctx,
        open_setup,
    })
}
