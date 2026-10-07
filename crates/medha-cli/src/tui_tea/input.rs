//! Terminal input and presentation only. Application effects are typed backend calls.
use super::backend_ui::*;
use super::*;

pub(super) fn handle_mouse(model: &mut Model, event: MouseEvent) {
    const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(450);
    /// Any of these turns a click into "open this", leaving plain click to select.
    const OPEN_CHORD: KeyModifiers = KeyModifiers::CONTROL
        .union(KeyModifiers::ALT)
        .union(KeyModifiers::SUPER);

    match event.kind {
        // Mouse capture is on, so the terminal's own ⌘-click on a URL never
        // fires — Medha has to do the opening itself.
        MouseEventKind::Down(MouseButton::Left) if event.modifiers.intersects(OPEN_CHORD) => {
            if let Some(point) = model.transcript_point(event.column, event.row) {
                open_under_cursor(model, point);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(point) = model.transcript_point(event.column, event.row) else {
                model.text_selection = None;
                model.mouse_selecting = false;
                return;
            };
            let now = Instant::now();
            let count = model
                .last_click
                .filter(|last| {
                    now.duration_since(last.at) <= MULTI_CLICK_WINDOW
                        && last.point.row == point.row
                        && last.point.column.abs_diff(point.column) <= 1
                })
                .map_or(1, |last| (last.count % 3) + 1);
            model.last_click = Some(LastClick {
                point,
                at: now,
                count,
            });

            match count {
                1 => {
                    model.text_selection = Some(TextSelection {
                        anchor: point,
                        focus: point,
                        non_empty: false,
                    });
                    model.mouse_selecting = true;
                }
                2 => {
                    model.mouse_selecting = false;
                    model.select_word(point);
                }
                _ => {
                    model.mouse_selecting = false;
                    model.select_line(point);
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if model.mouse_selecting => {
            if let Some(point) = model.transcript_point(event.column, event.row)
                && let Some(selection) = &mut model.text_selection
            {
                selection.focus = point;
                selection.non_empty = selection.anchor != point;
            }
        }
        MouseEventKind::Up(MouseButton::Left) if model.mouse_selecting => {
            if let Some(point) = model.transcript_point(event.column, event.row)
                && let Some(selection) = &mut model.text_selection
            {
                selection.focus = point;
                selection.non_empty = selection.anchor != point;
            }
            model.mouse_selecting = false;
            model.queue_selection_copy();
        }
        _ => {}
    }
}

/// What a modifier-click can hand to the operating system.
enum OpenTarget {
    Url(String),
    Path(std::path::PathBuf),
}

/// Open the URL or file path under the cursor. Anything else is left alone
/// rather than guessed at — a stray word must not launch an application.
fn open_under_cursor(model: &mut Model, point: TextPoint) {
    let target = model
        .transcript_line(point.row)
        .and_then(|line| target_at(&line_text(line), point.column));
    match target {
        Some(OpenTarget::Url(url)) => {
            os_open(&url);
            model.push_notice(format!("opening {url}"));
        }
        Some(OpenTarget::Path(path)) => {
            os_open(&path.to_string_lossy());
            model.push_notice(format!("opening {}", path.display()));
        }
        None => model
            .push_notice("nothing to open there — ctrl/alt-click a URL or a file path that exists"),
    }
}

/// Classify the token under `column`: an http(s) URL, or a path that resolves
/// inside the workspace.
fn target_at(text: &str, column: usize) -> Option<OpenTarget> {
    let token = token_at(text, column)?;
    let token = markdown_link_target(&token).unwrap_or_else(|| trim_delimiters(&token).to_string());
    if token.starts_with("https://") || token.starts_with("http://") {
        return Some(OpenTarget::Url(token));
    }
    // `path:42`, `path#L42` and `path:12:5` are how tools and the model cite
    // code; the locator is not part of the filename.
    let bare = strip_locator(token.split('#').next().unwrap_or(&token));
    if bare.is_empty() {
        return None;
    }
    let path = match bare.strip_prefix("~/") {
        Some(rest) => std::path::PathBuf::from(std::env::var_os("HOME")?).join(rest),
        None => std::path::PathBuf::from(bare),
    };
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    path.exists().then_some(OpenTarget::Path(path))
}

/// The whitespace-delimited token covering `column`, measured in terminal cells
/// so wide characters earlier in the line do not shift the hit test.
fn token_at(text: &str, column: usize) -> Option<String> {
    use unicode_width::UnicodeWidthChar;

    let mut cell = 0usize;
    let mut start = 0usize;
    let mut token = String::new();
    // The trailing space closes the final token without duplicating the check.
    for character in text.chars().chain(std::iter::once(' ')) {
        let width = character.width().unwrap_or(0);
        if character.is_whitespace() {
            if !token.is_empty() && (start..cell).contains(&column) {
                return Some(token);
            }
            token.clear();
            start = cell + width;
        } else {
            token.push(character);
        }
        cell += width;
    }
    None
}

/// `[label](target)` → `target`, so a rendered markdown link is clickable.
fn markdown_link_target(token: &str) -> Option<String> {
    let (_, rest) = token.split_once("](")?;
    Some(rest.trim_end_matches(')').to_string())
}

/// Strip the punctuation prose wraps around a reference: ``see `src/lib.rs`,``.
fn trim_delimiters(token: &str) -> &str {
    token
        .trim_start_matches(['`', '\'', '"', '(', '[', '{', '<'])
        .trim_end_matches(['`', '\'', '"', ')', ']', '}', '>', ',', ';', '!', '?', '.'])
}

/// Drop a trailing `:line[:col]` locator while leaving a Windows drive letter
/// (`C:\…`) alone, since that suffix is not numeric.
fn strip_locator(token: &str) -> &str {
    match token.rsplit_once(':') {
        Some((head, tail))
            if !head.is_empty() && !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) =>
        {
            strip_locator(head)
        }
        _ => token,
    }
}

/// Hand the target to the OS opener — the user's browser or GUI editor, never a
/// terminal editor that would fight the TUI for the screen.
fn os_open(target: &str) {
    #[cfg(target_os = "macos")]
    let (program, args) = ("open", vec![target]);
    #[cfg(target_os = "linux")]
    let (program, args) = ("xdg-open", vec![target]);
    #[cfg(target_os = "windows")]
    let (program, args) = ("cmd", vec!["/C", "start", "", target]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let (program, args): (&str, Vec<&str>) = ("", Vec::new());

    if !program.is_empty() {
        let _ = std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Removes exact bracketed-paste guards without trimming legitimate content.
pub(super) fn strip_paste_markers(s: &str) -> String {
    s.replace("\u{1b}[200~", "").replace("\u{1b}[201~", "")
}

/// Expands stored paste placeholders, preserving non-token text.
pub(super) fn expand_paste_tokens(pastes: &[String], s: &str) -> Result<String, String> {
    const MARK: &str = "[paste #";
    fn append(out: &mut String, text: &str) -> Result<(), String> {
        if text.len() > MAX_COMPOSER_BYTES.saturating_sub(out.len()) {
            return Err("Expanded input exceeds 2 MiB. Split it into smaller messages; your draft is preserved.".into());
        }
        out.push_str(text);
        Ok(())
    }
    if pastes.is_empty() || !s.contains(MARK) {
        let mut out = String::new();
        append(&mut out, s)?;
        return Ok(out);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(MARK) {
        append(&mut out, &rest[..start])?;
        let after = &rest[start + MARK.len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        match (digits.parse::<usize>(), after.find(']')) {
            (Ok(idx), Some(end)) if !digits.is_empty() => {
                let token = &rest[start..start + MARK.len() + end + 1];
                append(&mut out, pastes.get(idx).map_or(token, String::as_str))?;
                rest = &after[end + 1..];
            }
            _ => {
                // Not a real token — emit the marker literally and keep scanning.
                append(&mut out, MARK)?;
                rest = after;
            }
        }
    }
    append(&mut out, rest)?;
    Ok(out)
}

/// Lets a visible picker consume Esc before turn cancellation.
fn dismiss_running_picker_on_esc(model: &mut Model, key: &KeyEvent) -> bool {
    // Approval cards suppress picker rendering. A hidden picker must not steal
    // Esc from the visible approval/running-turn cancellation path.
    if key.code != KeyCode::Esc
        || !model.running
        || model.picker.is_none()
        || model.pending_approval().is_some()
    {
        return false;
    }
    model.picker = None;
    model.dirty = true;
    true
}

/// Process shortcuts that must remain available regardless of which modal,
/// form, picker, or approval currently owns ordinary keyboard input.
fn handle_global_key(model: &mut Model, key: &KeyEvent) -> bool {
    if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
        model.should_quit = true;
        return true;
    }
    false
}

pub(super) fn handle_key(model: &mut Model, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Ctrl-D is the unconditional escape hatch. Keep it ahead of every modal
    // handler so no picker, credential form, or approval can trap the user.
    if handle_global_key(model, &key) {
        return;
    }

    // A second Esc must win over UI that raced with graceful cancellation.
    if key.code == KeyCode::Esc && model.running && model.cancelling {
        force_abort_foreground_turn(model);
        return;
    }

    // A visible picker owns the first Esc; a later Esc can stop the turn.
    if dismiss_running_picker_on_esc(model, &key) {
        return;
    }

    // Clarify owns Esc while its tool is awaiting an answer.
    if model.clarify.is_some() {
        handle_clarify_key(model, key);
        return;
    }

    // Reading an agent, Esc comes back before it can cancel the foreground
    // conversation that owns the running turn. A live approval still owns Esc.
    if key.code == KeyCode::Esc
        && model.focus.is_some()
        && model.pending_approval().is_none()
        && model.picker.is_none()
        && model.model_setup.is_none()
        && model.search_setup.is_none()
        && model.mcp_credential.is_none()
    {
        model.focus_pane(None);
        model.switching = false;
        return;
    }

    // With no modal owner, Esc gracefully cancels the turn rather than a prompt.
    if key.code == KeyCode::Esc && model.running {
        cancel_foreground(model);
        // Denying owned prompts unblocks gates during cancellation.
        // A picker may have been suppressed while the approval card was shown;
        // do not let it unexpectedly reappear after cancellation.
        model.picker = None;
        return;
    }

    // Keep the editor usable while the aborted task's Drop cleanup joins, but
    // do not dispatch a command or consume a prompt until ownership is clear.
    if key.code == KeyCode::Enter && model.force_aborting {
        model.push_main_notice("(force stop is still quiescing owned work…)");
        return;
    }

    if key.code == KeyCode::Enter && model.session_op.is_some() {
        model.push_main_notice("(session change is still finishing — input preserved)");
        return;
    }

    // A visible approval owns ordinary input.
    if model.pending_approval().is_some() {
        handle_approval_key(model, key);
        return;
    }

    if handle_model_setup_key(model, key) {
        return;
    }
    if handle_mcp_credential_key(model, key) {
        return;
    }

    if handle_search_setup_key(model, key) {
        return;
    }

    if handle_memory_picker_key(model, &key) {
        return;
    }
    if handle_reasoning_picker_key(model, key) {
        return;
    }

    // Picker handling
    if backend_plugins::handle_key(model, key.code) {
        return;
    }
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
                return;
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
                return;
            }
            // Space toggles one tool's exposure inside the tool browser.
            KeyCode::Char(' ') if matches!(&picker.kind, PickerKind::McpTools { .. }) => {
                if let PickerKind::McpTools { id, tools } = &picker.kind
                    && let Some((name, on)) = tools.get(picker.selected)
                {
                    let (id, name, on) = (id.clone(), name.clone(), *on);
                    mcp_set_tool(model, &id, &name, !on);
                }
                return;
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
                return;
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
                return;
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
                return;
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
                return;
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
                return;
            }
            // → mirrors Enter and ← mirrors Esc, so the pickers navigate like
            // a nested menu: right descends, left backs out one level.
            KeyCode::Enter | KeyCode::Right => {
                if matches!(picker.kind, PickerKind::ModelProtocol) {
                    // Resolve through the same table that produced the rows.
                    let Some(&(label, available, protocol)) = MODEL_PROTOCOLS.get(picker.selected)
                    else {
                        return;
                    };
                    if !available {
                        model.push_notice(format!(
                            "{label} is not wired up yet — pick an available protocol for now"
                        ));
                        return;
                    }
                    match protocol {
                        kernel::Protocol::GeminiInteractions => {
                            model.picker = None;
                            if let Some(setup) = model.model_setup.as_mut() {
                                setup.protocol = protocol;
                                setup.base_url =
                                    "https://generativelanguage.googleapis.com/v1".into();
                                setup.step = ModelSetupStep::ApiKey;
                            }
                            model.push_notice("Gemini Interactions v1 — enter the Gemini API key");
                        }
                        // Everything else reaches a base URL the same way.
                        _ => {
                            if let Some(setup) = model.model_setup.as_mut() {
                                setup.protocol = protocol;
                            }
                            model.picker = Some(Picker::new(PickerKind::ProviderPreset));
                        }
                    }
                    return;
                }
                if matches!(picker.kind, PickerKind::ProviderPreset) {
                    let selected = picker.selected;
                    let presets = config::provider_presets();
                    model.picker = None;
                    if let Some(setup) = model.model_setup.as_mut() {
                        if let Some((name, url)) = presets.get(selected) {
                            setup.base_url = (*url).to_string();
                            setup.step = ModelSetupStep::ApiKey;
                            model.push_notice(format!(
                                "{name} — now the API key (blank for local servers)"
                            ));
                        } else {
                            setup.step = ModelSetupStep::BaseUrl;
                            model.push_notice("Custom provider selected — enter its base URL");
                        }
                    }
                    return;
                }
                if matches!(picker.kind, PickerKind::AutonomyMode) {
                    let selected = picker.selected;
                    model.picker = None;
                    if let Some((level, _)) = AUTONOMY_MODES.get(selected).copied() {
                        set_autonomy(model, level);
                    }
                    return;
                }
                if matches!(picker.kind, PickerKind::Theme) {
                    let selected = picker.selected;
                    model.picker = None;
                    if let Some((id, _)) = super::theme::modes().get(selected).copied() {
                        set_theme(model, id);
                    }
                    return;
                }
                // `/search` step 1: a provider was chosen. DuckDuckGo needs
                // nothing more and commits here; the others advance to the key
                // (Tavily/Brave) or URL (SearXNG) input.
                if matches!(picker.kind, PickerKind::SearchProvider) {
                    let selected = picker.selected;
                    model.picker = None;
                    let Some((provider, _)) = SEARCH_PROVIDERS.get(selected).copied() else {
                        model.search_setup = None;
                        return;
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
                    return;
                }
                // Server-reported context can complete setup without another field.
                if let PickerKind::ModelDiscovery(models) = &picker.kind {
                    let choice = models.get(picker.selected).cloned();
                    model.picker = None;
                    if model.model_setup.is_none() {
                        return;
                    }
                    match choice {
                        Some(m) => {
                            let ctx_known = m.context_length.is_some();
                            if let Some(setup) = model.model_setup.as_mut() {
                                setup.model = m.id;
                                setup.max_ctx = m.context_length;
                                if !ctx_known {
                                    setup.step = ModelSetupStep::ContextWindow;
                                }
                            }
                            if ctx_known {
                                finish_model_setup(model);
                            } else {
                                model.push_notice(
                                    "the server didn't report a context window — enter it in tokens (blank = unknown)",
                                );
                            }
                        }
                        None => {
                            if let Some(setup) = model.model_setup.as_mut() {
                                setup.step = ModelSetupStep::ModelId;
                            }
                        }
                    }
                    return;
                }
                if let PickerKind::Session(sessions) = &picker.kind
                    && let Some(meta) = sessions.get(picker.selected)
                {
                    let id = meta.id;
                    if model.foreground_owned() || model.has_active_agents() {
                        model.picker = None;
                        model.push_notice(
                            "finish foreground and background work before resuming a session",
                        );
                        return;
                    }
                    resume(model, id.to_string());
                    model.picker = None;
                    model.push_notice(format!("(loading session {id} …)"));
                    return;
                }
                if let PickerKind::Rewind(points) = &picker.kind
                    && let Some(point) = points.get(picker.selected).cloned()
                {
                    model.picker = Some(Picker::new(PickerKind::RewindMode(point)));
                    return;
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
                                model.push_notice(
                                    "finish foreground and background work before rewinding",
                                );
                                return;
                            }
                            model.picker = None;
                            rewind(model, at_event.to_string(), scope);
                            model.push_notice("(rewinding …)");
                        }
                        None => model.picker = None,
                    }
                    return;
                }
                if let PickerKind::Memory(entries) = &picker.kind {
                    if let Some(entry) = entries.get(picker.selected).cloned() {
                        let name = entry.name.clone();
                        model.picker = None;
                        spawn_memory_provenance(model, entry);
                        model.push_notice(format!("(opening memory '{name}' provenance …)"));
                    }
                    return;
                }
                if let PickerKind::McpTools { id, tools } = &picker.kind {
                    let (id, sel) = (id.clone(), picker.selected);
                    match tools.get(sel).cloned() {
                        Some((name, on)) => mcp_set_tool(model, &id, &name, !on),
                        None => open_mcp_picker(model),
                    }
                    return;
                }
                if let PickerKind::McpAuth { id, url } = &picker.kind {
                    let (id, url, sel) = (id.clone(), url.clone(), picker.selected);
                    model.picker = None;
                    mcp_choose_auth(model, &id, &url, sel);
                    return;
                }
                if let PickerKind::Mcp(rows) = &picker.kind {
                    let sel = picker.selected;
                    if sel == 0 {
                        model.picker = None;
                        prefill_command(
                            model,
                            "/mcp add ",
                            "paste a server URL — e.g.  https://mcp.linear.app/mcp   (sign-in is automatic)   ·   or:  <id> -- npx -y @modelcontextprotocol/server-github",
                        );
                        return;
                    }
                    let id = rows.get(sel - 1).map(|row| row.id.clone());
                    model.picker = None;
                    if let Some(id) = id {
                        start_mcp_server(model, &id);
                    }
                    return;
                }
                if let PickerKind::Connectors(picks) = &picker.kind {
                    let id = picks.get(picker.selected).map(|pick| pick.id.clone());
                    model.picker = None;
                    if let Some(id) = id {
                        connect_connector(model, &id);
                    }
                    return;
                }
                if let PickerKind::McpCatalog(picks) = &picker.kind {
                    let choice = picks.get(picker.selected).cloned();
                    model.picker = None;
                    if let Some(pick) = choice {
                        model.input = pick.line;
                        model.cursor = pick.cursor.min(model.input.len());
                        model.push_notice(
                            "fill in what's missing, then Enter to add it — registry listings aren't reviewed by Medha, so check the source first",
                        );
                    }
                    return;
                }
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
                    return;
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
                    return;
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
                    return;
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
                    return;
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
                    return;
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
                    return;
                }
                if let PickerKind::RemoveModel(name) = &picker.kind {
                    let name = name.clone();
                    let confirmed = picker.selected == 1;
                    model.picker = None;
                    if confirmed {
                        remove_saved_model(model, &name);
                    } else {
                        open_model_picker(model);
                    }
                    return;
                }
                if let PickerKind::ModelCredential(profiles) = &picker.kind {
                    let profile = profiles.get(picker.selected).cloned();
                    model.picker = None;
                    if let Some(profile) = profile {
                        begin_model_key_update(model, profile);
                    }
                    return;
                }
                if let PickerKind::ModelDefault(profiles) = &picker.kind {
                    let name = profiles.get(picker.selected).map(|p| p.name.clone());
                    model.picker = None;
                    if let Some(name) = name {
                        set_default_model(model, &name);
                    }
                    return;
                }
                if let PickerKind::ModelRemove(profiles) = &picker.kind {
                    let name = profiles.get(picker.selected).map(|p| p.name.clone());
                    if let Some(name) = name {
                        if name == model.active_profile {
                            model.picker = None;
                            model.push_notice(
                                "switch to another model before removing the active profile",
                            );
                        } else {
                            model.picker = Some(Picker::new(PickerKind::RemoveModel(name)));
                        }
                    }
                    return;
                }
                if let PickerKind::Model { profiles, active } = &picker.kind {
                    // Rows 0..n are the models themselves — Enter switches
                    // directly. Management actions sit after the list.
                    if let Some(profile) = profiles.get(picker.selected) {
                        let name = profile.name.clone();
                        model.picker = None;
                        switch_saved_model(model, &name);
                        return;
                    }
                    match picker.selected - profiles.len() {
                        0 => {
                            model.picker = None;
                            begin_model_setup(model);
                        }
                        1 => {
                            model.picker =
                                Some(Picker::new(PickerKind::ModelCredential(profiles.clone())));
                        }
                        2 => {
                            model.picker =
                                Some(Picker::new(PickerKind::ModelDefault(profiles.clone())));
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
                                model.picker =
                                    Some(Picker::new(PickerKind::ModelRemove(removable)));
                            }
                        }
                        _ => {}
                    }
                    return;
                }
            }
            KeyCode::Esc | KeyCode::Left => {
                if matches!(
                    picker.kind,
                    PickerKind::ModelProtocol
                        | PickerKind::ProviderPreset
                        | PickerKind::ModelDiscovery(_)
                ) {
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
        return;
    }

    // The agent switcher. Handled before the input so its keys are unambiguous
    // while it is open, and it only opens when there is somewhere to go.
    if model.switching {
        match key.code {
            KeyCode::Esc | KeyCode::Tab => {
                model.switching = false;
                return;
            }
            KeyCode::Enter => {
                let rows = model.switch_rows();
                let Some(target) = rows.get(model.switch_cursor).cloned() else {
                    model.reconcile_switch_cursor(None);
                    return;
                };
                model.focus_pane(target);
                model.switching = false;
                return;
            }
            // Stop the agent under the cursor. Never `main` — the conversation is
            // not something you stop, and Esc already interrupts a turn.
            KeyCode::Char('x') => {
                let rows = model.switch_rows();
                match rows.get(model.switch_cursor).cloned().flatten() {
                    Some(path) => agents_stop_path(model, &path),
                    None => model.push_notice("that is the conversation — Esc interrupts a turn"),
                }
                return;
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                stop_every_agent(model);
                return;
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
        return;
    }

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
                    return;
                }
                KeyCode::Down => {
                    model.ac_sel = (model.ac_sel + 1) % matches.len();
                    return;
                }
                KeyCode::Tab => {
                    model.input = format!("{} ", matches[model.ac_sel].0);
                    model.cursor = model.input.len();
                    return;
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
                    return;
                }
                _ => {}
            }
        }
    }

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

/// Handles keys owned by the active structured-question form.
pub(super) fn handle_clarify_key(model: &mut Model, key: KeyEvent) {
    // Snapshot the layout from a short immutable borrow (mutating helpers below
    // re-borrow `model`, so we can't hold the state borrow across them).
    let Some((rows, other_row, multi, cursor, entering_other)) = model.clarify.as_ref().map(|s| {
        (
            s.row_count(),
            s.other_row(),
            s.questions[s.idx].multi_select,
            s.cursor,
            s.entering_other,
        )
    }) else {
        return;
    };

    // Free-text "Other" owns keys until Enter (commit) or Esc (cancel input). It
    // has a form-local buffer so the user's main composer draft remains untouched.
    if entering_other {
        match key.code {
            KeyCode::Enter => {
                if let Some(s) = model.clarify.as_mut() {
                    let text = std::mem::take(&mut s.other_input).trim().to_string();
                    s.other_cursor = 0;
                    let i = s.idx;
                    let has_text = !text.is_empty();
                    s.drafts[i].other = has_text.then_some(text);
                    // Radio: a typed Other IS the answer — clear the option pick so
                    // the user can't submit "Python" and "actually Java" at once.
                    if has_text && !s.questions[i].multi_select {
                        s.drafts[i].selected.clear();
                    }
                    s.entering_other = false;
                    s.validation = None;
                }
                model.dirty = true;
            }
            KeyCode::Esc => {
                if let Some(s) = model.clarify.as_mut() {
                    s.other_input.clear();
                    s.other_cursor = 0;
                    s.entering_other = false;
                    s.validation = None;
                }
                model.dirty = true;
            }
            KeyCode::Backspace => edit_other(model, OtherEdit::Backspace),
            KeyCode::Left => edit_other(model, OtherEdit::Left),
            KeyCode::Right => edit_other(model, OtherEdit::Right),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                edit_other(model, OtherEdit::Insert(c));
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => cancel_clarify(model), // dismiss → tool returns skipped
        // ←→ switch between questions (each keeps its own selections).
        KeyCode::Left => switch_question(model, -1),
        KeyCode::Right => switch_question(model, 1),
        // ↑↓ move within the current question's rows (options + Other).
        KeyCode::Up => {
            if let Some(s) = model.clarify.as_mut() {
                s.cursor = s.cursor.checked_sub(1).unwrap_or(rows - 1);
            }
            model.dirty = true;
        }
        KeyCode::Down => {
            if let Some(s) = model.clarify.as_mut() {
                s.cursor = (s.cursor + 1) % rows;
            }
            model.dirty = true;
        }
        // Space interacts with the highlighted row: pick/toggle an option, or
        // open the free-text editor on the Other row.
        KeyCode::Char(' ') => {
            if cursor < other_row {
                if let Some(s) = model.clarify.as_mut() {
                    toggle_option(s, cursor, multi);
                    s.validation = None;
                }
                model.dirty = true;
            } else {
                open_other(model);
            }
        }
        // Enter proceeds: advance to the next question, or submit from the last.
        // On a radio option it first selects the focused row. Enter never opens
        // the Other editor (Space does) — so it's never a dead end on the Other
        // row of a multi-question form.
        KeyCode::Enter => {
            if !multi
                && cursor < other_row
                && let Some(s) = model.clarify.as_mut()
            {
                toggle_option(s, cursor, false);
                s.validation = None;
            }
            let (idx, last) = model
                .clarify
                .as_ref()
                .map(|s| (s.idx, s.questions.len().saturating_sub(1)))
                .unwrap_or((0, 0));
            if idx < last {
                switch_question(model, 1);
            } else {
                submit_clarify(model);
            }
        }
        _ => {}
    }
}

/// Updates an option, keeping single-select choices exclusive with free text.
fn toggle_option(s: &mut ClarifyState, i: usize, multi: bool) {
    let d = &mut s.drafts[s.idx];
    if multi {
        if let Some(pos) = d.selected.iter().position(|&x| x == i) {
            d.selected.remove(pos);
        } else {
            d.selected.push(i);
        }
    } else {
        d.selected = vec![i];
        d.other = None;
    }
}

enum OtherEdit {
    Insert(char),
    Backspace,
    Left,
    Right,
}

/// Edit the form-local Other buffer while maintaining the same UTF-8 byte-offset
/// invariant as the main composer.
fn edit_other(model: &mut Model, edit: OtherEdit) {
    let Some(s) = model.clarify.as_mut() else {
        return;
    };
    match edit {
        OtherEdit::Insert(c) => {
            if s.other_input.len() + c.len_utf8() > 128 * 1024 {
                s.validation = Some("This answer is too large; keep it under 128 KiB.".into());
                model.dirty = true;
                return;
            }
            s.other_input.insert(s.other_cursor, c);
            s.other_cursor += c.len_utf8();
        }
        OtherEdit::Backspace => {
            if let Some(c) = s.other_input[..s.other_cursor].chars().next_back() {
                s.other_cursor -= c.len_utf8();
                s.other_input.remove(s.other_cursor);
            }
        }
        OtherEdit::Left => {
            if let Some(c) = s.other_input[..s.other_cursor].chars().next_back() {
                s.other_cursor -= c.len_utf8();
            }
        }
        OtherEdit::Right => {
            if let Some(c) = s.other_input[s.other_cursor..].chars().next() {
                s.other_cursor += c.len_utf8();
            }
        }
    }
    s.validation = None;
    model.dirty = true;
}

/// Open the free-text "Other" editor for the current question, seeded with any
/// text already entered in this form's dedicated buffer.
fn open_other(model: &mut Model) {
    if let Some(s) = model.clarify.as_mut() {
        let i = s.idx;
        s.other_input = s.drafts[i].other.clone().unwrap_or_default();
        s.other_cursor = s.other_input.len();
        s.entering_other = true;
        s.validation = None;
    }
    model.dirty = true;
}

/// Move between questions by `delta`, clamped to the range; reset the row cursor.
fn switch_question(model: &mut Model, delta: isize) {
    if let Some(s) = model.clarify.as_mut() {
        let n = s.questions.len() as isize;
        let next = (s.idx as isize + delta).clamp(0, n - 1);
        s.idx = next as usize;
        s.cursor = s.drafts[s.idx].selected.first().copied().unwrap_or(0);
        s.validation = None;
        model.dirty = true;
    }
}

/// `/theme` opens the picker (↑↓ + Enter). `/theme <id>` and `/theme toggle`
/// apply directly without the menu.
pub(super) fn apply_theme_command(model: &mut Model, arg: &str) {
    use super::theme;
    match arg {
        "" => open_theme_picker(model),
        // Light and back. Returning to the *default* rather than to `dark` keeps
        // the toggle a round trip — otherwise a session that started on the
        // default could never toggle its way home.
        "toggle" => {
            let id = if theme::current().is_dark {
                "light"
            } else {
                theme::default_palette().id
            };
            set_theme(model, id);
        }
        id if theme::modes().iter().any(|(m, _)| *m == id) => set_theme(model, id),
        _ => {
            let ids: Vec<&str> = theme::modes().iter().map(|(m, _)| *m).collect();
            model.push_notice(format!(
                "usage: /theme [{}]  (bare /theme opens a picker)",
                ids.join("|")
            ));
        }
    }
}

/// Open the `/theme` picker, cursor on the current mode.
pub(super) fn open_theme_picker(model: &mut Model) {
    let id = super::theme::current().id;
    let sel = super::theme::modes()
        .iter()
        .position(|(m, _)| *m == id)
        .unwrap_or(0);
    model.picker = Some(Picker::with_selected(PickerKind::Theme, sel));
}

/// Apply a theme by its id and re-colour the UI live.
pub(super) fn set_theme(model: &mut Model, id: &str) {
    use super::theme;
    let palette = theme::resolve(id);
    theme::set(palette);
    model.invalidate_all_renders();
    // On the splash the re-colour is its own confirmation, and a notice would
    // empty the transcript check that keeps the splash up — replacing the very
    // screen the theme is being judged on with a one-line receipt.
    if !model.on_welcome_splash() {
        model.push_notice(format!("theme: {}", palette.id));
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SlashAction {
    Resume,
    Rewind,
    Clear,
    /// `/attach <path>` — stage a local image on the composer.
    Attach(String),
    /// `/attach paste` — stage an image from the clipboard (also Ctrl-V).
    Paste,
    /// `/attach remove [number|all]` — unstage images before sending.
    Detach(String),
    Lsp,
    /// `/plugins [install|enable|disable|remove …]`.
    Plugins(String),
    /// `/hooks [add …]` — add a project hook without writing a file by hand.
    Hooks(String),
    /// `/mcp` — open the MCP management picker.
    Mcp,
    Agents,
    /// `/agents steer <agent> <text>` — instruct a running agent.
    Steer(String),
    /// `/agents followup <agent> <text>` — resume an agent with more work.
    Followup(String),
    /// `/agents tree` — the whole agent tree, settled ones included.
    Tree,
    /// `/mcp start <id>` — approve and connect a configured MCP server.
    McpStart(String),
    /// `/mcp add <id> [--trust trusted] [--env K=V] -- <command>` — add + connect.
    McpAdd(String),
    /// `/mcp catalog [search]` — browse the public MCP Registry.
    McpCatalog(String),
    /// `/connect [app]` — the reviewed connectors.
    Connect(String),
    /// `/usage` — tokens and cost for this session and the last week.
    Usage,
    Memory(String),
    SkillPicker,
    LoadSkill(String),
    SkillInfo(String),
    RemoveSkill(String),
    EnableSkill(String),
    DisableSkill(String),
    ModelPicker,
    AddModel,
    /// `/model <name>` — switch to a saved profile without opening the picker.
    SwitchModel(String),
    /// `/search` — open the web-search provider picker.
    SearchConfig,
    /// `/mode` — open the autonomy-level picker.
    ModePicker,
    /// `/mode <level>` — set the autonomy level without opening the picker.
    SwitchMode(String),
    /// `/skill install <path-or-url>` — install a skill into the user scope.
    InstallSkill(String),
    /// `/skill sources [add <owner/repo [path]> | remove <owner/repo>]` —
    /// list or edit the registered skill sources ("taps").
    SkillSources(String),
    /// `/skill search <query>` — search registered sources for skills.
    SearchSkills(String),
    /// `/skill add <word-or-link>` — search the catalog or install a link/path.
    AddSkill(String),
    /// `/skill update [<name> | --all]` — check (and optionally apply) updates.
    UpdateSkills(String),
    /// `/skill lock` — write the skills lockfile from what's installed.
    LockSkills,
    /// `/skill sync` — install/repair skills to match the lockfile.
    SyncSkills,
    /// Everything else — handled by `run_slash` (help, status, skills, think…).
    Other,
}

/// Map a slash command (leading `/` already stripped) to its action. This is the
/// single source of truth for routing; both Enter paths go through it.
pub(super) fn classify_slash(cmd: &str) -> SlashAction {
    match cmd {
        "resume" => SlashAction::Resume,
        "rewind" => SlashAction::Rewind,
        "clear" => SlashAction::Clear,
        "paste" => SlashAction::Paste,
        "attach paste" => SlashAction::Paste,
        "attach clear" => SlashAction::Detach("all".into()),
        c if c.strip_prefix("attach remove").is_some_and(is_cmd_boundary) => SlashAction::Detach(
            c.strip_prefix("attach remove")
                .unwrap_or("")
                .trim()
                .to_string(),
        ),
        c if c.strip_prefix("attach").is_some_and(is_cmd_boundary) => {
            SlashAction::Attach(c.strip_prefix("attach").unwrap_or("").trim().to_string())
        }
        c if c.strip_prefix("detach").is_some_and(is_cmd_boundary) => {
            SlashAction::Detach(c.strip_prefix("detach").unwrap_or("").trim().to_string())
        }
        "lsp" => SlashAction::Lsp,
        c if c.strip_prefix("hooks").is_some_and(is_cmd_boundary) => {
            SlashAction::Hooks(c.strip_prefix("hooks").unwrap_or("").trim().to_string())
        }
        c if c.strip_prefix("plugins").is_some_and(is_cmd_boundary) => {
            SlashAction::Plugins(c.strip_prefix("plugins").unwrap_or("").trim().to_string())
        }
        "mcp" => SlashAction::Mcp,
        "agents" => SlashAction::Agents,
        "agents tree" => SlashAction::Tree,
        c if c
            .strip_prefix("agents followup")
            .is_some_and(is_cmd_boundary) =>
        {
            SlashAction::Followup(
                c.strip_prefix("agents followup")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            )
        }
        c if c.strip_prefix("agents steer").is_some_and(is_cmd_boundary) => SlashAction::Steer(
            c.strip_prefix("agents steer")
                .unwrap_or("")
                .trim()
                .to_string(),
        ),
        "tree" => SlashAction::Tree,
        c if c.strip_prefix("followup").is_some_and(is_cmd_boundary) => {
            SlashAction::Followup(c.strip_prefix("followup").unwrap_or("").trim().to_string())
        }
        c if c.strip_prefix("steer").is_some_and(is_cmd_boundary) => {
            SlashAction::Steer(c.strip_prefix("steer").unwrap_or("").trim().to_string())
        }
        c if c.strip_prefix("mcp start").is_some_and(is_cmd_boundary) => {
            SlashAction::McpStart(c.strip_prefix("mcp start").unwrap_or("").trim().to_string())
        }
        c if c.strip_prefix("mcp add").is_some_and(is_cmd_boundary) => {
            SlashAction::McpAdd(c.strip_prefix("mcp add").unwrap_or("").trim().to_string())
        }
        "usage" => SlashAction::Usage,
        c if c.strip_prefix("connect").is_some_and(is_cmd_boundary) => {
            SlashAction::Connect(c.strip_prefix("connect").unwrap_or("").trim().to_string())
        }
        c if c.strip_prefix("mcp catalog").is_some_and(is_cmd_boundary) => SlashAction::McpCatalog(
            c.strip_prefix("mcp catalog")
                .unwrap_or("")
                .trim()
                .to_string(),
        ),
        c if c.strip_prefix("memory").is_some_and(is_cmd_boundary) => {
            SlashAction::Memory(c.strip_prefix("memory").unwrap_or("").trim().to_string())
        }
        "skill" => SlashAction::SkillPicker,
        "model" => SlashAction::ModelPicker,
        "model add" => SlashAction::AddModel,
        "search" => SlashAction::SearchConfig,
        "mode" => SlashAction::ModePicker,
        "plan" => SlashAction::SwitchMode("plan".into()),
        c if c.starts_with("mode ") => {
            SlashAction::SwitchMode(c.strip_prefix("mode ").unwrap_or("").trim().to_string())
        }
        c if c.starts_with("model ") => {
            SlashAction::SwitchModel(c.strip_prefix("model ").unwrap_or("").trim().to_string())
        }
        c if c.strip_prefix("skill sources").is_some_and(is_cmd_boundary) => {
            SlashAction::SkillSources(
                c.strip_prefix("skill sources")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            )
        }
        c if c.strip_prefix("skill search").is_some_and(is_cmd_boundary) => {
            SlashAction::SearchSkills(
                c.strip_prefix("skill search")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            )
        }
        c if c.strip_prefix("skill update").is_some_and(is_cmd_boundary) => {
            SlashAction::UpdateSkills(
                c.strip_prefix("skill update")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            )
        }
        c if c.strip_prefix("skill add").is_some_and(is_cmd_boundary) => {
            SlashAction::AddSkill(c.strip_prefix("skill add").unwrap_or("").trim().to_string())
        }
        "skill lock" => SlashAction::LockSkills,
        "skill sync" => SlashAction::SyncSkills,
        c if c.strip_prefix("skill install").is_some_and(is_cmd_boundary) => {
            SlashAction::InstallSkill(
                c.strip_prefix("skill install")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            )
        }
        c if c.strip_prefix("skill info").is_some_and(is_cmd_boundary) => SlashAction::SkillInfo(
            c.strip_prefix("skill info")
                .unwrap_or("")
                .trim()
                .to_string(),
        ),
        c if c.strip_prefix("skill remove").is_some_and(is_cmd_boundary) => {
            SlashAction::RemoveSkill(
                c.strip_prefix("skill remove")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            )
        }
        c if c.strip_prefix("skill enable").is_some_and(is_cmd_boundary) => {
            SlashAction::EnableSkill(
                c.strip_prefix("skill enable")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            )
        }
        c if c.strip_prefix("skill disable").is_some_and(is_cmd_boundary) => {
            SlashAction::DisableSkill(
                c.strip_prefix("skill disable")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            )
        }
        c if c.strip_prefix("skill load").is_some_and(is_cmd_boundary) => SlashAction::LoadSkill(
            c.strip_prefix("skill load")
                .unwrap_or("")
                .trim()
                .to_string(),
        ),
        c if c.starts_with("skill ") => {
            SlashAction::LoadSkill(c.strip_prefix("skill ").unwrap_or("").trim().to_string())
        }
        _ => SlashAction::Other,
    }
}

/// Where a staging request came from: explicit paths, or the system clipboard.
pub(super) enum ImageSource {
    Clipboard,
    Paths(Vec<std::path::PathBuf>),
}

pub(super) fn unstaged_images(model: &Model, text: &str) -> Vec<std::path::PathBuf> {
    crate::attachments::refs::scan(text, model.restore.root())
        .into_iter()
        .filter(|path| !model.images.holds(path))
        .collect()
}

/// The message once its images are attachments rather than paths. A dropped
/// screenshot leaves nothing behind; if it was the whole message, the shared
/// image-only line stands in for it.
pub(super) fn message_for(model: &Model, line: &str, attached: &[std::path::PathBuf]) -> String {
    let stripped =
        crate::attachments::refs::strip_unreachable(line, attached, model.restore.root());
    if stripped.trim().is_empty() {
        crate::attachments::IMAGE_ONLY_PROMPT.to_string()
    } else {
        stripped
    }
}

pub(super) fn handle_paste(model: &mut Model, data: String) {
    if let Some(state) = &mut model.clarify {
        if state.entering_other {
            let clean = strip_paste_markers(&data);
            if clean.len() <= (128 * 1024usize).saturating_sub(state.other_input.len()) {
                state.other_input.insert_str(state.other_cursor, &clean);
                state.other_cursor += clean.len();
                model.dirty = true;
            } else {
                state.validation = Some("This answer is too large; keep it under 128 KiB.".into());
            }
        }
        return;
    }
    if model.model_setup.as_ref().is_some_and(|setup| {
        matches!(
            setup.step,
            ModelSetupStep::Saving | ModelSetupStep::Activating | ModelSetupStep::Discovering
        )
    }) || model
        .search_setup
        .as_ref()
        .is_some_and(|setup| setup.step == SearchSetupStep::Saving)
        || model
            .mcp_credential
            .as_ref()
            .is_some_and(|form| form.saving)
    {
        return;
    }
    let mut clean = strip_paste_markers(&data);
    if model.model_setup.is_some() || model.search_setup.is_some() || model.mcp_credential.is_some()
    {
        model.insert_text(&clean);
        return;
    }
    if !model.can_insert(clean.len()) {
        return;
    }
    let images = unstaged_images(model, &clean);
    if !images.is_empty() && stage_images(model, ImageSource::Paths(images.clone())) {
        let only_paths =
            crate::attachments::refs::only_paths(&clean, &images, model.restore.root());
        clean = crate::attachments::refs::strip_unreachable(&clean, &images, model.restore.root());
        if only_paths || clean.is_empty() {
            model.ac_sel = 0;
            return;
        }
    }
    let count = clean.chars().count();
    if count > PASTE_COLLAPSE_THRESHOLD {
        let idx = model.pastes.len();
        let token = format!("[paste #{idx}: {count} chars]");
        if !model.can_insert(clean.len() + token.len()) {
            return;
        }
        model.insert_text(&token);
        model.pastes.push(clean);
    } else {
        model.insert_text(&clean);
    }
    model.ac_sel = 0;
}
fn is_cmd_boundary(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_slash_routes_steer_with_its_whole_message() {
        assert_eq!(
            classify_slash("steer only the parser, skip the lexer"),
            SlashAction::Steer("only the parser, skip the lexer".into())
        );
        assert_eq!(classify_slash("steer"), SlashAction::Steer(String::new()));
        assert_ne!(
            classify_slash("steering"),
            SlashAction::Steer(String::new())
        );
        assert_eq!(
            classify_slash("agents steer parser only fix this"),
            SlashAction::Steer("parser only fix this".into())
        );
        assert_eq!(classify_slash("agents tree"), SlashAction::Tree);
        assert_eq!(
            classify_slash("agents followup parser add tests"),
            SlashAction::Followup("parser add tests".into())
        );
    }

    #[test]
    fn attach_is_one_command_with_compatible_hidden_aliases() {
        assert_eq!(classify_slash("attach paste"), SlashAction::Paste);
        assert_eq!(
            classify_slash("attach remove 2"),
            SlashAction::Detach("2".into())
        );
        assert_eq!(
            classify_slash("attach clear"),
            SlashAction::Detach("all".into())
        );
        assert_eq!(classify_slash("paste"), SlashAction::Paste);
        assert_eq!(
            classify_slash("detach all"),
            SlashAction::Detach("all".into())
        );
    }

    #[test]
    fn classify_slash_routes_skill_commands() {
        assert_eq!(classify_slash("skill"), SlashAction::SkillPicker);
        assert_eq!(
            classify_slash("skill frontend-ui-design"),
            SlashAction::LoadSkill("frontend-ui-design".into())
        );
        assert_eq!(classify_slash("skills"), SlashAction::Other);
        assert_eq!(classify_slash("model"), SlashAction::ModelPicker);
        assert_eq!(classify_slash("model add"), SlashAction::AddModel);
        assert_eq!(classify_slash("memory"), SlashAction::Memory(String::new()));
        assert_eq!(
            classify_slash("memory quoted-fact"),
            SlashAction::Memory("quoted-fact".into())
        );
        assert_eq!(
            classify_slash("mcp catalog postgres"),
            SlashAction::McpCatalog("postgres".into())
        );
        assert_eq!(
            classify_slash("mcp catalog"),
            SlashAction::McpCatalog(String::new())
        );
        assert_eq!(classify_slash("usage"), SlashAction::Usage);
        assert_eq!(
            classify_slash("model fast-local"),
            SlashAction::SwitchModel("fast-local".into())
        );
        assert_eq!(
            classify_slash("skill install https://example.com/SKILL.md"),
            SlashAction::InstallSkill("https://example.com/SKILL.md".into())
        );
        assert_eq!(
            classify_slash("skill install"),
            SlashAction::InstallSkill(String::new())
        );
        assert_eq!(
            classify_slash("skill info frontend-ui-design"),
            SlashAction::SkillInfo("frontend-ui-design".into())
        );
        assert_eq!(
            classify_slash("skill remove frontend-ui-design"),
            SlashAction::RemoveSkill("frontend-ui-design".into())
        );
        assert_eq!(
            classify_slash("skill load frontend-ui-design"),
            SlashAction::LoadSkill("frontend-ui-design".into())
        );
        assert_eq!(
            classify_slash("skill installer"),
            SlashAction::LoadSkill("installer".into())
        );
        assert_eq!(
            classify_slash("skill sources"),
            SlashAction::SkillSources(String::new())
        );
        assert_eq!(
            classify_slash("skill sources add anthropics/skills"),
            SlashAction::SkillSources("add anthropics/skills".into())
        );
        assert_eq!(
            classify_slash("skill search pdf"),
            SlashAction::SearchSkills("pdf".into())
        );
        assert_eq!(
            classify_slash("skill update"),
            SlashAction::UpdateSkills(String::new())
        );
        assert_eq!(
            classify_slash("skill update --all"),
            SlashAction::UpdateSkills("--all".into())
        );
        assert_eq!(classify_slash("skill lock"), SlashAction::LockSkills);
        assert_eq!(classify_slash("skill sync"), SlashAction::SyncSkills);
        assert_eq!(
            classify_slash("skill add pdf"),
            SlashAction::AddSkill("pdf".into())
        );
        assert_eq!(
            classify_slash("skill add"),
            SlashAction::AddSkill(String::new())
        );
        assert_eq!(
            classify_slash("skill adder"),
            SlashAction::LoadSkill("adder".into())
        );
    }

    #[test]
    fn cmd_boundary_distinguishes_think_from_thinking() {
        assert!(is_cmd_boundary("")); // /think
        assert!(is_cmd_boundary(" high")); // /think high
        assert!(!is_cmd_boundary("ing")); // /thinking → must fall through
    }

    #[test]
    fn token_hit_test_uses_terminal_cells() {
        let line = "see src/lib.rs and https://x.dev/a";
        assert_eq!(token_at(line, 0).as_deref(), Some("see"));
        assert_eq!(token_at(line, 4).as_deref(), Some("src/lib.rs"));
        assert_eq!(token_at(line, 20).as_deref(), Some("https://x.dev/a"));
        assert_eq!(token_at(line, 3), None); // the space between tokens
        // Hit testing counts cells occupied by earlier wide glyphs.
        assert_eq!(token_at("→ src/lib.rs", 2).as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn references_are_unwrapped_before_resolving() {
        assert_eq!(trim_delimiters("`src/lib.rs`,"), "src/lib.rs");
        assert_eq!(trim_delimiters("(src/lib.rs)."), "src/lib.rs");
        assert_eq!(
            markdown_link_target("[lib.rs](crates/mcp/src/lib.rs)").as_deref(),
            Some("crates/mcp/src/lib.rs")
        );
        assert_eq!(markdown_link_target("plain"), None);
    }

    #[test]
    fn line_locators_are_not_part_of_the_filename() {
        assert_eq!(strip_locator("src/lib.rs:42"), "src/lib.rs");
        assert_eq!(strip_locator("src/lib.rs:42:7"), "src/lib.rs");
        assert_eq!(strip_locator("src/lib.rs"), "src/lib.rs");
        assert_eq!(strip_locator("C:\\src\\lib.rs"), "C:\\src\\lib.rs");
    }

    #[test]
    fn only_urls_and_existing_paths_are_openable() {
        assert!(matches!(
            target_at("visit https://medha.dev/docs now", 6),
            Some(OpenTarget::Url(url)) if url == "https://medha.dev/docs"
        ));
        assert!(target_at("just some prose here", 5).is_none());
        assert!(target_at("see does/not/exist.rs", 4).is_none());
    }
}
