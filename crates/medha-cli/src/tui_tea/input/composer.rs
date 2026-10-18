use super::*;

pub(super) fn switch_agent(model: &mut Model, key: KeyEvent) -> bool {
    // The agent switcher. Handled before the input so its keys are unambiguous
    // while it is open, and it only opens when there is somewhere to go.
    if model.switching {
        match key.code {
            KeyCode::Esc | KeyCode::Tab => {
                model.switching = false;
                return true;
            }
            KeyCode::Enter => {
                let rows = model.switch_rows();
                let Some(target) = rows.get(model.switch_cursor).cloned() else {
                    model.reconcile_switch_cursor(None);
                    return true;
                };
                model.focus_pane(target);
                model.switching = false;
                return true;
            }
            // Stop the agent under the cursor. Never `main` — the conversation is
            // not something you stop, and Esc already interrupts a turn.
            KeyCode::Char('x') => {
                let rows = model.switch_rows();
                match rows.get(model.switch_cursor).cloned().flatten() {
                    Some(path) => agents_stop_path(model, &path),
                    None => model.push_notice("that is the conversation — Esc interrupts a turn"),
                }
                return true;
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                stop_every_agent(model);
                return true;
            }
            _ => {}
        }
    }
    // Opening it needs somewhere to go, so it never steals Tab from nothing.
    // The destination set includes parked settled panes, not only live agents.
    if key.code == KeyCode::Tab
        && model.input.is_empty()
        && model.switch_rows().len() > 1
        && model.picker.is_none()
    {
        model.switching = true;
        model.switched_before = true;
        let rows = model.switch_rows();
        // Start on what is displayed, so Enter without moving is a no-op rather
        // than a jump to whatever happened to be first.
        model.switch_cursor = rows
            .iter()
            .position(|row| row == &model.focus)
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        return true;
    }
    false
}

pub(super) fn autocomplete(model: &mut Model, key: KeyEvent) -> bool {
    // Autocomplete handling
    if model.input.starts_with('/') {
        let matches = command_matches(model);
        let chosen_plugin_command = matches
            .get(model.ac_sel.min(matches.len().saturating_sub(1)))
            .filter(|(name, _)| {
                model
                    .remote
                    .as_ref()
                    .expect("backend UI")
                    .plugins
                    .commands
                    .iter()
                    .any(|c| &c.name == name)
            })
            .map(|(name, _)| name.clone());
        if !matches.is_empty() {
            model.ac_sel = model.ac_sel.min(matches.len() - 1);
            match key.code {
                KeyCode::Up => {
                    model.ac_sel = model.ac_sel.checked_sub(1).unwrap_or(matches.len() - 1);
                    return true;
                }
                KeyCode::Down => {
                    model.ac_sel = (model.ac_sel + 1) % matches.len();
                    return true;
                }
                KeyCode::Tab => {
                    model.input = format!("{} ", matches[model.ac_sel].0);
                    model.cursor = model.input.len();
                    return true;
                }
                // A plugin command sends its prompt through the normal Enter path.
                KeyCode::Enter
                    if chosen_plugin_command.is_some()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    let name = chosen_plugin_command.unwrap_or_default();
                    if !model.input.trim_start().starts_with(&name) {
                        model.input = name;
                        model.cursor = model.input.len();
                    }
                }
                KeyCode::Enter
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    let cmd = matches[model.ac_sel].0.trim_start_matches('/').to_string();
                    model.input.clear();
                    model.cursor = 0;
                    model.ac_sel = 0;
                    dispatch_slash(model, &cmd);
                    return true;
                }
                _ => {}
            }
        }
    }
    false
}

pub(super) fn edit(model: &mut Model, key: KeyEvent) {
    match key.code {
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            stage_images(model, ImageSource::Clipboard);
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !model.running {
                model.input.clear();
                model.cursor = 0;
            }
        }
        // ^E expands/collapses the compaction summary cards.
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            model.show_summary = !model.show_summary;
            model.invalidate_all_renders();
            model.push_notice(if model.show_summary {
                "summaries: expanded (^E)"
            } else {
                "summaries: collapsed (^E)"
            });
        }
        KeyCode::Esc if model.running => {
            // Unreachable in practice (the top-of-handler intercept fires
            // first) — kept as a safety arm with the same graceful path.
            cancel_foreground(model);
        }
        // Modified Enter and Ctrl-J insert a newline.
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            model.insert_char('\n');
        }
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            model.insert_char('\n');
        }
        KeyCode::Enter => submit(model),
        KeyCode::Backspace => model.backspace(),
        KeyCode::Left => model.move_left(),
        KeyCode::Right => model.move_right(),
        // While the switcher holds the arrows it owns them outright — the input's
        // history and the transcript's scrolling both already claim bare Up/Down,
        // which is why entering the region is explicit rather than implied.
        KeyCode::Up if model.switching => {
            let rows = model.switch_rows().len();
            model.switch_cursor = model
                .switch_cursor
                .saturating_sub(1)
                .min(rows.saturating_sub(1));
        }
        KeyCode::Down if model.switching => {
            let rows = model.switch_rows().len();
            model.switch_cursor = (model.switch_cursor + 1).min(rows.saturating_sub(1));
        }
        // Scroll with Up/Down when input empty
        KeyCode::Up if model.input.is_empty() => model.scroll_by(-1),
        KeyCode::Down if model.input.is_empty() => model.scroll_by(1),
        KeyCode::Up => {
            if !model.history.is_empty() {
                let idx = model
                    .history_idx
                    .map(|i| i.saturating_sub(1))
                    .unwrap_or(model.history.len() - 1);
                model.input = model.history[idx].clone();
                model.cursor = model.input.len();
                model.history_idx = Some(idx);
            }
        }
        KeyCode::Down => {
            if let Some(idx) = model.history_idx {
                if idx + 1 < model.history.len() {
                    model.history_idx = Some(idx + 1);
                    model.input = model.history[idx + 1].clone();
                } else {
                    model.history_idx = None;
                    model.input.clear();
                }
                model.cursor = model.input.len();
            }
        }
        KeyCode::PageUp => model.scroll_by(-5),
        KeyCode::PageDown => model.scroll_by(5),
        KeyCode::Home => model.scroll_to_top(),
        KeyCode::End => model.scroll_to_bottom(),
        KeyCode::Char(c) => {
            model.insert_char(c);
            model.ac_sel = 0;
        }
        _ => {}
    }
}
