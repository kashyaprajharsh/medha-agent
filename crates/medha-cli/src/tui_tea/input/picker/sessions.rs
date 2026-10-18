use super::*;

pub(super) fn activate(model: &mut Model) -> bool {
    let Some(picker) = model.picker.as_mut() else {
        return false;
    };
    if let PickerKind::Session(sessions) = &picker.kind
        && let Some(meta) = sessions.get(picker.selected)
    {
        let id = meta.id;
        if model.foreground_owned() || model.has_active_agents() {
            model.picker = None;
            model.push_notice("finish foreground and background work before resuming a session");
            return true;
        }
        resume(model, id.to_string());
        model.picker = None;
        model.push_notice(format!("(loading session {id} …)"));
        return true;
    }
    if let PickerKind::Rewind(points) = &picker.kind
        && let Some(point) = points.get(picker.selected).cloned()
    {
        model.picker = Some(Picker::new(PickerKind::RewindMode(point)));
        return true;
    }
    if let PickerKind::RewindMode(point) = &picker.kind {
        let scope = point
            .scope_options()
            .get(picker.selected)
            .and_then(|(_, s)| *s);
        let at_event = point.at_event; // Copy — ends the picker borrow
        match scope {
            Some(scope) => {
                if model.foreground_owned() || model.has_active_agents() {
                    model.picker = None;
                    model.push_notice("finish foreground and background work before rewinding");
                    return true;
                }
                model.picker = None;
                rewind(model, at_event.to_string(), scope);
                model.push_notice("(rewinding …)");
            }
            None => model.picker = None,
        }
        return true;
    }
    if let PickerKind::Memory(entries) = &picker.kind {
        if let Some(entry) = entries.get(picker.selected).cloned() {
            let name = entry.name.clone();
            model.picker = None;
            spawn_memory_provenance(model, entry);
            model.push_notice(format!("(opening memory '{name}' provenance …)"));
        }
        return true;
    }
    false
}
