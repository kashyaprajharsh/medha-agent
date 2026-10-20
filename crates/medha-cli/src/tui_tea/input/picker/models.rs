use super::*;

pub(super) fn activate(model: &mut Model) -> bool {
    let Some(picker) = model.picker.as_mut() else {
        return false;
    };
    if matches!(picker.kind, PickerKind::ModelProtocol) {
        // Resolve through the same table that produced the rows.
        let Some(&(label, available, protocol)) = MODEL_PROTOCOLS.get(picker.selected) else {
            return true;
        };
        if !available {
            model.push_notice(format!(
                "{label} is not wired up yet — pick an available protocol for now"
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
    if let PickerKind::ProviderPreset {
        saved,
        providers,
        query,
    } = &picker.kind
    {
        let selected = picker.selected;
        let saved = saved_deployment_matches(saved, query);
        if let Some(saved) = saved.get(selected).cloned().cloned() {
            let protocol = backend_features::kernel_protocol(saved.protocol);
            if !matches!(
                protocol,
                kernel::Protocol::OpenAiChat | kernel::Protocol::GeminiInteractions
            ) {
                model.push_notice("This saved deployment's protocol is not available yet.");
                return true;
            }
            let provider = saved
                .provider_id
                .as_ref()
                .and_then(|id| providers.iter().find(|p| &p.id == id))
                .cloned();
            model.picker = None;
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
            return true;
        }
        let selected = selected.saturating_sub(saved.len());
        let provider = provider_matches(providers, query)
            .get(selected)
            .map(|p| (*p).clone());
        if let Some(provider) = &provider
            && !provider.available
        {
            model.push_notice(
                provider
                    .unavailable_reason
                    .clone()
                    .unwrap_or_else(|| "This provider is not available yet.".into()),
            );
            return true;
        }
        model.picker = None;
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
                model.picker = Some(Picker::new(PickerKind::ModelProtocol));
                model.push_notice("Custom provider selected — enter its base URL");
            }
        }
        backend_features::discover_setup_if_ready(model);
        return true;
    }
    false
}
