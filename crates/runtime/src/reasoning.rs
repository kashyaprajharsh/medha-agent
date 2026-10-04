//! Turning a saved reasoning setting into one the endpoint can use.

/// Enabling reasoning chooses a supported explicit level so Chat endpoints
/// receive a usable request. Existing effort is preserved.
pub fn normalize_reasoning_on<P: kernel::Provider>(
    provider: &P,
    config: kernel::ReasoningConfig,
) -> kernel::ReasoningConfig {
    if config.enabled == Some(true) && config.effort.is_none() {
        reasoning_on_config(provider)
    } else {
        config
    }
}

pub fn reasoning_on_config<P: kernel::Provider>(provider: &P) -> kernel::ReasoningConfig {
    let levels = provider.reasoning_efforts();
    let effort = provider
        .reasoning()
        .effort
        .filter(|e| *e != kernel::ReasoningEffort::None)
        .or_else(|| {
            levels
                .contains(&kernel::ReasoningEffort::Medium)
                .then_some(kernel::ReasoningEffort::Medium)
        })
        .or_else(|| {
            levels
                .into_iter()
                .find(|e| *e != kernel::ReasoningEffort::None)
        });
    kernel::ReasoningConfig {
        enabled: Some(true),
        effort,
    }
}
