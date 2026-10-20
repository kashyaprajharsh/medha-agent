//! Which reasoning settings a profile accepts, checked before anything is sent.

use crate::ProviderProfile;
use crate::protocol::gemini_interactions;
use kernel::{Protocol, ProviderError, ReasoningConfig, ReasoningEffort, ReasoningSupport};

pub(super) fn validate_reasoning(
    profile: &ProviderProfile,
    config: &ReasoningConfig,
) -> Result<(), ProviderError> {
    if config == &ReasoningConfig::default() {
        return Ok(());
    }
    if profile.reasoning == ReasoningSupport::Unsupported {
        return Err(ProviderError::Decode(
            "reasoning controls are disabled by this model profile".into(),
        ));
    }
    let effort = if config.enabled == Some(false) {
        Some(ReasoningEffort::None)
    } else {
        config.effort
    };
    if let Some(effort) = effort
        && !profile_reasoning_efforts(profile).contains(&effort)
    {
        return Err(ProviderError::Decode(format!(
            "reasoning effort '{}' is not supported by this protocol/model profile; available: {}",
            effort.as_str(),
            profile_reasoning_efforts(profile)
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    match profile.protocol {
        Protocol::OpenAiChat => {
            // Compatible servers use `reasoning_effort: "none"` to disable.
            if config.enabled == Some(true) && config.effort.is_none() {
                return Err(ProviderError::Decode(
                    "open-ai-chat cannot enable reasoning without an explicit effort".into(),
                ));
            }
        }
        Protocol::GeminiInteractions => {
            if config.enabled == Some(false) {
                return Err(ProviderError::Decode(
                    "gemini-interactions v1 has no portable thinking-disable control; use server default instead"
                        .into(),
                ));
            }
        }
        Protocol::OpenAiResponses | Protocol::AnthropicMessages => {}
    }
    Ok(())
}

pub(super) fn profile_reasoning_efforts(profile: &ProviderProfile) -> Vec<ReasoningEffort> {
    if profile.reasoning == ReasoningSupport::Unsupported {
        return Vec::new();
    }
    let levels = profile.reasoning_efforts.as_deref().unwrap_or_else(|| {
        if profile.protocol == Protocol::GeminiInteractions {
            gemini_interactions::known_reasoning_efforts(&profile.model).unwrap_or(&[])
        } else {
            ReasoningEffort::ALL
        }
    });
    levels
        .iter()
        .copied()
        .filter(|effort| profile.reasoning_efforts.is_some() || *effort != ReasoningEffort::Ultra)
        .filter(|effort| {
            profile.protocol != Protocol::GeminiInteractions
                || matches!(
                    effort,
                    ReasoningEffort::Minimal
                        | ReasoningEffort::Low
                        | ReasoningEffort::Medium
                        | ReasoningEffort::High
                )
        })
        .collect()
}

/// A carried setting the new profile refuses becomes the server default; `set_reasoning` stays strict.
pub(super) fn reconcile_reasoning_for_switch(
    profile: &ProviderProfile,
    config: ReasoningConfig,
) -> ReasoningConfig {
    if validate_reasoning(profile, &config).is_err() {
        ReasoningConfig::default()
    } else {
        config
    }
}
