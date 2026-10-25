use super::*;

pub(super) fn activate(model: &mut Model) -> bool {
    let Some(picker) = model.picker.as_mut() else {
        return false;
    };
    if let PickerKind::Update(latest) = &picker.kind {
        let (latest, selected) = (latest.clone(), picker.selected);
        model.picker = None;
        match selected {
            // The installer runs once the TUI has closed and the terminal is back to normal.
            0 => {
                crate::self_update::request(&latest);
                model.should_quit = true;
            }
            2 => crate::self_update::skip(&latest),
            _ => {}
        }
        return true;
    }
    if matches!(picker.kind, PickerKind::AutonomyMode) {
        let selected = picker.selected;
        model.picker = None;
        if let Some((level, _)) = AUTONOMY_MODES.get(selected).copied() {
            set_autonomy(model, level);
        }
        return true;
    }
    if matches!(picker.kind, PickerKind::Theme) {
        let selected = picker.selected;
        model.picker = None;
        if let Some((id, _)) = super::theme::modes().get(selected).copied() {
            set_theme(model, id);
        }
        return true;
    }
    // `/search` step 1: a provider was chosen. DuckDuckGo needs
    // nothing more and commits here; the others advance to the key
    // (Tavily/Brave) or URL (SearXNG) input.
    if matches!(picker.kind, PickerKind::SearchProvider) {
        let selected = picker.selected;
        model.picker = None;
        let Some((provider, _)) = SEARCH_PROVIDERS.get(selected).copied() else {
            model.search_setup = None;
            return true;
        };
        if provider == tools::SearchProvider::DuckDuckGo {
            commit_search(model, provider, None);
        } else if let Some(setup) = model.search_setup.as_mut() {
            setup.provider = provider;
            setup.step = SearchSetupStep::Secret;
            model.push_notice(match provider {
                tools::SearchProvider::Searxng => {
                    "SearXNG selected — enter its instance URL".to_string()
                }
                _ => format!("{} selected — enter the API key", provider.label()),
            });
        }
        return true;
    }
    false
}
