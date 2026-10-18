use super::*;

pub(super) fn activate(model: &mut Model) -> bool {
    let Some(picker) = model.picker.as_mut() else {
        return false;
    };
    // Action rows precede installed skills; keep indexing in one place.
    if let PickerKind::Skill(skills) = &picker.kind {
        let sel = picker.selected;
        let n_actions = SKILL_HUB_ACTIONS.len();
        let action = (sel < n_actions).then(|| SKILL_HUB_ACTIONS[sel].1);
        let skill_name = skills
            .get(sel.wrapping_sub(n_actions))
            .map(|(n, _)| n.clone());
        model.picker = None;
        match action {
            Some("add") => search_skills(model, ""),
            Some("manage") => {
                model.picker = Some(Picker::new(PickerKind::SkillManage));
            }
            _ => {
                if let Some(name) = skill_name {
                    // One left off at install is offered to be switched on; it cannot be used as it is.
                    if skill_is_off(model, &name) {
                        begin_enable_skill(model, &name);
                    } else {
                        load_skill_by_name(model, &name);
                    }
                }
            }
        }
        return true;
    }
    if let PickerKind::EnableSkill(name) = &picker.kind {
        let name = name.clone();
        let confirmed = picker.selected == 1;
        model.picker = None;
        if confirmed {
            set_skill_enabled(model, &name, true);
        } else {
            open_skill_picker(model);
        }
        return true;
    }
    if matches!(&picker.kind, PickerKind::SkillManage) {
        let id = SKILL_MANAGE_ACTIONS.get(picker.selected).map(|(_, id)| *id);
        model.picker = None;
        match id {
            Some("update") => update_skills(model, ""),
            Some("sources") => open_sources_picker(model),
            Some("lock") => lock_skills(model),
            Some("sync") => sync_skills(model),
            _ => open_skill_picker(model), // Back (or anything unknown)
        }
        return true;
    }
    if let PickerKind::SkillSources(sources) = &picker.kind {
        let sel = picker.selected;
        let n = sources.len();
        let chosen = (sel >= 1 && sel <= n).then(|| sources[sel - 1].clone());
        model.picker = None;
        if sel == 0 {
            prefill_command(
                model,
                "/skill sources add ",
                "type owner/repo (e.g. anthropics/skills), then Enter",
            );
        } else if let Some((repo, path, removable)) = chosen {
            if removable {
                remove_source(model, &format!("{repo}/{path}"));
            } else {
                hub_notice(model, format!("{repo} is built-in — always available"));
            }
            open_sources_picker(model); // reopen with the updated list
        } else {
            // "← Back" (or past the end) → back to Manage.
            model.picker = Some(Picker::new(PickerKind::SkillManage));
        }
        return true;
    }
    if let PickerKind::RemoveSkill(name) = &picker.kind {
        let name = name.clone();
        let confirmed = picker.selected == 1;
        model.picker = None;
        if confirmed {
            remove_user_skill(model, &name);
        } else {
            open_skill_picker(model);
        }
        return true;
    }
    // Add-a-skill picker: row 0 installs from a pasted link; every
    // other row is a catalog skill → install it (guard-gated).
    if let PickerKind::SkillSearch(hits) = &picker.kind {
        let sel = picker.selected;
        let url = (sel >= 1)
            .then(|| hits.get(sel - 1).map(|h| h.install_url.clone()))
            .flatten();
        model.picker = None;
        if sel == 0 {
            prefill_command(
                model,
                "/skill add ",
                "paste a GitHub link, or a local folder / SKILL.md path — then Enter",
            );
        } else if let Some(url) = url {
            install_skill(model, &url);
        }
        return true;
    }
    false
}
