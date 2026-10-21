use super::*;

pub(super) fn activate(model: &mut Model) -> bool {
    let Some(picker) = model.picker.as_mut() else {
        return false;
    };
    if matches!(picker.kind, PickerKind::ModelProtocol) {
        // Resolve through the same table that produced the rows.
        let Some(&(label, protocol)) = MODEL_PROTOCOLS.get(picker.selected) else {
            return true;
        };
        if !providers::implemented(protocol) {
            model.push_notice(format!(
                "{label} is not available yet. Pick an available protocol for now."
            ));
            return true;
        }
        if let Some(setup) = model.model_setup.as_mut() {
            setup.protocol = protocol;
            setup.step = ModelSetupStep::BaseUrl;
        }
        model.picker = None;
        return true;
    }
    false
}

/// Set the setup on a saved model's server, then look for its models when nothing is missing.
pub(in crate::tui_tea) fn choose_saved(
    model: &mut Model,
    saved: protocol::ModelProfile,
    providers: &[protocol::ModelProvider],
) {
    let protocol = backend_features::kernel_protocol(saved.protocol);
    if !matches!(
        protocol,
        kernel::Protocol::OpenAiChat | kernel::Protocol::GeminiInteractions
    ) {
        model.push_notice("This saved deployment's protocol is not available yet.");
        return;
    }
    let provider = saved
        .provider_id
        .as_ref()
        .and_then(|id| providers.iter().find(|p| &p.id == id))
        .cloned();
    if let Some(setup) = model.model_setup.as_mut() {
        setup.protocol = protocol;
        setup.auth = saved.auth.or(Some(if !saved.requires_key {
            protocol::ModelAuth::None
        } else if protocol == kernel::Protocol::GeminiInteractions {
            protocol::ModelAuth::XGoogApiKey
        } else {
            protocol::ModelAuth::Bearer
        }));
        setup.provider_id = saved.provider_id;
        setup.connection = saved.connection;
        setup.base_url = saved.base_url;
        setup.provider = provider;
        setup.step = ModelSetupStep::ApiKey;
    }
    backend_features::discover_setup_if_ready(model);
}

/// Set the setup on a catalogue provider, or on a server of one's own when there is none.
/// Returns whether the setup moved on; an unavailable provider only explains why not.
pub(in crate::tui_tea) fn choose_provider(
    model: &mut Model,
    provider: Option<protocol::ModelProvider>,
) -> bool {
    if let Some(provider) = &provider
        && !provider.available
    {
        model.push_notice(
            provider
                .unavailable_reason
                .clone()
                .unwrap_or_else(|| "This provider is not available yet.".into()),
        );
        return false;
    }
    if let Some(setup) = model.model_setup.as_mut() {
        if let Some(provider) = provider {
            if let Some(protocol) = provider.protocol {
                setup.protocol = backend_features::kernel_protocol(protocol);
            }
            setup.auth = Some(provider.auth);
            setup.provider_id = Some(provider.id.clone());
            setup.base_url = provider.url.clone().unwrap_or_default();
            setup.step = if provider.parameters.is_empty() {
                ModelSetupStep::ApiKey
            } else {
                ModelSetupStep::EndpointParameter
            };
            setup.provider = Some(provider);
        } else {
            setup.provider = None;
            setup.provider_id = None;
            setup.auth = None;
            setup.step = ModelSetupStep::BaseUrl;
            // What the server speaks is chosen in the card first.
            model.picker = Some(Picker::new(PickerKind::ModelProtocol));
        }
    }
    backend_features::discover_setup_if_ready(model);
    true
}
