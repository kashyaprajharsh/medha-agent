use super::*;

mod connections;
pub(in crate::tui_tea) mod models;
mod saved_models;
mod sessions;
mod settings;
mod skills;

pub(super) fn handle(model: &mut Model, key: KeyEvent) -> bool {
    if let Some(picker) = model.picker.as_mut() {
        let labels = picker.kind.labels();
        match key.code {
            KeyCode::Up => {
                picker.selected = picker
                    .selected
                    .checked_sub(1)
                    .unwrap_or(labels.len().saturating_sub(1))
            }
            KeyCode::Down => picker.selected = (picker.selected + 1) % labels.len().max(1),
            // Space switches the selected skill off or on, and leaves it installed.
            KeyCode::Char(' ') if matches!(&picker.kind, PickerKind::Skill(_)) => {
                let name = if let PickerKind::Skill(skills) = &picker.kind {
                    let row = picker.selected.checked_sub(SKILL_HUB_ACTIONS.len());
                    row.and_then(|row| skills.get(row))
                        .map(|(name, _)| name.clone())
                } else {
                    None
                };
                if let Some(name) = name {
                    toggle_skill(model, &name);
                }
                return true;
            }
            // Space switches the selected server on or off without removing it,
            // so a parked server keeps its definition and credentials.
            KeyCode::Char(' ') if matches!(&picker.kind, PickerKind::Mcp(_)) => {
                let row = if let PickerKind::Mcp(rows) = &picker.kind {
                    (picker.selected > 0)
                        .then(|| rows.get(picker.selected - 1).cloned())
                        .flatten()
                } else {
                    None
                };
                if let Some(row) = row {
                    mcp_set_disabled(model, &row.id, !row.disabled);
                }
                return true;
            }
            // Space toggles one tool's exposure inside the tool browser.
            KeyCode::Char(' ') if matches!(&picker.kind, PickerKind::McpTools { .. }) => {
                if let PickerKind::McpTools { id, tools } = &picker.kind
                    && let Some((name, on)) = tools.get(picker.selected)
                {
                    let (id, name, on) = (id.clone(), name.clone(), *on);
                    mcp_set_tool(model, &id, &name, !on);
                }
                return true;
            }
            // Space or Enter switches the selected tool for the scope shown; Enter on Back closes.
            KeyCode::Char(' ') | KeyCode::Enter
                if matches!(&picker.kind, PickerKind::Tools { .. }) =>
            {
                if let PickerKind::Tools { scope, tools } = &picker.kind {
                    match tools.get(picker.selected).cloned() {
                        Some(tool) => {
                            let scope = *scope;
                            flip(model, scope, &tool);
                        }
                        None if key.code == KeyCode::Enter => model.picker = None,
                        None => {}
                    }
                }
                return true;
            }
            // Tab changes which settings the switches change; what is off, and why, stays as it is.
            KeyCode::Tab => {
                if let PickerKind::Tools { scope, .. } = &mut picker.kind {
                    *scope = match scope {
                        protocol::ToolScope::User => protocol::ToolScope::Project,
                        protocol::ToolScope::Project => protocol::ToolScope::User,
                    };
                    return true;
                }
            }
            // `t` opens the selected server's catalogue.
            KeyCode::Char('t') if matches!(&picker.kind, PickerKind::Mcp(_)) => {
                let id = if let PickerKind::Mcp(rows) = &picker.kind {
                    (picker.selected > 0)
                        .then(|| rows.get(picker.selected - 1).map(|row| row.id.clone()))
                        .flatten()
                } else {
                    None
                };
                if let Some(id) = id {
                    open_mcp_tools(model, &id);
                }
                return true;
            }
            KeyCode::Char('d') if matches!(&picker.kind, PickerKind::Agents(_)) => {
                let session = match &picker.kind {
                    PickerKind::Agents(rows) => match rows.get(picker.selected) {
                        Some(AgentRow::Agent { agent, .. }) if agent.is_running() => {
                            Some(agent.session.clone())
                        }
                        _ => None,
                    },
                    _ => None,
                };
                match session {
                    Some(session) => agents_stop(model, &session),
                    None => model.push_notice("that agent has already finished"),
                }
                return true;
            }
            // Uppercase `A` deliberately overrides only verification failure.
            KeyCode::Char(key @ ('a' | 'A')) if matches!(&picker.kind, PickerKind::Agents(_)) => {
                let dispatch = match &picker.kind {
                    PickerKind::Agents(rows) => match rows.get(picker.selected) {
                        Some(AgentRow::Patch { dispatch, .. }) => Some(dispatch.clone()),
                        _ => None,
                    },
                    _ => None,
                };
                match dispatch {
                    Some(dispatch) => agents_apply_patch(model, &dispatch, key == 'A'),
                    None => model.push_notice(
                        "nothing to apply on this row — press Enter to watch what it is doing",
                    ),
                }
                return true;
            }
            // Viewing the diff stays distinct from applying it.
            KeyCode::Enter | KeyCode::Right if matches!(&picker.kind, PickerKind::Agents(_)) => {
                let dispatch = match &picker.kind {
                    PickerKind::Agents(rows) => match rows.get(picker.selected) {
                        Some(AgentRow::Patch { dispatch, .. }) => Some(dispatch.clone()),
                        _ => None,
                    },
                    _ => None,
                };
                match dispatch {
                    Some(dispatch) => agents_view_patch(model, &dispatch),
                    None => {
                        let watching = match &picker.kind {
                            PickerKind::Agents(rows) => match rows.get(picker.selected) {
                                Some(AgentRow::Agent { agent, .. }) => Some(agent.session.clone()),
                                _ => None,
                            },
                            _ => None,
                        };
                        match watching {
                            Some(session) => agents_view_transcript(model, &session),
                            None => model.picker = None,
                        }
                    }
                }
                return true;
            }
            KeyCode::Char('d') if matches!(&picker.kind, PickerKind::Mcp(_)) => {
                let id = if let PickerKind::Mcp(rows) = &picker.kind {
                    (picker.selected > 0)
                        .then(|| rows.get(picker.selected - 1).map(|row| row.id.clone()))
                        .flatten()
                } else {
                    None
                };
                if let Some(id) = id {
                    mcp_remove(model, &id);
                }
                return true;
            }
            // → mirrors Enter and ← mirrors Esc, so the pickers navigate like
            // a nested menu: right descends, left backs out one level.
            KeyCode::Enter | KeyCode::Right => {
                if models::activate(model)
                    || settings::activate(model)
                    || sessions::activate(model)
                    || connections::activate(model)
                    || skills::activate(model)
                    || saved_models::activate(model)
                {
                    return true;
                }
            }
            KeyCode::Esc | KeyCode::Left => {
                if matches!(picker.kind, PickerKind::ModelProtocol) {
                    // Back out of "add model" to the model menu, not to nothing —
                    // ← behaves like a menu's back button. On a fresh install
                    // there is no menu to go back to (the menu would immediately
                    // reopen this form — an Esc trap), so just close.
                    model.model_setup = None;
                    model.form_generation = model.form_generation.wrapping_add(1);
                    model.picker = None;
                    let has_models = model
                        .remote
                        .as_ref()
                        .is_some_and(|peer| !peer.models.profiles.is_empty());
                    if has_models {
                        open_model_picker(model);
                    } else {
                        model.push_notice("model setup cancelled — /model reopens it");
                    }
                } else if matches!(
                    picker.kind,
                    PickerKind::RemoveModel(_)
                        | PickerKind::ModelCredential(_)
                        | PickerKind::ModelDefault(_)
                        | PickerKind::ModelRemove(_)
                        | PickerKind::ModelKeys(_)
                        | PickerKind::ModelCertificate(_)
                        | PickerKind::ModelKeySlots { .. }
                ) {
                    open_model_picker(model);
                } else if matches!(picker.kind, PickerKind::SearchProvider) {
                    model.picker = None;
                    model.search_setup = None;
                    model.form_generation = model.form_generation.wrapping_add(1);
                    model.push_notice("web-search setup cancelled — /search reopens it");
                } else {
                    model.picker = None;
                }
            }
            _ => {}
        }
        return true;
    }
    false
}
