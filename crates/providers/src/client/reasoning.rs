//! Which reasoning settings a profile accepts, checked before anything is sent.

use crate::ProviderProfile;
use crate::protocol::{self, requested_effort};
use kernel::{ProviderError, ReasoningConfig, ReasoningEffort, ReasoningSupport};

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
    if let Some(effort) = requested_effort(config)
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
    protocol::wire(profile.protocol).map_or(Ok(()), |wire| wire.check_reasoning(config))
}

pub(super) fn profile_reasoning_efforts(profile: &ProviderProfile) -> Vec<ReasoningEffort> {
    if profile.reasoning == ReasoningSupport::Unsupported {
        return Vec::new();
    }
    let wire = protocol::wire(profile.protocol).ok();
    let levels = profile.reasoning_efforts.as_deref().unwrap_or_else(|| {
        wire.map_or(ReasoningEffort::ALL, |wire| {
            wire.default_efforts(&profile.model)
        })
    });
    levels
        .iter()
        .copied()
        .filter(|effort| profile.reasoning_efforts.is_some() || *effort != ReasoningEffort::Ultra)
        .filter(|effort| wire.is_none_or(|wire| wire.carries(*effort)))
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
