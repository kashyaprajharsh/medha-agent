use super::*;

pub(super) fn activate(model: &mut Model) -> bool {
    let Some(picker) = model.picker.as_mut() else {
        return false;
    };
    if let PickerKind::RemoveModel(name) = &picker.kind {
        let name = name.clone();
        let confirmed = picker.selected == 1;
        model.picker = None;
        if confirmed {
            remove_saved_model(model, &name);
        } else {
            open_model_picker(model);
        }
        return true;
    }
    if let PickerKind::ModelCredential(profiles) = &picker.kind {
        let profile = profiles.get(picker.selected).cloned();
        model.picker = None;
        if let Some(profile) = profile {
            begin_model_key_update(model, profile);
        }
        return true;
    }
    if let PickerKind::ModelDefault(profiles) = &picker.kind {
        let name = profiles.get(picker.selected).map(|p| p.name.clone());
        model.picker = None;
        if let Some(name) = name {
            set_default_model(model, &name);
        }
        return true;
    }
    if let PickerKind::ModelRemove(profiles) = &picker.kind {
        let name = profiles.get(picker.selected).map(|p| p.name.clone());
        if let Some(name) = name {
            if name == model.active_profile {
                model.picker = None;
                model.push_notice("switch to another model before removing the active profile");
            } else {
                model.picker = Some(Picker::new(PickerKind::RemoveModel(name)));
            }
        }
        return true;
    }
    if let PickerKind::ModelKeys(profiles) = &picker.kind {
        let name = profiles.get(picker.selected).map(|p| p.name.clone());
        model.picker = None;
        if let Some(name) = name {
            open_model_keys(model, name);
        }
        return true;
    }
    if let PickerKind::ModelCertificate(profiles) = &picker.kind {
        let name = profiles.get(picker.selected).map(|p| p.name.clone());
        model.picker = None;
        if let Some(name) = name {
            begin_model_certificate(model, &name);
        }
        return true;
    }
    if let PickerKind::ModelKeySlots { name, slots } = &picker.kind {
        // The first row adds a key; each row after it forgets one.
        let (name, slot) = (name.clone(), picker.selected.checked_sub(1));
        let slot = slot.map(|at| slots.get(at).copied());
        model.picker = None;
        match slot {
            None => begin_model_key_add(model, &name),
            Some(Some(slot)) => remove_model_key(model, &name, slot),
            Some(None) => {}
        }
        return true;
    }
    if let PickerKind::ModelFallback { profiles, listed } = &picker.kind {
        // A listed model is taken out, another goes last; the row after them clears.
        let mut names = listed.clone();
        match profiles.get(picker.selected) {
            Some(profile) => match names.iter().position(|n| *n == profile.name) {
                Some(at) => drop(names.remove(at)),
                None => names.push(profile.name.clone()),
            },
            None => names.clear(),
        }
        set_fallback(model, names);
        return true;
    }
    if let PickerKind::Model { profiles, active } = &picker.kind {
        // Rows 0..n are the models themselves — Enter switches
        // directly. Management actions sit after the list.
        if let Some(profile) = profiles.get(picker.selected) {
            let name = profile.name.clone();
            model.picker = None;
            switch_saved_model(model, &name);
            return true;
        }
        match picker.selected - profiles.len() {
            0 => {
                model.picker = None;
                begin_model_setup(model);
            }
            1 => {
                model.picker = Some(Picker::new(PickerKind::ModelCredential(profiles.clone())));
            }
            2 => {
                model.picker = Some(Picker::new(PickerKind::ModelDefault(profiles.clone())));
            }
            3 => {
                // Any model but the one in use can be removed.
                let removable: Vec<_> = profiles
                    .iter()
                    .filter(|p| p.name != *active)
                    .cloned()
                    .collect();
                if removable.is_empty() {
                    model.upsert_notice(
                        "model manager:",
                        "model manager: only the active model is saved — add another before removing."
                            .to_string(),
                    );
                } else {
                    model.picker = Some(Picker::new(PickerKind::ModelRemove(removable)));
                }
            }
            4 => {
                model.picker = None;
                open_fallback_picker(model);
            }
            5 => {
                model.picker = Some(Picker::new(PickerKind::ModelKeys(profiles.clone())));
            }
            6 => {
                let choose = PickerKind::ModelCertificate(profiles.clone());
                model.picker = Some(Picker::new(choose));
            }
            7 => {
                model.picker = None;
                connect::open_accounts(model);
            }
            _ => {}
        }
        return true;
    }
    false
}
