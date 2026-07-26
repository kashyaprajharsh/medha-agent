//! Update/input/async logic: the TEA update fn, key handling, and the
//! StreamSink→TuiEvent bridge. Split out of the old monolithic tui_tea.rs.
#![allow(clippy::too_many_arguments)]
use super::*;
use sandbox::WorkspaceSandbox;

pub(super) fn update<P, L>(
    model: &mut Model,
    msg: Msg,
    kernel: &Arc<Kernel<P, L>>,
    session: &mut Session,
    transcript: &mut Vec<Message>,
    budget: &Budget,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) where
    P: ProfileProvider + 'static,
    L: EventLog + 'static,
{
    match msg {
        Msg::KeyPress(key) => handle_key(model, key, kernel, session, transcript, budget, tx),
        Msg::MouseScroll(delta) => model.scroll_by(delta),
        Msg::Mouse(event) => handle_mouse(model, event),
        Msg::Paste(data) => handle_paste(model, data),
        Msg::Resize(height) => {
            model.viewport_height = height as usize;
            model.auto_scroll = model.scroll_offset >= model.max_scroll();
            model.text_selection = None;
        }
        Msg::AgentEvent(ev) => handle_agent_event(model, ev, session, transcript),
        Msg::Tick => {
            model.anim_frame = model.anim_frame.wrapping_add(1);
            // A background agent's report is durably recorded and no turn is in
            // flight — so run one. The answer to a question the user already
            // asked should not wait on them sending an unrelated message.
            //
            // The flag is cleared *before* the turn starts: `spawn_turn` sets
            // `running`, and leaving it set would arm a second turn the instant
            // this one settles, on a report that has already been delivered.
            if model.agent_report_deferred && !model.running && model.picker.is_none() {
                model.agent_report_deferred = false;
                spawn_turn(model, kernel, session, transcript, budget, tx, None);
            }
            if let Some(f) = model.intro_frame {
                model.intro_frame = if f >= 40 { None } else { Some(f + 1) };
            }
            // Refresh the live background-task list a few times a second (cheap
            // mutex read) so the status-line indicator tracks reality.
            if model.anim_frame % 16 == 0 {
                model.bg_tasks = kernel.executor.background_tasks();
                // Delegation is invisible otherwise: the child's work never
                // enters the transcript, so without this the user cannot tell
                // Medha handed part of the task to an agent.
                let running = model
                    .agents
                    .as_ref()
                    .map(|control| control.active())
                    .unwrap_or_default();
                // A child leaves the running set exactly when it settles, so the
                // difference is the finish notification — no extra channel, and
                // the user is not left watching a spinner for work that landed.
                let finished: Vec<(String, String)> = model
                    .agent_runs
                    .iter()
                    .filter(|previous| !running.iter().any(|run| run.session == previous.session))
                    .map(|previous| (previous.path.name().to_string(), previous.session.clone()))
                    .collect();
                model.agent_runs = running;
                for (agent, session) in finished {
                    // A writer that produced a patch is not just "finished" —
                    // there is something waiting on the user, and saying only
                    // that a report is coming would bury it. The cached lookup
                    // is deliberate: this runs on the UI thread, and the patch
                    // was written by this process moments ago.
                    let patch = model
                        .agents
                        .as_ref()
                        .and_then(|control| control.cached_patch(&session));
                    match patch {
                        Some(patch) if !patch.is_empty() => model.push_notice(format!(
                            "⎇ agent '{agent}' finished with a patch to {} file(s), {} — \
                             review it with /agents",
                            patch.files.len(),
                            match patch.verification.as_ref().map(|e| e.passed) {
                                Some(true) => "verified",
                                Some(false) => "BUILD FAILED",
                                None => "not verified",
                            }
                        )),
                        _ => model.push_notice(format!(
                            "⚇ agent '{agent}' finished — reading its report"
                        )),
                    }
                }
            }
        }
    }
}

fn handle_mouse(model: &mut Model, event: MouseEvent) {
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

/// Remove any leaked bracketed-paste guard sequences (PART 2). Terminals send these
/// around a paste; if they leak into the payload they must be stripped exactly, not
/// via per-character trimming (which would eat legitimate content).
pub(super) fn strip_paste_markers(s: &str) -> String {
    s.replace("\u{1b}[200~", "").replace("\u{1b}[201~", "")
}

/// Replace `[paste #N: M chars]` placeholder tokens with the full content stored in
/// `pastes[N]` (PART 2). Non-token text is passed through untouched.
pub(super) fn expand_paste_tokens(pastes: &[String], s: &str) -> String {
    const MARK: &str = "[paste #";
    if pastes.is_empty() || !s.contains(MARK) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(MARK) {
        out.push_str(&rest[..start]);
        let after = &rest[start + MARK.len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        match (digits.parse::<usize>(), after.find(']')) {
            (Ok(idx), Some(end)) if !digits.is_empty() => {
                if let Some(full) = pastes.get(idx) {
                    out.push_str(full);
                }
                rest = &after[end + 1..];
            }
            _ => {
                // Not a real token — emit the marker literally and keep scanning.
                out.push_str(MARK);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Handle paste event (PART 2: bracketed paste, atomic insert, large-paste collapse)
pub(super) fn handle_paste(model: &mut Model, data: String) {
    let clean = strip_paste_markers(&data);
    let count = clean.chars().count();
    if count > PASTE_COLLAPSE_THRESHOLD {
        // Keep the full text in the model; show only a compact placeholder inline.
        let idx = model.pastes.len();
        model.pastes.push(clean);
        let token = format!("[paste #{idx}: {count} chars]");
        model.insert_text(&token);
    } else {
        model.insert_text(&clean);
    }
    model.ac_sel = 0;
}

/// Give a visible picker first refusal on Esc while an agent turn is running.
/// Returning `true` tells the caller not to forward that same keypress to the
/// turn-cancellation path.
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
        // Signal the running kernel before the event loop restores the terminal
        // and exits, so streams/tools get the same cancellation notification as
        // an ordinary Esc stop. Pending gates must be answered to unblock them.
        if let Some(handle) = &model.interrupt {
            handle.cancel_turn();
        }
        model.deny_pending_approvals();
        model.should_quit = true;
        return true;
    }
    false
}

/// Capture one `/model add` form field. This has its own input path so an API
/// key never reaches command history, completion, a transcript entry, or the
/// session event log.
fn handle_model_setup_key<P: ProfileProvider>(
    model: &mut Model,
    key: KeyEvent,
    provider: &P,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) -> bool {
    if model.model_setup.is_none() {
        return false;
    }
    // The provider and discovered-model steps are real arrow-key pickers; let
    // the generic picker handler own navigation while the draft remains alive.
    if matches!(
        model.picker.as_ref().map(|p| &p.kind),
        Some(PickerKind::ModelProtocol)
            | Some(PickerKind::ProviderPreset)
            | Some(PickerKind::ModelDiscovery(_))
    ) {
        return false;
    }

    match key.code {
        KeyCode::Esc => {
            model.model_setup = None;
            model.input.clear();
            model.cursor = 0;
            model.push_notice("model setup cancelled — /model reopens it");
        }
        KeyCode::Backspace => model.backspace(),
        KeyCode::Left => model.move_left(),
        KeyCode::Right => model.move_right(),
        KeyCode::Enter => advance_model_setup(model, provider, tx),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => model.insert_char(c),
        _ => {}
    }
    true
}

pub(super) fn begin_model_setup(model: &mut Model) {
    if model.running {
        model.push_notice("finish or Esc the current turn before adding a model");
        return;
    }
    open_model_setup_quiet(model);
    model.push_notice("Add a model — choose a provider. Esc cancels.");
}

/// Capture one `/search` form field. Like `handle_model_setup_key`, this owns
/// the keyboard while a draft is alive so an API key never reaches command
/// history, completion, the transcript, or the session log. Returns false while
/// the provider picker is up, letting the generic picker handler drive it.
fn handle_search_setup_key(model: &mut Model, key: KeyEvent) -> bool {
    if model.search_setup.is_none() {
        return false;
    }
    if matches!(
        model.picker.as_ref().map(|p| &p.kind),
        Some(PickerKind::SearchProvider)
    ) {
        return false;
    }
    match key.code {
        KeyCode::Esc => {
            model.search_setup = None;
            model.input.clear();
            model.cursor = 0;
            model.push_notice("web-search setup cancelled — /search reopens it");
        }
        KeyCode::Backspace => model.backspace(),
        KeyCode::Left => model.move_left(),
        KeyCode::Right => model.move_right(),
        KeyCode::Enter => advance_search_setup(model),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => model.insert_char(c),
        _ => {}
    }
    true
}

/// Open the `/mode` autonomy picker, cursor on the current level.
pub(super) fn open_mode_picker(model: &mut Model) {
    let sel = AUTONOMY_MODES
        .iter()
        .position(|(l, _)| *l == model.autonomy)
        .unwrap_or(0);
    model.picker = Some(Picker::with_selected(PickerKind::AutonomyMode, sel));
}

/// Set the session autonomy dial live and confirm with a level-appropriate notice
/// (yolo gets a warning glyph — autonomous mode is never silent).
fn set_autonomy(model: &mut Model, level: kernel::AutonomyLevel) {
    model.autonomy = level;
    let note = match level {
        kernel::AutonomyLevel::Careful => {
            "✔ mode: careful — edits and shell ask for approval".to_string()
        }
        kernel::AutonomyLevel::Normal => {
            "✔ mode: normal — edits auto-apply; shell still asks".to_string()
        }
        kernel::AutonomyLevel::Yolo => {
            "⚠ mode: yolo — edits and shell auto-run; only dangerous ops (rm -rf, personal files, deploys) still ask".to_string()
        }
    };
    model.push_notice(note);
}

/// Open the `/search` flow on the provider picker.
pub(super) fn begin_search_setup(model: &mut Model) {
    if model.running {
        model.push_notice("finish or Esc the current turn before changing web search");
        return;
    }
    model.search_setup = Some(SearchSetup::new());
    // Preselect the currently-configured provider so Enter is a no-surprise
    // confirm of the status quo.
    let current = model
        .model_config
        .lock()
        .map(|c| c.search_provider())
        .unwrap_or_default();
    let selected = SEARCH_PROVIDERS
        .iter()
        .position(|(p, _)| *p == current)
        .unwrap_or(0);
    model.picker = Some(Picker::with_selected(PickerKind::SearchProvider, selected));
    model.input.clear();
    model.cursor = 0;
    model.push_notice("Web search — choose a provider. Esc cancels.");
}

/// Advance the `/search` draft after a value is typed in the Secret step
/// (an API key for Tavily/Brave, or the instance URL for SearXNG).
fn advance_search_setup(model: &mut Model) {
    let value = std::mem::take(&mut model.input).trim().to_string();
    model.cursor = 0;
    let Some(setup) = model.search_setup.as_ref() else {
        return;
    };
    match setup.provider {
        tools::SearchProvider::Tavily | tools::SearchProvider::Brave => {
            if value.is_empty() {
                model.push_notice("an API key is required (Esc to cancel)");
                model.input.clear();
                return;
            }
            let provider = setup.provider;
            if let Some(cred_id) = config::search_cred_id(provider) {
                if let Err(e) = config::store_key(cred_id, &value) {
                    model.push_notice(format!("could not store API key: {e}"));
                    return;
                }
            }
            commit_search(model, provider, None);
        }
        tools::SearchProvider::Searxng => {
            if value.is_empty() {
                model.push_notice("a SearXNG instance URL is required (Esc to cancel)");
                model.input.clear();
                return;
            }
            let url = value.trim_end_matches('/').to_string();
            commit_search(model, tools::SearchProvider::Searxng, Some(url));
        }
        // DuckDuckGo never reaches the Secret step; committed straight from the picker.
        tools::SearchProvider::DuckDuckGo => {
            commit_search(model, tools::SearchProvider::DuckDuckGo, None)
        }
    }
}

/// Persist the chosen provider, save config, and update the live search handle
/// the running `web.*` tools read — so the change applies on the next search
/// without a restart.
fn commit_search(model: &mut Model, provider: tools::SearchProvider, searxng_url: Option<String>) {
    model.search_setup = None;
    let saved = match model.model_config.lock() {
        Ok(mut cfg) => {
            cfg.set_search(provider, searxng_url);
            config::save(&cfg)
                .map_err(|e| format!("could not write config: {e}"))
                .map(|_| config::resolve_search(&cfg))
        }
        Err(_) => Err("configuration is temporarily unavailable".into()),
    };
    match saved {
        Ok(settings) => {
            if let Ok(mut live) = model.search.lock() {
                *live = settings;
            }
            model.push_notice(format!(
                "✔ web search now uses {} (DuckDuckGo remains the fallback)",
                provider.label()
            ));
        }
        Err(e) => model.push_notice(e),
    }
}

/// Open the add-model form without pushing a transcript notice. First-run uses
/// this so the welcome identity screen stays visible behind the form.
pub(super) fn open_model_setup_quiet(model: &mut Model) {
    // Protocol is explicit and saved. Only adapters available in this build
    // can continue; planned native routes remain visible without creating an
    // unusable profile.
    model.model_setup = Some(ModelSetup::new());
    model.picker = Some(Picker::new(PickerKind::ModelProtocol));
    model.input.clear();
    model.cursor = 0;
}

fn begin_model_key_update(model: &mut Model, profile: config::ModelProfile) {
    if model.running {
        model.push_notice("finish or Esc the current turn before updating credentials");
        return;
    }
    model.model_setup = Some(ModelSetup::update_key(
        profile.name.clone(),
        profile.provider.protocol,
        profile.provider.base_url,
    ));
    model.input.clear();
    model.cursor = 0;
    model.push_notice(format!(
        "Update API key for '{}' — input is masked; Esc cancels.",
        profile.name
    ));
}

fn advance_model_setup<P: ProfileProvider>(
    model: &mut Model,
    provider: &P,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    let value = std::mem::take(&mut model.input).trim().to_string();
    model.cursor = 0;
    let Some(setup) = model.model_setup.as_mut() else {
        return;
    };
    match setup.step {
        ModelSetupStep::BaseUrl => {
            if value.is_empty() {
                model.push_notice("a base URL is required");
                return;
            }
            setup.base_url = value.trim_end_matches('/').to_string();
            setup.step = ModelSetupStep::ApiKey;
        }
        // The server is being queried; Enter is inert (Esc still cancels).
        ModelSetupStep::Discovering => {}
        ModelSetupStep::ApiKey => {
            if let ModelSetupMode::UpdateKey { profile } = &setup.mode {
                if value.is_empty() {
                    model.push_notice("an API key is required");
                    return;
                }
                let profile = profile.clone();
                let base_url = setup.base_url.clone();
                if let Err(e) = config::store_key(&base_url, &value) {
                    model.push_notice(format!("could not store API key: {e}"));
                    return;
                }
                model.model_setup = None;
                let resolved: Result<config::Resolved, String> = match model.model_config.lock() {
                    Ok(mut cfg) => {
                        if let Some(saved) = cfg.models.get_mut(&profile) {
                            if !saved.auth.requires_credential() {
                                saved.auth = providers::AuthKind::for_protocol(saved.protocol);
                            }
                        }
                        config::save(&cfg)
                            .map_err(|e| format!("could not write config: {e}"))
                            .and_then(|_| {
                                config::resolve_model_with_key(&cfg, &profile, &value)
                                    .map_err(|e| e.to_string())
                            })
                    }
                    Err(_) => Err("model configuration is temporarily unavailable".into()),
                };
                match resolved {
                    Ok(resolved) => {
                        let activate =
                            model.active_profile == profile || model.active_profile == "override";
                        if activate {
                            if let Err(error) = provider.switch_profile(&resolved) {
                                model.push_notice(format!("could not switch model: {error}"));
                                return;
                            }
                            model.model = resolved.provider.model;
                            model.protocol = resolved.provider.protocol;
                            model.reasoning_support = resolved.provider.reasoning;
                            model.max_ctx = resolved.provider.max_ctx;
                            model.ctx_pct = None;
                            model.active_profile = resolved.name;
                        }
                        model.push_notice(if activate {
                            format!("✔ API key updated; switched to '{profile}'")
                        } else {
                            format!("✔ API key updated for '{profile}'")
                        });
                    }
                    Err(e) => model.push_notice(e),
                }
                return;
            }
            if setup.protocol == kernel::Protocol::GeminiInteractions && value.is_empty() {
                model.push_notice("a Gemini API key is required");
                return;
            }
            setup.api_key = value;
            setup.step = ModelSetupStep::Discovering;
            // Ask the endpoint what it serves — picking from a live list beats
            // typing a model id blind (and typos in ids are the #1 setup bug).
            let base_url = setup.base_url.clone();
            let api_key = setup.api_key.clone();
            let protocol = setup.protocol;
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut profile = providers::ProviderProfile::openai_chat(
                    &base_url,
                    "model-discovery",
                    if api_key.is_empty() {
                        providers::AuthKind::None
                    } else {
                        providers::AuthKind::for_protocol(protocol)
                    },
                );
                profile.protocol = protocol;
                let result = providers::openai_compat::list_models_for_profile(&profile, &api_key)
                    .await
                    .map_err(|error| error.to_string());
                let _ = tx.send(TuiEvent::ModelsDiscovered { base_url, result });
            });
        }
        ModelSetupStep::ModelId => {
            if value.is_empty() {
                model.push_notice("a model ID is required");
                return;
            }
            setup.model = value;
            setup.step = ModelSetupStep::ContextWindow;
        }
        ModelSetupStep::ContextWindow => {
            let typed = if value.is_empty() {
                None
            } else {
                match value.parse::<u32>() {
                    Ok(v) if v > 0 => Some(v),
                    _ => {
                        model.push_notice(
                            "context window must be a positive whole number, or blank",
                        );
                        return;
                    }
                }
            };
            setup.max_ctx = setup.max_ctx.or(typed);
            finish_model_setup(model, provider);
        }
    }
}

/// Discovery result for an in-flight add-model draft: a non-empty list opens
/// the model picker; anything else falls back to manual id entry, honestly
/// labelled with the reason.
pub(super) fn on_models_discovered(
    model: &mut Model,
    base_url: String,
    result: Result<Vec<providers::openai_compat::ModelInfo>, String>,
) {
    let Some(setup) = model.model_setup.as_mut() else {
        return;
    };
    // Only the draft that asked may consume this reply (guards against a slow
    // response from an abandoned draft landing in a newer one).
    if !matches!(setup.step, ModelSetupStep::Discovering) || setup.base_url != base_url {
        return;
    }
    match result {
        Ok(models) if !models.is_empty() => {
            model.picker = Some(Picker::new(PickerKind::ModelDiscovery(models)));
        }
        Ok(_) => {
            setup.step = ModelSetupStep::ModelId;
            model.push_notice("the server reported no models — type the model id");
        }
        Err(e) => {
            setup.step = ModelSetupStep::ModelId;
            model.push_notice(format!(
                "model discovery unavailable ({e}) — type the model id"
            ));
        }
    }
}

/// Complete an add-model draft: derive the profile name from the model id
/// (never asked; users pasted model ids into the old name field), persist the
/// key + config, and switch to the new connection.
fn finish_model_setup<P: ProfileProvider>(model: &mut Model, provider: &P) {
    let Some(completed) = model.model_setup.take() else {
        return;
    };
    let profile = config::ProviderConfig {
        protocol: completed.protocol,
        base_url: completed.base_url.clone(),
        model: completed.model.clone(),
        auth: if completed.api_key.is_empty() {
            providers::AuthKind::None
        } else {
            providers::AuthKind::for_protocol(completed.protocol)
        },
        headers: std::collections::BTreeMap::new(),
        max_ctx: completed.max_ctx,
        max_output_tokens: None,
        token_counter: providers::openai_compat::OpenAiTokenCounter::None,
        token_accounting: kernel::TokenAccountingMode::Adaptive,
        reasoning: kernel::ReasoningSupport::Unknown,
    };
    if !completed.api_key.is_empty() {
        if let Err(e) = config::store_key(&completed.base_url, &completed.api_key) {
            model.push_notice(format!("could not store API key: {e}"));
            return;
        }
    }
    let saved: Result<(String, config::Resolved), String> = match model.model_config.lock() {
        Ok(mut cfg) => {
            let name = config::derive_profile_name(&cfg, &completed.model);
            match cfg.add_model(name.clone(), profile, false) {
                Err(e) => Err(format!("could not save model: {e}")),
                Ok(()) => match config::save(&cfg) {
                    Ok(()) => config::resolve_model_with_key(&cfg, &name, &completed.api_key)
                        .map(|r| (name, r))
                        .map_err(|e| e.to_string()),
                    Err(e) => {
                        // Do not leave a session-only ghost profile if the
                        // durable write fails (disk permissions/full disk).
                        cfg.models.remove(&name);
                        Err(format!("could not write config: {e}"))
                    }
                },
            }
        }
        Err(_) => Err("model configuration is temporarily unavailable".into()),
    };
    match saved {
        Ok((name, resolved)) => {
            if let Err(error) = provider.switch_profile(&resolved) {
                model.push_notice(format!("saved '{name}' but could not switch: {error}"));
                return;
            }
            model.model = resolved.provider.model;
            model.protocol = resolved.provider.protocol;
            model.reasoning_support = resolved.provider.reasoning;
            model.max_ctx = resolved.provider.max_ctx;
            model.ctx_pct = None;
            model.active_profile = resolved.name;
            model.push_notice(format!("✔ saved '{name}' and switched to it"));
        }
        Err(e) => model.push_notice(e),
    }
}

/// Handle keyboard input
pub(super) fn handle_key<P, L>(
    model: &mut Model,
    key: KeyEvent,
    kernel: &Arc<Kernel<P, L>>,
    session: &mut Session,
    transcript: &mut Vec<Message>,
    budget: &Budget,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) where
    P: ProfileProvider + 'static,
    L: EventLog + 'static,
{
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Ctrl-D is the unconditional escape hatch. Keep it ahead of every modal
    // handler so no picker, credential form, or approval can trap the user.
    if handle_global_key(model, &key) {
        return;
    }

    // A visible picker owns the first Esc, even while a turn is running. This
    // lets `/reasoning` behave like a normal dismissible panel; a second Esc,
    // now that no picker is open, interrupts the turn. Without this ordering the
    // global running-turn handler swallowed Esc and left the panel stuck open.
    if dismiss_running_picker_on_esc(model, &key) {
        return;
    }

    // A `clarify` question form owns all input while it's up. It appears mid-turn
    // (the tool awaits an answer), so intercept BEFORE the Esc→cancel path: here
    // Esc dismisses the form (agent proceeds on best judgment), it never kills
    // the turn.
    if model.clarify.is_some() {
        handle_clarify_key(model, key);
        return;
    }

    // With no picker open, Esc is reserved for stopping a running turn, not for
    // answering an approval. Intercept it before the approval branch so it
    // interrupts instead of silently denying whichever prompt is on screen.
    // The cancel is GRACEFUL: the kernel lets in-flight tools settle (bounded)
    // and returns Done with StopReason::Interrupted.
    if key.code == KeyCode::Esc && model.running {
        if let Some(h) = &model.interrupt {
            h.cancel_turn();
            if !model.cancelling {
                model.cancelling = true;
                model.push_notice("⏹ stopping — letting in-flight tools settle…");
            }
        }
        // Answering the pending approvals unblocks any gate the kernel is
        // waiting on, so the cancel settles immediately (K8, by design).
        model.deny_pending_approvals();
        // A picker may have been suppressed while the approval card was shown;
        // do not let it unexpectedly reappear after cancellation.
        model.picker = None;
        return;
    }

    // Inline approval handling (PART 3) — input captured when last item is approval
    if model.pending_approval().is_some() {
        handle_approval_key(model, key);
        return;
    }

    if handle_model_setup_key(model, key, kernel.provider.as_ref(), tx) {
        return;
    }

    if handle_search_setup_key(model, key) {
        return;
    }

    if handle_memory_picker_key(model, &key, kernel, session, tx) {
        return;
    }
    if handle_reasoning_picker_key(model, key, kernel.provider.as_ref()) {
        return;
    }

    // Picker handling
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
                    mcp_set_disabled(model, &row.id, !row.disabled, tx);
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
                    open_mcp_tools(model, &id, tx);
                }
                return;
            }
            // `d` stops the selected sub-agent. Only a running one — a finished
            // writer's patch is not something you can cancel.
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
                    Some(session) => agents_stop(model, &session, tx),
                    None => model.push_notice("that agent has already finished"),
                }
                return;
            }
            // `a` applies the selected patch; `A` applies one whose build
            // failed. Two keys rather than a prompt, so the override is a
            // deliberate act and never a reflex Enter on a confirmation.
            KeyCode::Char(key @ ('a' | 'A')) if matches!(&picker.kind, PickerKind::Agents(_)) => {
                let session = match &picker.kind {
                    PickerKind::Agents(rows) => match rows.get(picker.selected) {
                        Some(AgentRow::Patch { session, .. }) => Some(session.clone()),
                        _ => None,
                    },
                    _ => None,
                };
                match session {
                    Some(session) => agents_apply_patch(model, &session, key == 'A', tx),
                    // Say what this row *can* do rather than only what it
                    // cannot — a running agent has a transcript to watch, which
                    // is usually what someone reaching for a key here wants.
                    None => model.push_notice(
                        "nothing to apply on this row — press Enter to watch what it is doing",
                    ),
                }
                return;
            }
            // Enter on a patch row shows the diff, so applying is never a blind
            // choice between two words on a row.
            KeyCode::Enter | KeyCode::Right if matches!(&picker.kind, PickerKind::Agents(_)) => {
                let session = match &picker.kind {
                    PickerKind::Agents(rows) => match rows.get(picker.selected) {
                        Some(AgentRow::Patch { session, .. }) => Some(session.clone()),
                        _ => None,
                    },
                    _ => None,
                };
                match session {
                    Some(session) => agents_view_patch(model, &session, tx),
                    // No diff on this row — but a running or finished child has
                    // a live transcript, and "what is it actually doing" is the
                    // question the panel gets asked most. Stop-or-nothing was
                    // the wrong answer to it.
                    None => {
                        let watching = match &picker.kind {
                            PickerKind::Agents(rows) => match rows.get(picker.selected) {
                                Some(AgentRow::Agent { agent, .. }) => Some(agent.session.clone()),
                                _ => None,
                            },
                            _ => None,
                        };
                        match watching {
                            Some(session) => agents_view_transcript(model, &session, tx),
                            None => model.picker = None,
                        }
                    }
                }
                return;
            }
            // `d` removes the selected server in the /mcp picker (row 0 is Add).
            KeyCode::Char('d') if matches!(&picker.kind, PickerKind::Mcp(_)) => {
                let id = if let PickerKind::Mcp(rows) = &picker.kind {
                    (picker.selected > 0)
                        .then(|| rows.get(picker.selected - 1).map(|row| row.id.clone()))
                        .flatten()
                } else {
                    None
                };
                if let Some(id) = id {
                    mcp_remove(model, &id, tx);
                }
                return;
            }
            // → mirrors Enter and ← mirrors Esc, so the pickers navigate like
            // a nested menu: right descends, left backs out one level.
            KeyCode::Enter | KeyCode::Right => {
                if matches!(picker.kind, PickerKind::ModelProtocol) {
                    let selected = picker.selected;
                    match selected {
                        0 => {
                            if let Some(setup) = model.model_setup.as_mut() {
                                setup.protocol = kernel::Protocol::OpenAiChat;
                            }
                            model.picker = Some(Picker::new(PickerKind::ProviderPreset));
                        }
                        2 => {
                            model.picker = None;
                            if let Some(setup) = model.model_setup.as_mut() {
                                setup.protocol = kernel::Protocol::GeminiInteractions;
                                setup.base_url =
                                    "https://generativelanguage.googleapis.com/v1".into();
                                setup.step = ModelSetupStep::ApiKey;
                            }
                            model.push_notice(
                                "Gemini Interactions v1 — enter the Gemini API key",
                            );
                        }
                        _ => model.push_notice(
                            "that native protocol is shown for roadmap clarity but its adapter is not implemented yet",
                        ),
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
                // `/mode`: a level was chosen — set it live and close.
                if matches!(picker.kind, PickerKind::AutonomyMode) {
                    let selected = picker.selected;
                    model.picker = None;
                    if let Some((level, _)) = AUTONOMY_MODES.get(selected).copied() {
                        set_autonomy(model, level);
                    }
                    return;
                }
                // `/theme`: a theme was chosen — apply it live and close.
                if matches!(picker.kind, PickerKind::Theme) {
                    let selected = picker.selected;
                    model.picker = None;
                    if let Some((id, _)) = THEME_MODES.get(selected).copied() {
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
                // Discovery step 2: a live model was chosen (or manual entry).
                // With a server-reported context window the profile completes
                // right here — name derived, saved, switched.
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
                                finish_model_setup(model, kernel.provider.as_ref());
                            } else {
                                model.push_notice(
                                    "the server didn't report a context window — enter it in tokens (blank = unknown)",
                                );
                            }
                        }
                        // Trailing "Type a model id manually…" row.
                        None => {
                            if let Some(setup) = model.model_setup.as_mut() {
                                setup.step = ModelSetupStep::ModelId;
                            }
                        }
                    }
                    return;
                }
                // Session picker: fetch the selected session's events and replay
                // them into the transcript.
                if let PickerKind::Session(sessions) = &picker.kind {
                    if let Some(meta) = sessions.get(picker.selected) {
                        let id = meta.id;
                        let log = kernel.log.clone();
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            let events = log.events(id).await;
                            let msgs = kernel::project_messages(&events);
                            let _ = tx.send(TuiEvent::Resumed(id, msgs, events));
                        });
                        model.picker = None;
                        model.push_notice(format!("(loading session {id} …)"));
                        return;
                    }
                }
                // Rewind step 1: a cut point was chosen → open the scope menu
                // (conversation only · + code · cancel). No async work yet.
                if let PickerKind::Rewind(points) = &picker.kind {
                    if let Some(point) = points.get(picker.selected).cloned() {
                        model.picker = Some(Picker::new(PickerKind::RewindMode(point)));
                        return;
                    }
                }
                // Rewind step 2: a scope was chosen → branch the session and,
                // for the "+ code" scope, roll the workspace back. `None` = cancel
                // (return to no picker; the user can re-open with /rewind).
                if let PickerKind::RewindMode(point) = &picker.kind {
                    let scope = point
                        .scope_options()
                        .get(picker.selected)
                        .and_then(|(_, s)| *s);
                    let at_event = point.at_event; // Copy — ends the picker borrow
                    match scope {
                        Some(scope) => {
                            let restore = model.restore.clone();
                            model.picker = None;
                            spawn_rewind(kernel, restore, session.id, at_event, scope, tx);
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
                        spawn_memory_provenance(entry, kernel, tx);
                        model.push_notice(format!("(opening memory '{name}' provenance …)"));
                    }
                    return;
                }
                // Tool browser: the last row goes back, every other row toggles.
                if let PickerKind::McpTools { id, tools } = &picker.kind {
                    let (id, sel) = (id.clone(), picker.selected);
                    match tools.get(sel).cloned() {
                        Some((name, on)) => mcp_set_tool(model, &id, &name, !on),
                        None => open_mcp_picker(model),
                    }
                    return;
                }
                // Credential choice for a server the probe could not classify.
                if let PickerKind::McpAuth { id, url } = &picker.kind {
                    let (id, url, sel) = (id.clone(), url.clone(), picker.selected);
                    model.picker = None;
                    mcp_choose_auth(model, &id, &url, sel, tx);
                    return;
                }
                // `/mcp` picker: row 0 adds a server; any other row connects it.
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
                    if let (Some(id), Some(manager)) = (id, model.mcp.clone()) {
                        model.push_notice(format!("(connecting MCP server '{id}' …)"));
                        connect_mcp_server(manager, id, tx.clone());
                    }
                    return;
                }
                // Skill hub: the top rows are actions, the rest are installed
                // skills. An action runs (or prefills a command needing input); a
                // skill row force-loads its procedure (same as `/skill <name>`).
                if let PickerKind::Skill(skills) = &picker.kind {
                    let sel = picker.selected;
                    let n_actions = SKILL_HUB_ACTIONS.len();
                    let action = (sel < n_actions).then(|| SKILL_HUB_ACTIONS[sel].1);
                    let skill_name = skills
                        .get(sel.wrapping_sub(n_actions))
                        .map(|(n, _)| n.clone());
                    model.picker = None;
                    match action {
                        // "Add a skill" immediately opens the scrollable catalog
                        // (browse all from your sources) — arrow-keys → Enter to
                        // install. To filter or install a link directly, type
                        // `/skill add <word|link>`.
                        Some("add") => search_skills(model, "", tx),
                        // Power operations live one layer deep, not on the main path.
                        Some("manage") => {
                            model.picker = Some(Picker::new(PickerKind::SkillManage));
                        }
                        _ => {
                            if let Some(name) = skill_name {
                                load_skill_by_name(model, &name, transcript);
                            }
                        }
                    }
                    return;
                }
                // Manage sub-menu: run the chosen power operation, or go back.
                if matches!(&picker.kind, PickerKind::SkillManage) {
                    let id = SKILL_MANAGE_ACTIONS.get(picker.selected).map(|(_, id)| *id);
                    model.picker = None;
                    match id {
                        Some("update") => update_skills(model, "", tx),
                        Some("sources") => open_sources_picker(model),
                        Some("lock") => lock_skills(model),
                        Some("sync") => sync_skills(model, tx),
                        _ => open_skill_picker(model), // Back (or anything unknown)
                    }
                    return;
                }
                // Sources sub-picker: Add / Remove <source> / Back.
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
                        remove_user_skill(model, &name, transcript);
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
                        install_skill(model, &url, tx);
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
                        switch_saved_model(model, kernel.provider.as_ref(), &name);
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
                    model.picker = None;
                    let has_models = model
                        .model_config
                        .lock()
                        .map(|c| !c.models.is_empty())
                        .unwrap_or(false);
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
                    model.push_notice("web-search setup cancelled — /search reopens it");
                } else {
                    model.picker = None;
                }
            }
            _ => {}
        }
        return;
    }

    // Autocomplete handling
    if model.input.starts_with('/') {
        let matches = command_matches(&model.input);
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
                KeyCode::Enter
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    let cmd = matches[model.ac_sel].0.trim_start_matches('/').to_string();
                    model.input.clear();
                    model.cursor = 0;
                    model.ac_sel = 0;
                    dispatch_slash(model, &cmd, kernel, session, transcript, tx);
                    return;
                }
                _ => {}
            }
        }
    }

    match key.code {
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
            if let Some(h) = &model.interrupt {
                h.cancel_turn();
            }
        }
        // Shift/Alt+Enter or Ctrl+J for newline (PART 2)
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
        KeyCode::Enter => {
            // Backslash line-continuation
            if !model.running && model.cursor > 0 && model.input[..model.cursor].ends_with('\\') {
                model.input.remove(model.cursor - 1);
                model.input.insert(model.cursor - 1, '\n');
                return;
            }
            // Slash commands — only when the first token IS a known command;
            // a pasted path ("/Users/… do X") goes to the model as chat.
            if is_slash_command(&model.input) {
                let line = std::mem::take(&mut model.input);
                model.cursor = 0;
                model.ac_sel = 0;
                model.history.push(line.clone());
                model.history_idx = None;
                let cmd = line.trim_start_matches('/').trim();
                dispatch_slash(model, cmd, kernel, session, transcript, tx);
                return;
            }
            if model.input.trim().is_empty() {
                return;
            }
            let raw = std::mem::take(&mut model.input);
            model.cursor = 0;
            model.history.push(raw.clone());
            model.history_idx = None;
            model.welcome = false;
            // Expand any collapsed pastes before the agent sees the line (PART 2).
            let line = model.resolve_pastes(&raw);

            if model.running {
                // Mid-turn steer: the kernel injects it at the next turn
                // boundary (same session, model sees it immediately after the
                // current tools settle). Never reaches here with an approval
                // card open — the approval branch owns Enter above.
                if let Some(h) = &model.interrupt {
                    h.steer(line.clone());
                    let preview: String = raw.chars().take(60).collect();
                    model.push_notice(format!("↳ queued for this task: {preview}"));
                } else {
                    model.push_notice("(turn is finishing — try again in a moment)");
                }
            } else {
                spawn_turn(model, kernel, session, transcript, budget, tx, Some(line));
            }
        }
        KeyCode::Backspace => model.backspace(),
        KeyCode::Left => model.move_left(),
        KeyCode::Right => model.move_right(),
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

/// Handle approval keyboard input (inline, PART 3)
pub(super) fn handle_approval_key(model: &mut Model, key: KeyEvent) {
    // Reject selection input until the card's options are actually on screen —
    // stops a blind Enter (queued behind stream backlog) from confirming an
    // approval the user never saw.
    if !model.approval_ready {
        return;
    }
    let sel: Option<usize> = match key.code {
        KeyCode::Char('1') | KeyCode::Char('y') | KeyCode::Char('Y') => Some(0),
        KeyCode::Char('2') | KeyCode::Char('a') | KeyCode::Char('A') => Some(1),
        // Esc is intentionally NOT a deny: it is intercepted at the top of the
        // key handler to cancel a running turn. Denial requires an explicit 'n'
        // or '3' — no accidental rejections from a reflexive Esc.
        KeyCode::Char('3') | KeyCode::Char('n') | KeyCode::Char('N') => Some(2),
        KeyCode::Enter => Some(model.approval_sel),
        KeyCode::Up => {
            model.approval_sel = model.approval_sel.checked_sub(1).unwrap_or(2);
            model.dirty = true;
            return;
        }
        KeyCode::Down => {
            model.approval_sel = (model.approval_sel + 1) % 3;
            model.dirty = true;
            return;
        }
        _ => None,
    };
    if let Some(choice) = sel {
        if let Some(pending) = model.pending_approvals.pop_front() {
            // Card is about to leave the screen; the next queued approval (if any)
            // will render on the next frame and re-arm `approval_ready` then.
            model.approval_ready = false;
            model.approval_sel = 0;
            let decision = match choice {
                0 => kernel::Approval::Once,
                1 => kernel::Approval::Always,
                _ => kernel::Approval::Deny,
            };
            // "Always" for a tool means don't re-ask this session; for a path the
            // permission layer persists it to medha.lock (PART 1). A trust-flow
            // escalated action is NEVER remembered — each web-tainted action is
            // reviewed afresh (K9), so treat its "always" as a one-time approve.
            if choice == 1 && !pending.escalated {
                model.auto_approve.insert(pending.action.clone());
            }
            let _ = pending.responder.send(decision);
            let verb = match choice {
                0 => "approved",
                1 => "approved (allowing all this session)",
                _ => "rejected",
            };
            model.push_notice(format!("{verb} {}", pending.action));
        }
    }
}

/// Keyboard input for the `clarify` question form. Owns all keys while a form is
/// up. Rows per question = options, then "✎ Other…". Space toggles (multi) or
/// selects (single); Enter on a radio option selects it before submitting, Enter
/// on Other opens free text, and Esc dismisses the whole form (agent proceeds).
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
            if !multi && cursor < other_row {
                if let Some(s) = model.clarify.as_mut() {
                    toggle_option(s, cursor, false);
                    s.validation = None;
                }
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

/// Toggle (multi) or set (single) an option in the current question's draft.
/// For a radio (single-select) question, options and "Other" are mutually
/// exclusive — picking an option clears any typed Other, so the answer is never
/// self-contradictory.
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

/// Submit every question's draft as the answer set and close the form, echoing
/// the choices into the transcript so the user has a record of what they answered.
fn submit_clarify(model: &mut Model) {
    // A radio question promises exactly one answer. Keep the form open and move
    // focus to the first incomplete question instead of returning an ambiguous
    // empty selection to the agent.
    if let Some(state) = model.clarify.as_mut() {
        if let Some(i) = state.questions.iter().enumerate().find_map(|(i, q)| {
            let d = &state.drafts[i];
            (!q.multi_select && d.selected.is_empty() && d.other.is_none()).then_some(i)
        }) {
            state.idx = i;
            state.cursor = 0;
            let label = state.questions[i].header.trim();
            state.validation = Some(if label.is_empty() {
                "Choose one option or enter an Other answer before submitting.".to_string()
            } else {
                format!("Choose an answer for {label}, or enter Other.")
            });
            model.dirty = true;
            return;
        }
    }

    if let Some(state) = model.clarify.take() {
        // Human-readable summary, per question: "Header: pick, pick (“other”)".
        let mut parts = Vec::new();
        for (q, d) in state.questions.iter().zip(state.drafts.iter()) {
            let label = if q.header.trim().is_empty() {
                q.prompt.trim().to_string()
            } else {
                q.header.trim().to_string()
            };
            let mut picks: Vec<String> = d
                .selected
                .iter()
                .filter_map(|&i| q.options.get(i))
                .map(|o| o.label.clone())
                .collect();
            if let Some(o) = d.other.as_ref().filter(|s| !s.is_empty()) {
                picks.push(format!("“{o}”"));
            }
            let val = if picks.is_empty() {
                "—".to_string()
            } else {
                picks.join(", ")
            };
            parts.push(format!("{label}: {val}"));
        }
        let summary = format!("✔ answered — {}", parts.join(" · "));
        let answers = state.answers();
        let _ = state.responder.send(Some(answers));
        model.push_notice(summary);
        model.dirty = true;
    }
}

/// Dismiss the whole form; the tool receives `None` → the agent proceeds.
pub(super) fn cancel_clarify(model: &mut Model) {
    if let Some(state) = model.clarify.take() {
        let _ = state.responder.send(None);
        model.push_notice("clarify dismissed — proceeding on best judgment");
        model.dirty = true;
    }
}

/// Handle agent events. `session` is swapped when a `/resume` completes.
pub(super) fn handle_agent_event(
    model: &mut Model,
    ev: TuiEvent,
    session: &mut Session,
    transcript: &mut Vec<Message>,
) {
    match ev {
        // Record only. Starting the turn needs the kernel, which this handler
        // does not have; the tick drives it. Both run on the UI thread, so the
        // flag cannot be read between being set and being acted on.
        TuiEvent::AgentReportReady => model.agent_report_deferred = true,
        TuiEvent::ToolStarted(tool, target) => model.current_tool = Some((tool, target)),
        TuiEvent::Text(delta) => {
            model.current_tool = None;
            model.push_text_delta(&delta);
        }
        TuiEvent::Reasoning(delta) => model.push_thinking_delta(&delta),
        TuiEvent::ToolCall(tool, args) => {
            model.current_tool = None;
            model.push_item(Item::ToolCall { tool, args });
        }
        TuiEvent::ToolResult(tool, ok, payload) => {
            model.current_tool = None;
            model.push_item(Item::ToolResult { tool, ok, payload });
        }
        TuiEvent::Compaction(before, after, summarized, summary) => {
            model.compacting = false;
            model.push_item(Item::Compaction {
                before,
                after,
                summarized,
                summary,
            });
        }
        TuiEvent::Compacting(active) => model.compacting = active,
        TuiEvent::Usage(prompt_tokens, _total) => {
            if let Some(mc) = model.max_ctx {
                let usable = context::ContextBudget::from_max_ctx(mc).usable().max(1);
                model.ctx_pct = Some((prompt_tokens as f32 / usable as f32 * 100.0).round() as u32);
            }
        }
        TuiEvent::Cost(usd, indicative) => model.cost_usd = Some((usd, indicative)),
        TuiEvent::Verify(ok, summary) => model.push_item(Item::Verify { ok, summary }),
        TuiEvent::Approval(action, detail, escalated, responder) => {
            // Auto-approve only a previously "always"-ed action, and NEVER a
            // trust-flow-escalated one (a web-tainted action is always reviewed
            // afresh — approving one shell command must not wave through a later
            // web-derived one, K9).
            if !escalated && model.auto_approve.contains(&action) {
                let _ = responder.send(kernel::Approval::Once);
            } else {
                tracing::debug!(action = %action, escalated, "approval created");
                // Queue, don't clobber: the kernel runs tool calls concurrently
                // (buffered up to `max_parallel_tools`), so several `confirm()`
                // requests can arrive in the same turn. Replacing a pending one
                // would drop its `oneshot::Sender` and the kernel would read that
                // as `Approval::Deny` (the spurious "rejected by human").
                let was_empty = model.pending_approvals.is_empty();
                model.pending_approvals.push_back(PendingApproval {
                    action,
                    detail,
                    escalated,
                    responder,
                });
                if was_empty {
                    model.approval_sel = 0;
                    model.approval_ready = false;
                    model.dirty = true;
                    model.scroll_to_bottom();
                }
            }
        }
        TuiEvent::Clarify(questions, responder) => {
            // One form at a time. If somehow another is up, decline the new one.
            if model.clarify.is_some() || questions.is_empty() {
                let _ = responder.send(None);
            } else {
                // Pre-select the recommended option for radio questions, so a
                // single-question form is one Enter away from accepting the
                // suggested answer.
                let drafts: Vec<ClarifyDraft> = questions
                    .iter()
                    .map(|q| {
                        let mut d = ClarifyDraft::default();
                        if !q.multi_select {
                            if let Some(i) = q.options.iter().position(|o| o.recommended) {
                                d.selected = vec![i];
                            }
                        }
                        d
                    })
                    .collect();
                let cursor = drafts
                    .first()
                    .and_then(|d: &ClarifyDraft| d.selected.first())
                    .copied()
                    .unwrap_or(0);
                model.clarify = Some(ClarifyState {
                    questions,
                    idx: 0,
                    drafts,
                    cursor,
                    entering_other: false,
                    other_input: String::new(),
                    other_cursor: 0,
                    validation: None,
                    responder,
                });
                model.dirty = true;
                model.scroll_to_bottom();
            }
        }
        TuiEvent::Done(updated, reason) => {
            *transcript = updated;
            model.last_turn_reasoning_received = Some(model.reasoning_received_this_turn);
            model.running = false;
            model.current_tool = None;
            model.turn_started = None;
            model.interrupt = None;
            model.cancelling = false;
            model.deny_pending_approvals();
            match reason {
                StopReason::Budget(stop) => {
                    model.push_notice(format!("(stopped: {} reached)", stop.label()));
                }
                StopReason::Interrupted => {
                    // The kernel settled in-flight tools before returning —
                    // the transcript above is the consistent, resumable truth.
                    model.push_notice("⏹ stopped — in-flight work settled");
                }
                StopReason::Finished => {}
            }
        }
        TuiEvent::Error(e) => {
            model.last_turn_reasoning_received = Some(model.reasoning_received_this_turn);
            model.push_notice(format!("error: {e}"));
            model.running = false;
            model.current_tool = None;
            model.turn_started = None;
            model.interrupt = None;
            model.cancelling = false;
            model.deny_pending_approvals();
        }
        TuiEvent::LspStatus(result) => {
            let text = match result {
                Err(error) => format!("LSP: disabled or unavailable\n  {error}"),
                Ok(payload) => {
                    let servers = payload.get("servers").and_then(serde_json::Value::as_array);
                    match servers {
                        Some(servers) if !servers.is_empty() => {
                            let mut lines = vec!["LSP sessions:".to_string()];
                            for server in servers {
                                let name = server
                                    .get("server")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("language-server");
                                let state = server
                                    .get("state")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("unknown");
                                let root = server
                                    .get("root")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("?");
                                lines.push(format!("  {name} [{state}]  {root}"));
                                if let Some(detail) =
                                    server.get("detail").and_then(serde_json::Value::as_str)
                                {
                                    lines.push(format!("    {detail}"));
                                }
                            }
                            lines.push(lsp_inventory_line(&payload));
                            lines.join("\n")
                        }
                        // "No server started" is true but useless on its own: it
                        // reads the same whether nothing has been asked yet or
                        // nothing is installed, which is exactly the question
                        // someone typing /lsp is asking.
                        _ => format!(
                            "LSP: enabled, no server started yet\n{}",
                            lsp_inventory_line(&payload)
                        ),
                    }
                }
            };
            model.upsert_notice("LSP", text);
        }
        // The durable read finished. Only applied if the panel is still the one
        // that asked — the user may have moved on, and replacing a picker they
        // opened since would yank the list out from under them.
        TuiEvent::AgentRows(rows) => {
            if let Some(picker) = model.picker.as_mut()
                && matches!(picker.kind, PickerKind::Agents(_))
            {
                // Keep the cursor where the user put it, clamped to the new
                // list — a refresh that jumps the selection back to the top can
                // turn a considered `a` into one on the wrong row.
                picker.selected = picker.selected.min(rows.len().saturating_sub(1));
                picker.kind = PickerKind::Agents(rows);
                model.dirty = true;
            }
        }
        TuiEvent::AgentPatchAction(outcome) => match outcome {
            Ok(text) | Err(text) => model.push_notice(text),
        },
        TuiEvent::McpStatus(result) => {
            let text = match result {
                Err(error) => format!("MCP: {error}"),
                Ok(payload) => {
                    let servers = payload.get("servers").and_then(serde_json::Value::as_array);
                    match servers {
                        Some(servers) if !servers.is_empty() => {
                            let mut lines = vec!["MCP servers:".to_string()];
                            for server in servers {
                                let name = server
                                    .get("server")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("server");
                                let state = server
                                    .get("state")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("unknown");
                                let tools = server
                                    .get("tools")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(0);
                                let hidden = server
                                    .get("hidden")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(0);
                                let filtered = match hidden {
                                    0 => String::new(),
                                    n => format!(", {n} filtered"),
                                };
                                lines.push(format!(
                                    "  {name} [{state}]  {tools} tool(s){filtered}"
                                ));
                                if let Some(detail) =
                                    server.get("detail").and_then(serde_json::Value::as_str)
                                {
                                    lines.push(format!("    {detail}"));
                                }
                            }
                            lines.push("  · /mcp start <id> to connect · medha mcp add … to add".into());
                            lines.join("\n")
                        }
                        _ => "MCP: no servers configured — add one with `medha mcp add <id> -- <command>`".to_string(),
                    }
                }
            };
            model.upsert_notice("MCP", text);
        }
        TuiEvent::McpTools { server, tools } => {
            if tools.is_empty() {
                model.push_notice(format!("'{server}' exposes no tools"));
            } else {
                model.picker = Some(Picker::new(PickerKind::McpTools { id: server, tools }));
            }
        }
        TuiEvent::McpNeedsAuth { server, url } => {
            model.picker = Some(Picker::new(PickerKind::McpAuth { id: server, url }));
        }
        TuiEvent::McpAuthUrl { server, url } => {
            model.upsert_notice(
                "MCP",
                format!(
                    "MCP: signing in to '{server}' — a browser should have opened.\n  \
                     If not, open this URL:\n  {url}"
                ),
            );
        }
        // A queued steer reached its turn boundary: promote the "queued"
        // notice to a real user line (that's what the model now sees).
        TuiEvent::Steered(text) => {
            model.remove_last_notice("↳ queued for this task:");
            model.push_item(Item::User(text));
        }
        // Steers the session never applied (cancel/finish raced them): give
        // the text back to the input box — typed text must never vanish.
        TuiEvent::SteersReturned(texts) => {
            model.remove_last_notice("↳ queued for this task:");
            let restored = texts.join("\n");
            if !model.input.is_empty() {
                model.input.push('\n');
            }
            model.input.push_str(&restored);
            model.cursor = model.input.len();
            model.push_notice("(queued message returned to the input box — not sent)");
        }
        // `/resume` list arrived from the log — open the session picker.
        TuiEvent::SessionsLoaded(sessions) => {
            if sessions.is_empty() {
                model.push_notice("(no saved sessions in this workspace)");
            } else {
                model.picker = Some(Picker::new(PickerKind::Session(sessions)));
            }
        }
        // A past session's events were replayed — swap session id, rebuild the
        // transcript (keeping the system message at [0]), and repaint the items.
        TuiEvent::Resumed(id, msgs, memory_events) => {
            session.id = id;
            // Preserve transcript[0] (the system prompt); replace the rest.
            let mut system = transcript
                .first()
                .cloned()
                .unwrap_or_else(|| Message::system(""));
            if memory_events
                .first()
                .is_some_and(|event| event.provenance.source == "fork")
            {
                refresh_branch_memory(model, &mut system, memory_events);
            }
            transcript.clear();
            transcript.push(system);
            transcript.extend(msgs.clone());
            repaint_history(model, &msgs);
            model.reasoning_received_this_turn = false;
            model.last_turn_reasoning_received = None;
            model.push_notice(format!("(resumed session {id})"));
        }
        // `/skill install` finished (async when fetching a URL).
        TuiEvent::SkillInstalled(result) => match result {
            Ok(report) => {
                model.refresh_skill_manifest(transcript);
                let revision = report
                    .revision
                    .as_deref()
                    .map(|sha| format!(" @ {}", &sha[..sha.len().min(8)]))
                    .unwrap_or_default();
                let verb = if report.replaced {
                    "updated"
                } else {
                    "installed"
                };
                let effective_scope = model.skills.as_ref().and_then(|store| {
                    store
                        .discover(&model.known_tools)
                        .effective()
                        .find(|listing| listing.skill.name == report.name)
                        .map(|listing| listing.skill.scope)
                });
                let shadowing = if effective_scope == Some(tools::SkillScope::Project) {
                    "\n  Note: a project skill with this name takes precedence; the user copy is installed but inactive."
                } else {
                    ""
                };
                // A caution verdict installs but must be shown — the package
                // carried dual-use content the user should know about.
                let caution = if report.scan_verdict == "caution" {
                    let mut w = String::from("\n  ⚠ guard flagged (review before use):");
                    for f in &report.scan_findings {
                        w.push_str(&format!("\n    · {f}"));
                    }
                    w
                } else {
                    String::new()
                };
                model.push_notice(format!(
                    "✔ {verb} skill '{}' — {} files · {}{}\n  Load: /skill {} · Inspect: /skill info {}{shadowing}{caution}",
                    report.name,
                    report.files,
                    human_bytes(report.bytes),
                    revision,
                    report.name,
                    report.name,
                ));
            }
            Err(e) => model.push_notice(format!("skill install failed: {e}")),
        },
        // `/skill search` finished — open a results picker (Enter installs the
        // selection through the guard-gated installer), matching the app's
        // picker idiom instead of dumping copy-paste commands.
        TuiEvent::SkillSearchResults(result) => match result {
            Ok(res) => {
                for e in &res.errors {
                    model.push_notice(format!("⚠ skill source error — {e}"));
                }
                if res.hits.is_empty() {
                    // No catalog matches, but the picker still opens on its
                    // "Install from a link" row — never a dead end.
                    hub_notice(model, "no skills matched — install from a link, or Esc");
                }
                model.picker = Some(Picker::new(PickerKind::SkillSearch(res.hits)));
            }
            Err(e) => model.push_notice(format!("skill search failed: {e}")),
        },
        // `/skill update` finished — show the per-skill status/outcome rows.
        TuiEvent::SkillUpdateReport(lines) => {
            model.refresh_skill_manifest(transcript);
            model.push_notice(lines.join("\n"));
        }
        // Model setup finished querying /v1/models — show the picker or fall
        // back to manual entry.
        TuiEvent::ModelsDiscovered { base_url, result } => {
            on_models_discovered(model, base_url, result)
        }
        // `/rewind` cut points arrived from the log — open the rewind picker.
        TuiEvent::RewindPointsLoaded(points) => {
            if points.is_empty() {
                model.push_notice("(nothing to rewind to — no earlier turns in this session)");
            } else {
                model.picker = Some(Picker::new(PickerKind::Rewind(points)));
            }
        }
        TuiEvent::MemoryProvenance(entry, provenance) => {
            let mut notice = format!(
                "memory {}  [{} · {}]\n{}\n\nclaim:\n{}",
                entry.name,
                entry.trust.as_str(),
                entry.confidence.as_str(),
                entry.description,
                entry.claim
            );
            match provenance {
                Some(event) => notice.push_str(&format!(
                    "\n\nprovenance jump:\n  event {} · session {} · {}\n  {}",
                    event.id,
                    event.session_id,
                    event.kind.as_str(),
                    event.payload
                )),
                None => notice.push_str("\n\nprovenance event is not available locally"),
            }
            model.push_notice(notice);
        }
        // A rewind finished. For conversation scopes `new_id` is the forked
        // branch: swap to it, rebuild the transcript (keeping the system message
        // at [0]), and drop the chosen prompt back into the input box to edit or
        // re-send (edit-and-resubmit). The original session stays in the log
        // (non-destructive). Code-only (`new_id == None`) leaves the conversation
        // untouched and just reports the files reverted.
        TuiEvent::Rewound {
            new_id,
            msgs,
            memory_events,
            rolled,
            scope,
            prefill,
        } => {
            // "tracked" is honest (K18): only snapshot-carrying writes revert —
            // files mutated via shell (`sed -i`, `git checkout`) are not rolled back.
            let files = |n: usize| {
                if n == 1 {
                    "1 tracked file".to_string()
                } else {
                    format!("{n} tracked files")
                }
            };
            if let Some(id) = new_id {
                session.id = id;
                let mut system = transcript
                    .first()
                    .cloned()
                    .unwrap_or_else(|| Message::system(""));
                refresh_branch_memory(model, &mut system, memory_events);
                transcript.clear();
                transcript.push(system);
                transcript.extend(msgs.clone());
                repaint_history(model, &msgs);
                // Prefill the prompt so the user can tweak it and re-send.
                if let Some(text) = prefill {
                    model.input = text;
                    model.cursor = model.input.len();
                }
            }
            let note = match scope {
                RewindScope::ConversationAndCode => format!(
                    "(rewound · {} rolled back · prompt ready to edit · new branch, original kept)",
                    files(rolled)
                ),
                RewindScope::Conversation => {
                    "(rewound · code kept · prompt ready to edit · new branch, original kept)"
                        .to_string()
                }
                RewindScope::Code => {
                    format!("({} rolled back · conversation kept)", files(rolled))
                }
            };
            model.push_notice(note);
        }
    }
}

/// Rebuild the visible transcript items from a projected message list.
/// Used when resuming a past session: the replayed conversation replaces the
/// on-screen items. User text → `Item::User`; assistant text → `Item::Assistant`,
/// each tool call → `Item::ToolCall`, and each tool result → `Item::ToolResult`
/// — a resumed session shows what the agent did AND what came back, exactly
/// like the live view (results were silently dropped here once).
pub(super) fn repaint_history(model: &mut Model, msgs: &[Message]) {
    model.items.clear();
    // Tool results reference their call by id; remember each call's tool name
    // so the result row carries the right icon/label.
    let mut call_tools: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for m in msgs {
        match m.role {
            kernel::Role::User => model.push_item(Item::User(m.content.clone())),
            kernel::Role::Assistant => {
                if !m.content.trim().is_empty() {
                    model.push_item(Item::Assistant(m.content.clone()));
                }
                for tc in &m.tool_calls {
                    call_tools.insert(tc.id.clone(), tc.tool.clone());
                    model.push_item(Item::ToolCall {
                        tool: tc.tool.clone(),
                        args: tc.args.clone(),
                    });
                }
            }
            kernel::Role::Tool => {
                let payload: serde_json::Value = serde_json::from_str(&m.content)
                    .unwrap_or_else(|_| serde_json::Value::String(m.content.clone()));
                // Status isn't projected into the message; a top-level "error"
                // key is how every failing observation payload reports itself.
                let ok = payload.get("error").is_none();
                let tool = m
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| call_tools.get(id))
                    .cloned()
                    .unwrap_or_else(|| "tool".to_string());
                model.push_item(Item::ToolResult { tool, ok, payload });
            }
            // system messages are not shown as transcript rows.
            _ => {}
        }
    }
    model.welcome = false;
    model.invalidate_all_renders();
    model.scroll_to_bottom();
}

/// Open the resume picker — but refuse while a turn is mid-flight: resuming then
/// would let the finishing turn's `Done` event overwrite the freshly-loaded
/// transcript. The user must finish or Esc the current turn first.
/// What a slash command routes to. Pure — no kernel, no side effects — so the
/// routing itself is unit-testable (a `/skill` regression once slipped through
/// because only the *handler* was tested, not that the command reached it).
#[derive(Debug, PartialEq, Eq)]
enum SlashAction {
    Resume,
    Rewind,
    Clear,
    Lsp,
    /// `/mcp` — open the MCP management picker.
    Mcp,
    Agents,
    /// `/steer <agent> <text>` — send further instruction to a running agent.
    Steer(String),
    /// `/mcp start <id>` — approve and connect a configured MCP server.
    McpStart(String),
    /// `/mcp add <id> [--trust trusted] [--env K=V] -- <command>` — add + connect.
    McpAdd(String),
    Memory(String),
    SkillPicker,
    LoadSkill(String),
    SkillInfo(String),
    RemoveSkill(String),
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
fn classify_slash(cmd: &str) -> SlashAction {
    match cmd {
        "resume" => SlashAction::Resume,
        "rewind" => SlashAction::Rewind,
        "clear" => SlashAction::Clear,
        "lsp" => SlashAction::Lsp,
        "mcp" => SlashAction::Mcp,
        "agents" => SlashAction::Agents,
        c if c.strip_prefix("steer").is_some_and(is_cmd_boundary) => {
            SlashAction::Steer(c.strip_prefix("steer").unwrap_or("").trim().to_string())
        }
        c if c.strip_prefix("mcp start").is_some_and(is_cmd_boundary) => {
            SlashAction::McpStart(c.strip_prefix("mcp start").unwrap_or("").trim().to_string())
        }
        c if c.strip_prefix("mcp add").is_some_and(is_cmd_boundary) => {
            SlashAction::McpAdd(c.strip_prefix("mcp add").unwrap_or("").trim().to_string())
        }
        c if c.strip_prefix("memory").is_some_and(is_cmd_boundary) => {
            SlashAction::Memory(c.strip_prefix("memory").unwrap_or("").trim().to_string())
        }
        "skill" => SlashAction::SkillPicker,
        "model" => SlashAction::ModelPicker,
        "model add" => SlashAction::AddModel,
        "search" => SlashAction::SearchConfig,
        "mode" => SlashAction::ModePicker,
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

/// Route a slash command to its handler. The ONE dispatch point — both Enter
/// paths (autocomplete-accept and plain typed) call this, so they can never
/// diverge (a bug we hit when the two were duplicated).
fn dispatch_slash<P, L>(
    model: &mut Model,
    cmd: &str,
    kernel: &Arc<Kernel<P, L>>,
    session: &mut Session,
    transcript: &mut Vec<Message>,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) where
    P: ProfileProvider + 'static,
    L: EventLog + 'static,
{
    // Clear the welcome splash: the view draws it INSTEAD of the transcript, so
    // a command run as the very first action would otherwise push a notice that
    // stays hidden behind the splash — looks like nothing happened.
    model.welcome = false;
    match classify_slash(cmd) {
        SlashAction::Resume => start_resume(model, kernel, tx),
        SlashAction::Rewind => start_rewind(model, kernel, session, tx),
        SlashAction::Clear => do_clear(model, session, transcript),
        SlashAction::Lsp => show_lsp_status(kernel, tx),
        SlashAction::Mcp => open_mcp_picker(model),
        SlashAction::Agents => open_agents_picker(model, tx),
        SlashAction::Steer(rest) => agents_steer(model, &rest),
        SlashAction::McpStart(id) => start_mcp_server(kernel, &id, tx),
        SlashAction::McpAdd(args) => mcp_add(model, &args, tx),
        SlashAction::Memory(name) => open_memory(model, &name, kernel, tx),
        SlashAction::SkillPicker => open_skill_picker(model),
        SlashAction::LoadSkill(name) => load_skill_by_name(model, &name, transcript),
        SlashAction::SkillInfo(name) => show_skill_info(model, &name),
        SlashAction::RemoveSkill(name) => begin_remove_skill(model, &name),
        SlashAction::ModelPicker => open_model_picker(model),
        SlashAction::AddModel => begin_model_setup(model),
        SlashAction::SearchConfig => begin_search_setup(model),
        SlashAction::ModePicker => open_mode_picker(model),
        SlashAction::SwitchMode(level) => {
            set_autonomy(model, kernel::AutonomyLevel::from_id(&level))
        }
        SlashAction::SwitchModel(name) => {
            switch_saved_model(model, kernel.provider.as_ref(), &name)
        }
        SlashAction::InstallSkill(src) => install_skill(model, &src, tx),
        SlashAction::SkillSources(args) => skill_sources(model, &args),
        SlashAction::SearchSkills(query) => search_skills(model, &query, tx),
        SlashAction::AddSkill(input) => add_skill(model, &input, tx),
        SlashAction::UpdateSkills(arg) => update_skills(model, &arg, tx),
        SlashAction::LockSkills => lock_skills(model),
        SlashAction::SyncSkills => sync_skills(model, tx),
        SlashAction::Other => run_slash(model, cmd, transcript, kernel.provider.as_ref()),
    }
}

fn show_lsp_status(
    kernel: &Arc<Kernel<impl Provider + 'static, impl EventLog + 'static>>,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    let executor = kernel.executor.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let observation = executor
            .execute(&kernel::ToolIntent {
                id: "tui-lsp-status".into(),
                tool: "lsp.status".into(),
                args: serde_json::json!({}),
            })
            .await;
        let result = if observation.status == kernel::ObsStatus::Ok {
            Ok(observation.payload)
        } else {
            Err(observation
                .payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("LSP is disabled")
                .to_string())
        };
        let _ = tx.send(TuiEvent::LspStatus(result));
    });
}

/// Approve + connect a configured MCP server, then refresh status.
fn start_mcp_server(
    kernel: &Arc<Kernel<impl Provider + 'static, impl EventLog + 'static>>,
    id: &str,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    let executor = kernel.executor.clone();
    let id = id.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let started = executor
            .execute(&kernel::ToolIntent {
                id: "tui-mcp-start".into(),
                tool: "mcp.start".into(),
                args: serde_json::json!({ "server": id }),
            })
            .await;
        let result = if started.status == kernel::ObsStatus::Ok {
            mcp_status_via(&executor).await
        } else {
            Err(started
                .payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("failed to start MCP server")
                .to_string())
        };
        let _ = tx.send(TuiEvent::McpStatus(result));
    });
}

/// Open the `/mcp` management picker: row 0 adds a server, the rest are the
/// user-config servers (`~/.medha/config.toml`).
fn open_mcp_picker(model: &mut Model) {
    let servers = config::load()
        .ok()
        .flatten()
        .map(|c| c.mcp)
        .unwrap_or_default();
    let rows: Vec<McpRow> = servers
        .into_iter()
        .map(|(id, server)| McpRow {
            id,
            command: server.target(),
            disabled: server.disabled,
        })
        .collect();
    model.picker = Some(Picker::new(PickerKind::Mcp(rows)));
}

/// `/mcp add <id> --url <https://…> [--oauth | --bearer T]` for a hosted server,
/// or `/mcp add <id> [--key K] [--trust trusted] [--env K=V] [--allow-tool P]
/// [--deny-tool P] [--no-network] [--parallel] -- <command>` for a local one.
/// Saves to `~/.medha/config.toml` (secrets → credential store), then connects live.
fn mcp_add(model: &mut Model, args: &str, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let mut id = None;
    let mut trust = "workspace".to_string();
    let mut key: Option<String> = None;
    let mut url = String::new();
    let mut auth = String::new();
    let mut env = std::collections::BTreeMap::new();
    let mut allow_tools: Vec<String> = Vec::new();
    let mut deny_tools: Vec<String> = Vec::new();
    let mut network = None;
    let mut parallel_calls = false;
    let mut command: Vec<String> = Vec::new();
    let mut it = args.split_whitespace();
    while let Some(token) = it.next() {
        match token {
            "--key" => key = it.next().map(str::to_string),
            "--url" => url = it.next().unwrap_or_default().to_string(),
            "--bearer" => {
                auth = "bearer".into();
                key = it.next().map(str::to_string);
            }
            "--oauth" => auth = "oauth".into(),
            "--trust" => trust = it.next().unwrap_or("workspace").to_string(),
            "--env" => {
                if let Some((k, v)) = it.next().and_then(|kv| kv.split_once('=')) {
                    env.insert(k.to_string(), v.to_string());
                }
            }
            "--allow-tool" => allow_tools.extend(it.next().map(str::to_string)),
            "--deny-tool" => deny_tools.extend(it.next().map(str::to_string)),
            "--no-network" => network = Some(false),
            "--parallel" => parallel_calls = true,
            "--" => command.extend(it.by_ref().map(str::to_string)),
            // A pasted URL is the server, wherever it appears — the id is
            // derived from it when not given.
            _ if mcp::is_url(token) => url = token.to_string(),
            _ if id.is_none() => id = Some(token.to_string()),
            _ => command.push(token.to_string()),
        }
    }
    let (Some(id), false) = (
        id.or_else(|| mcp::id_from_url(&url)),
        url.is_empty() && command.is_empty(),
    ) else {
        model.push_notice(
            "usage: /mcp add https://…   ·   /mcp add <id> https://…   ·   /mcp add <id> -- <command>",
        );
        return;
    };
    if !url.is_empty()
        && let Err(error) = mcp::validate_remote_url(&url)
    {
        model.push_notice(format!("mcp add: {error}"));
        return;
    }
    if let Some(k) = &key
        && let Err(error) = config::store_mcp_key(&id, k)
    {
        model.push_notice(format!("mcp add: could not store key: {error}"));
        return;
    }
    let server = config::McpServer {
        command,
        disabled: false,
        url: url.clone(),
        auth,
        env,
        trust,
        allow_tools,
        deny_tools,
        network,
        parallel_calls,
    };
    let mut cfg = config::load().ok().flatten().unwrap_or_default();
    cfg.mcp.insert(id.clone(), server.clone());
    if let Err(error) = config::save(&cfg) {
        model.push_notice(format!("mcp add: could not save config.toml: {error}"));
        return;
    }
    if let Some(manager) = model.mcp.clone() {
        model.push_notice(format!("(adding MCP server '{id}' …)"));
        let resolved = config::resolve_mcp_server(&id, &server);
        let tx = tx.clone();
        tokio::spawn(async move {
            // Report the status list either way: a failed server shows up as a
            // row with its state and reason, which beats a bare error string.
            // The server decides what happens next: an OAuth challenge goes
            // straight to the browser, an ambiguous one asks how to authenticate.
            match manager.add_server(resolved).await {
                Err(mcp::Error::NeedsAuth(_)) => return authorize_mcp_server(manager, id, tx),
                Err(mcp::Error::NeedsToken(_)) => {
                    let _ = tx.send(TuiEvent::McpNeedsAuth { server: id, url });
                    return;
                }
                _ => {}
            }
            let servers = serde_json::json!({ "servers": manager.status().await });
            let _ = tx.send(TuiEvent::McpStatus(Ok(servers)));
        });
    } else {
        model.push_notice(format!(
            "added '{id}' to config.toml (MCP host unavailable)"
        ));
    }
    open_mcp_picker(model);
}

/// `/agents` — what Medha has delegated and is still waiting on. A child's work
/// never enters the transcript, so without this the only sign one exists is a
/// name in the status bar.
/// Open the panel on what this process knows, then refresh from the durable
/// record.
///
/// Two steps because the panel must appear on the keystroke: outstanding
/// patches live in the event log so they outlive the process that made them,
/// and reading the log is not something a key handler may block on. The
/// cached view is correct for everything produced in this session, which is the
/// common case; the refresh adds what survived a restart.
fn open_agents_picker(model: &mut Model, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let Some(control) = model.agents.clone() else {
        model.picker = Some(Picker::new(PickerKind::Agents(Vec::new())));
        return;
    };
    // Opens on what this process already knows, then fills in from the log.
    // Idle times need a read per child, which is not something a keystroke may
    // block on — so the first paint carries none and the refresh supplies them.
    model.picker = Some(Picker::new(PickerKind::Agents(agent_rows(
        &control,
        control.cached_unmerged_patches(),
        &std::collections::HashMap::new(),
    ))));
    let tx = tx.clone();
    tokio::spawn(async move {
        let outstanding = control.outstanding().await;
        let idle = control.idle_times().await;
        let _ = tx.send(TuiEvent::AgentRows(agent_rows(
            &control,
            outstanding,
            &idle,
        )));
    });
}

/// Assemble the panel: running first, then patches, then history. Reading down
/// the list goes from "act now" to "waiting on you" to "for reference".
fn agent_rows(
    control: &orchestrator::AgentControl,
    outstanding: Vec<(String, String, orchestrator::Patch)>,
    idle: &std::collections::HashMap<String, Option<u64>>,
) -> Vec<AgentRow> {
    let waiting: Vec<String> = outstanding
        .iter()
        .map(|(_, session, _)| session.clone())
        .collect();
    let (running, settled): (Vec<_>, Vec<_>) = control
        .agents()
        .into_iter()
        // A settled writer whose patch is still waiting appears as a patch row;
        // listing it twice would read as two separate pieces of work.
        .filter(|agent| agent.is_running() || !waiting.contains(&agent.session))
        .partition(orchestrator::Agent::is_running);

    let row = |agent: orchestrator::Agent| AgentRow::Agent {
        // Absent from the map reads the same as nothing recorded yet — both
        // mean no answer, and neither should be shown as a stall.
        idle_ms: idle.get(&agent.session).copied().flatten(),
        agent,
    };
    let mut rows: Vec<AgentRow> = running.into_iter().map(row).collect();
    for (agent, session, patch) in outstanding {
        rows.push(AgentRow::Patch {
            agent,
            session,
            files: patch.files.len(),
            verified: patch.verification.as_ref().map(|evidence| evidence.passed),
        });
    }
    rows.extend(settled.into_iter().rev().map(row));
    rows
}

/// Show a writer's patch in the transcript.
///
/// Applying without reading is the failure this panel exists to prevent, so
/// viewing is one keystroke and never requires asking the model to fetch it.
fn agents_view_patch(model: &mut Model, session: &str, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let Some(control) = model.agents.clone() else {
        return;
    };
    model.picker = None;
    let (session, tx) = (session.to_string(), tx.clone());
    tokio::spawn(async move {
        let Some(patch) = control.patch(&session).await else {
            let _ = tx.send(TuiEvent::AgentPatchAction(Err(
                "that patch is no longer available".into(),
            )));
            return;
        };
        let evidence = match &patch.verification {
            Some(v) if v.passed => format!("`{}` passed", v.command),
            Some(v) => format!("`{}` FAILED\n{}", v.command, v.output),
            None => "nothing was run against this patch".to_string(),
        };
        // Bounded: a large refactor's diff would otherwise push the
        // conversation out of the scrollback it is meant to sit beside.
        const MAX_LINES: usize = 400;
        let total = patch.diff.lines().count();
        let body: String = patch
            .diff
            .lines()
            .take(MAX_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        let _ = tx.send(TuiEvent::AgentPatchAction(Ok(format!(
            "patch from '{session}' — {evidence}\n\n{body}{}",
            match total.saturating_sub(MAX_LINES) {
                0 => String::new(),
                n => format!("\n… {n} more line(s)"),
            }
        ))));
    });
}

/// `/steer <agent> <text>` — correct a running agent without killing it.
///
/// With one agent running the id may be omitted, because that is the case where
/// having to look one up is pure friction. With several it is required: sending
/// a correction to the wrong agent is worse than being asked which.
fn agents_steer(model: &mut Model, rest: &str) {
    let Some(control) = model.agents.clone() else {
        model.push_notice("sub-agents are not enabled in this session");
        return;
    };
    let running = control.active();
    if running.is_empty() {
        model.push_notice("no agent is running — /agents shows what has finished");
        return;
    }
    let (id, text) = match rest.split_once(char::is_whitespace) {
        Some((first, tail))
            if running
                .iter()
                .any(|run| run.path.name() == first || run.session == first) =>
        {
            (first.to_string(), tail.trim().to_string())
        }
        // No recognised id in front: the whole thing is the message, which is
        // only unambiguous with exactly one agent running.
        _ if running.len() == 1 => (running[0].session.clone(), rest.trim().to_string()),
        _ => {
            let names: Vec<&str> = running.iter().map(|run| run.path.name()).collect();
            model.push_notice(format!(
                "several agents are running — say which: /steer <agent> <message>  ({})",
                names.join(", ")
            ));
            return;
        }
    };
    if text.is_empty() {
        model.push_notice("nothing to send — /steer <agent> <message>");
        return;
    }
    // The user is the root of the tree, so every agent is within reach.
    match control.steer(&orchestrator::AgentPath::root(), &id, &text) {
        Ok(path) => model.push_notice(format!(
            "sent to '{}' — it arrives at the agent's next step",
            path.name()
        )),
        // Between listing and sending it can finish; saying so beats implying
        // the message landed.
        Err(error) => model.push_notice(format!("'{id}': {error} — nothing was sent")),
    }
}

/// Show what a child has actually been doing.
///
/// A child's chain is appended as it works, so this answers for a *running*
/// agent as well as a finished one. Without it the panel offers stop-or-nothing
/// on a running child, and the only way to see inside is to ask the model to
/// fetch it — which spends a turn to read something already on disk.
fn agents_view_transcript(model: &mut Model, session: &str, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let Some(control) = model.agents.clone() else {
        return;
    };
    model.picker = None;
    let (session, tx) = (session.to_string(), tx.clone());
    tokio::spawn(async move {
        let root = orchestrator::AgentPath::root();
        let outcome = match control.transcript(&root, &session).await {
            Ok(lines) if lines.is_empty() => Err(format!(
                "'{session}' has not recorded anything yet — it may still be on its first step"
            )),
            Ok(lines) => {
                // Bounded, newest last: a long-running child accumulates more
                // than belongs in the scrollback, and the recent end is the
                // part that says what it is doing *now*.
                const MAX_LINES: usize = 120;
                let dropped = lines.len().saturating_sub(MAX_LINES);
                let body = lines
                    .into_iter()
                    .skip(dropped)
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(format!(
                    "transcript of '{session}'{}\n\n{body}",
                    match dropped {
                        0 => String::new(),
                        n => format!(" — earliest {n} step(s) omitted"),
                    }
                ))
            }
            Err(error) => Err(error.to_string()),
        };
        let _ = tx.send(TuiEvent::AgentPatchAction(outcome));
    });
}

/// Apply a writer's patch by hand. The user pressing `a` *is* the human gate —
/// this is not the model deciding to change the user's files.
///
/// `force` comes from `A` (shift), and only bypasses the verification refusal.
/// A conflict is never forceable from here: §6.4 sends conflicting patches to
/// reconciliation, and no keystroke should be able to override that.
fn agents_apply_patch(
    model: &mut Model,
    session: &str,
    force: bool,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    let Some(control) = model.agents.clone() else {
        return;
    };
    let (session, tx) = (session.to_string(), tx.clone());
    // Off the UI thread: applying runs git, and a keystroke handler that waits
    // on a subprocess freezes the interface at best.
    tokio::spawn(async move {
        let Some(patch) = control.patch(&session).await else {
            let _ = tx.send(TuiEvent::AgentPatchAction(Err(
                "that patch is no longer available".into(),
            )));
            return;
        };
        let files = patch.files.join(", ");
        let outcome = match control.merge(&patch, force).await {
            Ok(_) => {
                control.forget(&session).await;
                let caveat = match (patch.verification.is_some(), force) {
                    (_, true) => " — applied over a failing build",
                    (false, _) => " — nothing was run against this patch",
                    (true, _) => "",
                };
                Ok(format!("applied {files}{caveat}"))
            }
            // The refusal is the feature, so it says what to do next rather
            // than only what went wrong.
            Err(orchestrator::Error::Unverified(command)) => Err(format!(
                "not applied — `{command}` fails against this patch. Enter to read the \
                 failure, or shift+A to apply anyway."
            )),
            // §6.4: never last-writer-wins. Nothing was applied, and the
            // message says so plainly rather than leaving the user to check.
            Err(error) => Err(format!("nothing applied — {error}")),
        };
        let _ = tx.send(TuiEvent::AgentPatchAction(outcome));
        // Refresh the panel from the record, so an applied patch leaves it.
        let idle = control.idle_times().await;
        let _ = tx.send(TuiEvent::AgentRows(agent_rows(
            &control,
            control.outstanding().await,
            &idle,
        )));
    });
}

/// Stop one agent by hand. `agent.cancel` gives the model this; the user needs
/// it too, and for the same reason — work that is no longer worth paying for.
fn agents_stop(model: &mut Model, session: &str, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let Some(control) = model.agents.clone() else {
        return;
    };
    match control.cancel(&orchestrator::AgentPath::root(), session) {
        // Cancelling is not discarding: the child still reports what it found.
        Ok(path) => model.push_notice(format!(
            "stopped agent '{}' — whatever it had found still arrives with its report",
            path.name()
        )),
        Err(_) => model.push_notice("that agent already finished"),
    }
    open_agents_picker(model, tx);
}

/// Which language servers this machine can actually run, and what to do about
/// the ones it cannot. Servers start lazily, so the running list alone never
/// explains why an answer came back as a text match.
fn lsp_inventory_line(payload: &serde_json::Value) -> String {
    let Some(available) = payload
        .get("available")
        .and_then(serde_json::Value::as_array)
    else {
        return String::new();
    };
    let installed: Vec<&str> = available
        .iter()
        .filter(|entry| entry["installed"].as_bool() == Some(true))
        .filter_map(|entry| entry["server"].as_str())
        .collect();
    let fetchable: Vec<&str> = available
        .iter()
        .filter(|entry| {
            entry["installed"].as_bool() != Some(true)
                && entry["installable"].as_bool() == Some(true)
        })
        .filter_map(|entry| entry["server"].as_str())
        .collect();
    let mut text = format!(
        "  {}/{} servers available here",
        installed.len(),
        available.len()
    );
    if !installed.is_empty() {
        text.push_str(&format!(": {}", installed.join(", ")));
    }
    if !fetchable.is_empty() {
        text.push_str(&format!(
            "\n  · medha lsp install <id>   ({})",
            fetchable.join(", ")
        ));
    }
    text.push_str("\n  · a file with no server falls back to text search");
    text
}

/// Connect a configured server. The server itself decides what happens next: an
/// OAuth challenge goes straight to the browser, an ambiguous one asks the user.
fn connect_mcp_server(
    manager: Arc<mcp::McpManager>,
    id: String,
    tx: mpsc::UnboundedSender<TuiEvent>,
) {
    tokio::spawn(async move {
        // A server edited straight into config.toml is not in this session's
        // host yet; register it from the config rather than reporting it missing.
        let mut outcome = manager.approve_and_connect(&id, None).await;
        if matches!(outcome, Err(mcp::Error::UnknownServer(_)))
            && let Some(server) = config::load()
                .ok()
                .flatten()
                .and_then(|c| c.mcp.get(&id).cloned())
        {
            outcome = manager
                .add_server(config::resolve_mcp_server(&id, &server))
                .await;
        }
        match outcome {
            Err(mcp::Error::NeedsAuth(_)) => authorize_mcp_server(manager, id, tx),
            Err(mcp::Error::NeedsToken(_)) => {
                let url = manager
                    .start_preview(&id)
                    .await
                    .map(|preview| preview.target)
                    .unwrap_or_default();
                let _ = tx.send(TuiEvent::McpNeedsAuth { server: id, url });
            }
            outcome => {
                if let Err(error) = outcome {
                    let _ = tx.send(TuiEvent::McpStatus(Err(format!("'{id}': {error}"))));
                    return;
                }
                let servers = serde_json::json!({ "servers": manager.status().await });
                let _ = tx.send(TuiEvent::McpStatus(Ok(servers)));
            }
        }
    });
}

/// Apply the credential choice made in the [`PickerKind::McpAuth`] picker.
fn mcp_choose_auth(
    model: &mut Model,
    id: &str,
    url: &str,
    choice: usize,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    // 1 = paste a token: the value is a secret, so route it back through
    // `/mcp add`, which stores it in the keychain rather than config.toml.
    if choice == 1 {
        prefill_command(
            model,
            &format!("/mcp add {id} {url} --bearer "),
            "paste the API token after --bearer (it goes to the OS keychain, never a file)",
        );
        return;
    }
    let auth = if choice == 0 { "oauth" } else { "none" };
    let mut cfg = config::load().ok().flatten().unwrap_or_default();
    let Some(server) = cfg.mcp.get_mut(id) else {
        model.push_notice(format!("no MCP server '{id}'"));
        return;
    };
    server.auth = auth.to_string();
    let server = server.clone();
    if let Err(error) = config::save(&cfg) {
        model.push_notice(format!("mcp: could not save config.toml: {error}"));
        return;
    }
    let Some(manager) = model.mcp.clone() else {
        return;
    };
    model.push_notice(format!("(connecting MCP server '{id}' …)"));
    let resolved = config::resolve_mcp_server(id, &server);
    let (id, tx) = (id.to_string(), tx.clone());
    tokio::spawn(async move {
        let _ = manager.add_server(resolved).await;
        if manager.needs_sign_in(&id).await {
            authorize_mcp_server(manager, id, tx);
            return;
        }
        let servers = serde_json::json!({ "servers": manager.status().await });
        let _ = tx.send(TuiEvent::McpStatus(Ok(servers)));
    });
}

/// Run the browser sign-in, surfacing the authorization URL as a notice so a
/// machine that cannot open a browser can still finish by hand.
fn authorize_mcp_server(
    manager: Arc<mcp::McpManager>,
    id: String,
    tx: mpsc::UnboundedSender<TuiEvent>,
) {
    tokio::spawn(async move {
        let (urls, mut rx) = mpsc::unbounded_channel();
        let relay = {
            let tx = tx.clone();
            let id = id.clone();
            tokio::spawn(async move {
                while let Some(url) = rx.recv().await {
                    let _ = tx.send(TuiEvent::McpAuthUrl {
                        server: id.clone(),
                        url,
                    });
                }
            })
        };
        let outcome = manager.authorize(&id, &urls).await;
        drop(urls);
        let _ = relay.await;
        let _ = tx.send(TuiEvent::McpStatus(match outcome {
            Ok(_) => Ok(serde_json::json!({ "servers": manager.status().await })),
            Err(error) => Err(format!("'{id}': {error}")),
        }));
    });
}

/// Switch a server on or off. Disabling drops it from the live host so its
/// tools leave the model's context at once, but keeps its definition and
/// credentials so enabling costs nothing but a reconnect.
fn mcp_set_disabled(
    model: &mut Model,
    id: &str,
    disabled: bool,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    let mut cfg = config::load().ok().flatten().unwrap_or_default();
    let Some(server) = cfg.mcp.get_mut(id) else {
        return model.push_notice(format!("no MCP server '{id}'"));
    };
    server.disabled = disabled;
    let server = server.clone();
    if let Err(error) = config::save(&cfg) {
        return model.push_notice(format!("mcp: could not save config.toml: {error}"));
    }
    if let Some(manager) = model.mcp.clone() {
        let (id, tx) = (id.to_string(), tx.clone());
        let resolved = config::resolve_mcp_server(&id, &server);
        tokio::spawn(async move {
            // The server may predate this session's host (added straight to
            // config.toml); register it before switching it on.
            if manager.set_disabled(&id, disabled).await.is_err() && !disabled {
                let _ = manager.add_server(resolved).await;
            }
            let _ = tx.send(TuiEvent::McpStatus(Ok(
                serde_json::json!({ "servers": manager.status().await }),
            )));
        });
    }
    model.push_notice(format!(
        "MCP server '{id}' {}",
        if disabled { "off" } else { "on" }
    ));
    open_mcp_picker(model);
}

/// Open one server's catalogue so individual tools can be switched off. A large
/// server injects every schema into every request, so this is real context cost.
/// Connects on demand — browsing tools should not require connecting first.
fn open_mcp_tools(model: &mut Model, id: &str, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let Some(manager) = model.mcp.clone() else {
        return model.push_notice("MCP host unavailable");
    };
    let tools = manager.server_tools(id);
    if !tools.is_empty() {
        model.picker = Some(Picker::new(PickerKind::McpTools {
            id: id.to_string(),
            tools,
        }));
        return;
    }
    // A switched-off server has no catalogue and connecting would un-park it,
    // which is not what `t` should do behind the user's back.
    let configured = config::load()
        .ok()
        .flatten()
        .and_then(|c| c.mcp.get(id).cloned());
    if configured.as_ref().is_some_and(|server| server.disabled) {
        return model.push_notice(format!(
            "'{id}' is switched off — press space to turn it on"
        ));
    }
    model.push_notice(format!("(listing tools for '{id}' …)"));
    let (id, tx) = (id.to_string(), tx.clone());
    tokio::spawn(async move {
        let mut outcome = manager.approve_and_connect(&id, None).await;
        if matches!(outcome, Err(mcp::Error::UnknownServer(_)))
            && let Some(server) = configured
        {
            outcome = manager
                .add_server(config::resolve_mcp_server(&id, &server))
                .await;
        }
        match outcome {
            Ok(_) => {
                let tools = manager.server_tools(&id);
                let _ = tx.send(TuiEvent::McpTools { server: id, tools });
            }
            Err(error) => {
                let _ = tx.send(TuiEvent::McpStatus(Err(format!("'{id}': {error}"))));
            }
        }
    });
}

/// Switch one tool's exposure, persisting it as a `deny_tools` entry.
fn mcp_set_tool(model: &mut Model, id: &str, tool: &str, expose: bool) {
    let mut cfg = config::load().ok().flatten().unwrap_or_default();
    let Some(server) = cfg.mcp.get_mut(id) else {
        return model.push_notice(format!("no MCP server '{id}'"));
    };
    if expose {
        server.deny_tools.retain(|denied| denied != tool);
    } else if !server.deny_tools.iter().any(|denied| denied == tool) {
        server.deny_tools.push(tool.to_string());
    }
    let server = server.clone();
    if let Err(error) = config::save(&cfg) {
        return model.push_notice(format!("mcp: could not save config.toml: {error}"));
    }
    // Reflect the choice in the open picker immediately, then reconnect in the
    // background so the filter takes effect this turn rather than next launch.
    if let Some(Picker {
        kind: PickerKind::McpTools { tools, .. },
        ..
    }) = &mut model.picker
        && let Some(entry) = tools.iter_mut().find(|(name, _)| name == tool)
    {
        entry.1 = expose;
    }
    if let Some(manager) = model.mcp.clone() {
        let resolved = config::resolve_mcp_server(id, &server);
        tokio::spawn(async move {
            let _ = manager.add_server(resolved).await;
        });
    }
}

/// Remove a server from the `/mcp` picker: drop it from `config.toml` and the
/// live host, then reopen the picker.
fn mcp_remove(model: &mut Model, id: &str, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let mut cfg = config::load().ok().flatten().unwrap_or_default();
    if cfg.mcp.remove(id).is_some() {
        let _ = config::save(&cfg);
        config::delete_mcp_key(id);
    }
    if let Some(manager) = model.mcp.clone() {
        let id = id.to_string();
        let tx = tx.clone();
        tokio::spawn(async move {
            let _ = manager.remove_server(&id).await;
            let _ = tx.send(TuiEvent::McpStatus(Ok(
                serde_json::json!({ "servers": manager.status().await }),
            )));
        });
    }
    model.push_notice(format!("removed MCP server '{id}'"));
    open_mcp_picker(model);
}

async fn mcp_status_via<E: kernel::Executor + ?Sized>(
    executor: &Arc<E>,
) -> Result<serde_json::Value, String> {
    let observation = executor
        .execute(&kernel::ToolIntent {
            id: "tui-mcp-status".into(),
            tool: "mcp.status".into(),
            args: serde_json::json!({}),
        })
        .await;
    if observation.status == kernel::ObsStatus::Ok {
        Ok(observation.payload)
    } else {
        Err(observation
            .payload
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("MCP is disabled")
            .to_string())
    }
}

fn open_memory<L: EventLog + 'static>(
    model: &mut Model,
    name: &str,
    kernel: &Arc<Kernel<impl Provider + 'static, L>>,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    let Some(store) = model.memory.as_ref().filter(|_| model.memory_enabled) else {
        model.push_notice("memory: unavailable");
        return;
    };
    if name.is_empty() {
        let entries = store.list().unwrap_or_default();
        if entries.is_empty() {
            model.push_notice("memory: no entries");
        } else {
            model.picker = Some(Picker::new(PickerKind::Memory(entries)));
        }
        return;
    }

    let entry = store
        .get(memory::Scope::Project, name)
        .ok()
        .flatten()
        .or_else(|| store.get(memory::Scope::User, name).ok().flatten());
    let Some(entry) = entry else {
        model.push_notice(format!("memory '{name}' not found"));
        return;
    };
    spawn_memory_provenance(entry, kernel, tx);
    model.push_notice(format!("(opening memory '{name}' provenance …)"));
}

/// Memory picker actions (p = pin/unpin, f = forget). Returns true if the key
/// was handled. Mutations go through the log first (an event) then the
/// projection, exactly like the CLI — so trust/provenance and fork-safety hold.
fn handle_memory_picker_key<P, L>(
    model: &mut Model,
    key: &KeyEvent,
    kernel: &Arc<Kernel<P, L>>,
    session: &Session,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) -> bool
where
    P: Provider + 'static,
    L: EventLog + 'static,
{
    let Some(picker) = model.picker.as_ref() else {
        return false;
    };
    let PickerKind::Memory(entries) = &picker.kind else {
        return false;
    };
    let Some(entry) = entries.get(picker.selected).cloned() else {
        return false;
    };
    let op = match key.code {
        KeyCode::Char('p') => memory::MemoryOp::Pin {
            scope: entry.scope,
            name: entry.name.clone(),
            pinned: !entry.pinned,
        },
        KeyCode::Char('f') => memory::MemoryOp::Forget {
            scope: entry.scope,
            name: entry.name.clone(),
        },
        _ => return false,
    };
    let verb = match &op {
        memory::MemoryOp::Pin { pinned: true, .. } => format!("pinned '{}'", entry.name),
        memory::MemoryOp::Pin { .. } => format!("unpinned '{}'", entry.name),
        memory::MemoryOp::Forget { .. } => format!("forgot '{}'", entry.name),
        _ => String::new(),
    };
    apply_tui_memory_op(model, kernel, session, op, tx);
    model.push_notice(format!("✔ {verb}"));
    true
}

/// Append a memory mutation as a durable event, apply it to the projection, and
/// refresh the open picker's entry list so the change shows immediately.
fn apply_tui_memory_op<P, L>(
    model: &mut Model,
    kernel: &Arc<Kernel<P, L>>,
    session: &Session,
    op: memory::MemoryOp,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) where
    P: Provider + 'static,
    L: EventLog + 'static,
{
    let Some(store) = model.memory.clone().filter(|_| model.memory_enabled) else {
        model.push_notice("memory: unavailable");
        return;
    };
    // Event is the source of truth; the projection is a rebuildable cache.
    let log = kernel.log.clone();
    let session = session.clone();
    let event_op = op.clone();
    let _ = tx;
    tokio::spawn(async move {
        if let Ok(payload) = serde_json::to_value(&event_op) {
            let _ = log
                .append(kernel::Event::memory_write(&session, payload))
                .await;
        }
    });
    let _ = store.apply(&op);
    // Refresh the picker in place (or close it if nothing is left to show).
    if let Some(picker) = model.picker.as_mut() {
        if let PickerKind::Memory(entries) = &mut picker.kind {
            *entries = store.list().unwrap_or_default();
            if entries.is_empty() {
                model.picker = None;
            } else if picker.selected >= entries.len() {
                picker.selected = entries.len() - 1;
            }
        }
    }
}

fn spawn_memory_provenance<L: EventLog + 'static>(
    entry: memory::MemoryEntry,
    kernel: &Arc<Kernel<impl Provider + 'static, L>>,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    let log = kernel.log.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let mut provenance = None;
        for session_id in &entry.sessions {
            let events = log.events(*session_id).await;
            if let Some(event) = events
                .into_iter()
                .find(|event| entry.provenance.contains(&event.id))
            {
                provenance = Some(event);
                break;
            }
        }
        let _ = tx.send(TuiEvent::MemoryProvenance(Box::new(entry), provenance));
    });
}

fn refresh_branch_memory(model: &Model, system: &mut Message, events: Vec<kernel::Event>) {
    let Some(store) = model.memory.as_ref().filter(|_| model.memory_enabled) else {
        return;
    };
    if store.rebuild_project(events.into_iter()).is_err() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    if let Ok(block) = memory::recall::compile_k3_configured(
        store,
        model.memory_budget_tokens,
        now,
        model.memory_stale_after_days,
    ) {
        system.content = memory::recall::replace_k3(&system.content, &block);
    }
}

fn start_resume<L: EventLog + 'static>(
    model: &mut Model,
    kernel: &Arc<Kernel<impl Provider + 'static, L>>,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    if model.running {
        model.push_notice("finish or Esc the current turn before resuming a session");
        return;
    }
    spawn_sessions_fetch(kernel, tx);
}

/// Open the rewind (time-travel) picker for the CURRENT session. Like resume, it
/// refuses mid-turn. Reads the session's events off the main loop and derives one
/// cut point per past user turn — branching before turn *k* keeps turns 1..k-1 and
/// rolls the workspace back to that moment. Sends `RewindPointsLoaded` back.
fn start_rewind<L: EventLog + 'static>(
    model: &mut Model,
    kernel: &Arc<Kernel<impl Provider + 'static, L>>,
    session: &Session,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    if model.running {
        model.push_notice("finish or Esc the current turn before rewinding");
        return;
    }
    let log = kernel.log.clone();
    let session_id = session.id;
    let tx = tx.clone();
    tokio::spawn(async move {
        let events = log.events(session_id).await;
        // Rewind points are your past prompts: rewinding *to* a message goes
        // back to just BEFORE it ran — the message + everything
        // after leave the conversation, code reverts to before that turn's edits,
        // and the prompt is prefilled to edit/re-send. So the cut is the
        // user-message event itself, and every prompt (incl. the latest — "redo
        // my last") is a valid point.
        let points: Vec<RewindPoint> = events
            .iter()
            .filter(|e| e.kind == kernel::EventKind::UserMessage)
            .map(|e| {
                let text = e
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(empty)")
                    .trim()
                    .replace('\n', " ");
                let label = if text.chars().count() > 60 {
                    format!("{}…", text.chars().take(59).collect::<String>())
                } else {
                    text
                };
                // Files a code rollback from this prompt onward would revert —
                // shown in the picker; hides the code options when zero.
                let files = kernel::rollback_plan(&events, e.id).len();
                RewindPoint {
                    at_event: e.id,
                    label,
                    files,
                }
            })
            .collect();
        let _ = tx.send(TuiEvent::RewindPointsLoaded(points));
    });
}

/// Perform a rewind back to just before `at_event` (a past user prompt).
/// Reads the session history once. When `scope` touches code, it rolls files
/// back to before that turn's edits. When `scope` touches the conversation, it
/// forks the session before the prompt (a new branch — the original is
/// preserved), projects the conversation up to the cut, and lifts the prompt
/// text out to prefill the input box. Code-only leaves the conversation as-is
/// (no fork, `new_id = None`). Sends `Rewound` back with whatever it changed.
fn spawn_rewind<L: EventLog + 'static>(
    kernel: &Arc<Kernel<impl Provider + 'static, L>>,
    restore: Arc<WorkspaceSandbox>,
    session_id: ulid::Ulid,
    at_event: ulid::Ulid,
    scope: RewindScope,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    let log = kernel.log.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let events = log.events(session_id).await;
        let Some(idx) = kernel::cut_index(&events, at_event) else {
            return;
        };

        // Code rollback: revert every file written from this prompt onward,
        // returning the workspace to its state before the turn ran.
        let mut rolled = 0usize;
        if scope.touches_code() {
            for fr in kernel::rollback_plan(&events, at_event) {
                if restore
                    .restore(&fr.path, fr.snapshot.as_deref())
                    .await
                    .is_ok()
                {
                    rolled += 1;
                }
            }
        }

        // Conversation rewind: fork before the prompt (non-destructive), replay
        // the kept history, and prefill the prompt for editing/re-sending.
        let (new_id, msgs, memory_events, prefill) = if scope.touches_conversation() {
            let new_id = match log.fork(session_id, at_event).await {
                Ok(id) => id,
                Err(_) => return,
            };
            let prefill = events
                .get(idx)
                .and_then(|e| e.payload.get("text"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let memory_events = log.events(new_id).await;
            (
                Some(new_id),
                kernel::project_messages(&events[..idx]),
                memory_events,
                prefill,
            )
        } else {
            (None, Vec::new(), Vec::new(), None)
        };
        let _ = tx.send(TuiEvent::Rewound {
            new_id,
            msgs,
            memory_events,
            rolled,
            scope,
            prefill,
        });
    });
}

/// Spawn the async session-list fetch for `/resume`. Reads `log.sessions()`
/// off the main loop and sends `SessionsLoaded` back through the channel.
fn spawn_sessions_fetch<L: EventLog + 'static>(
    kernel: &Arc<Kernel<impl Provider + 'static, L>>,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    let log = kernel.log.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let sessions = log.sessions().await;
        let _ = tx.send(TuiEvent::SessionsLoaded(sessions));
    });
}

/// Spawn agent turn as background task.
///
/// `line` is `None` for a turn nobody typed: a background agent finished, and
/// its report is picked up at the head of this turn like any other. Without
/// that path a finished agent's work sits in the outbox until the user happens
/// to send something, so the answer to a question they already asked waits on
/// an unrelated message.
pub(super) fn spawn_turn<P, L>(
    model: &mut Model,
    kernel: &Arc<Kernel<P, L>>,
    session: &Session,
    transcript: &mut Vec<Message>,
    budget: &Budget,
    tx: &mpsc::UnboundedSender<TuiEvent>,
    line: Option<String>,
) where
    P: Provider + 'static,
    L: EventLog + 'static,
{
    model.welcome = false;
    let unprompted = line.is_none();
    if let Some(line) = &line {
        model.push_item(Item::User(line.clone()));
    }
    model.auto_scroll = true;
    model.running = true;
    model.reasoning_received_this_turn = false;
    model.turn_started = Some(Instant::now());
    // Pick up any skill saved/edited since startup so the model's manifest is
    // current this turn (not just next session).
    model.refresh_skill_manifest(transcript);
    if let Some(line) = line {
        transcript.push(Message::user(line));
    }

    // Graceful interruption: the kernel owns cancellation now. Esc trips the
    // handle; run_session ALWAYS returns (settled history + StopReason), so
    // there is no select! race dropping the session future mid-tool anymore.
    let (handle, queue) = kernel::InterruptQueue::pair();
    // The control plane watches this so an `agent.wait` ends the moment you
    // type, rather than holding the turn against instructions you have already
    // superseded. A fresh handle per turn, so this replaces the last one.
    if let Some(control) = &model.agents {
        control.attend(handle.clone());
    }
    model.interrupt = Some(handle);
    model.cancelling = false;

    let kernel = kernel.clone();
    // Apply the live autonomy dial to this turn's session (the policy reads it).
    let mut session = session.clone();
    session.autonomy = model.autonomy;
    let mut messages = transcript.clone();
    let budget = budget.clone();
    let tx = tx.clone();
    let agents = model.agents.clone();
    // A turn nobody typed exists only to deliver reports. If none are left by
    // the time it runs, there is nothing to say and the model must not be
    // called: two agents finishing together can arm this twice while the first
    // turn collects both, leaving the second with an empty inbox.
    let needs_a_report = unprompted;
    let mut collected = 0usize;

    tokio::spawn(async move {
        // Reports from background agents, delivered at the head of the turn.
        // Collecting here rather than on completion is what makes delivery
        // survive a restart: the outbox holds them until their owner next runs.
        if let Some(control) = &agents {
            for result in control.collect(session.id).await {
                collected += 1;
                // Bound it: a background report reaches context directly, without
                // the tool layer's cap, so a long one would otherwise arrive
                // whole and crowd out the conversation it was meant to serve.
                let mut summary = result.summary;
                if let Some(cut) = summary
                    .char_indices()
                    .nth(orchestrator::MAX_SUMMARY_CHARS)
                    .map(|(index, _)| index)
                {
                    // Spill before trimming: the tail is the part worth keeping,
                    // and truncating first would discard it unrecoverably.
                    let spilled = kernel.artifacts.put(summary.as_bytes()).ok();
                    summary.truncate(cut);
                    summary.push_str(&match spilled {
                        Some(hash) => {
                            format!("\n… truncated; read the rest with `read_artifact` {hash}")
                        }
                        None => "\n… report truncated".to_string(),
                    });
                }
                messages.push(Message::new(
                    kernel::Role::User,
                    format!(
                        "[background agent '{}' finished — {}]\n{}",
                        result.agent,
                        serde_json::to_string(&result.status)
                            .unwrap_or_default()
                            .trim_matches('"'),
                        summary
                    ),
                ));
            }
        }
        if needs_a_report && collected == 0 {
            // Nothing to deliver and nothing was asked: end the turn without
            // spending a request. `Done` restores the input and clears
            // `running`, so the UI settles exactly as it would after any turn.
            let _ = tx.send(TuiEvent::Done(messages, kernel::StopReason::Finished));
            return;
        }
        let sink = TuiSink { tx: tx.clone() };
        match kernel
            .run_session(&session, messages, budget, &sink, Some(queue))
            .await
        {
            Ok((updated, reason)) => {
                let _ = tx.send(TuiEvent::Done(updated, reason));
            }
            Err(e) => {
                let _ = tx.send(TuiEvent::Error(e.to_string()));
            }
        }
    });
}

/// Monotonic sequence for tracing events across the agent→UI channel (PART 7).
static EVENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Sink for sending events from kernel to TUI
pub(super) struct TuiSink {
    tx: mpsc::UnboundedSender<TuiEvent>,
}

impl TuiSink {
    fn emit(&self, kind: &'static str, ev: TuiEvent) {
        let seq = EVENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::trace!(seq, kind, "sink→ui");
        let _ = self.tx.send(ev);
    }
}

impl kernel::StreamSink for TuiSink {
    fn tool_started(&self, tool: &str, target: Option<&str>) {
        self.emit(
            "tool_started",
            TuiEvent::ToolStarted(tool.to_string(), target.map(str::to_string)),
        );
    }
    fn text(&self, delta: &str) {
        self.emit("text", TuiEvent::Text(delta.to_string()));
    }
    fn reasoning(&self, delta: &str) {
        self.emit("reasoning", TuiEvent::Reasoning(delta.to_string()));
    }
    fn tool_call(&self, tool: &str, args: &serde_json::Value) {
        self.emit(
            "tool_call",
            TuiEvent::ToolCall(tool.to_string(), args.clone()),
        );
    }
    fn tool_result(&self, tool: &str, ok: bool, payload: &serde_json::Value) {
        self.emit(
            "tool_result",
            TuiEvent::ToolResult(tool.to_string(), ok, payload.clone()),
        );
    }
    fn compacting(&self, active: bool) {
        self.emit("compacting", TuiEvent::Compacting(active));
    }
    fn compaction(&self, before: u32, after: u32, summarized: bool, summary: Option<&str>) {
        self.emit(
            "compaction",
            TuiEvent::Compaction(before, after, summarized, summary.map(str::to_string)),
        );
    }
    fn usage(&self, prompt_tokens: u32, total_tokens: u32) {
        self.emit("usage", TuiEvent::Usage(prompt_tokens, total_tokens));
    }
    fn cost(&self, total_usd: f64, indicative: bool) {
        self.emit("cost", TuiEvent::Cost(total_usd, indicative));
    }
    fn verify(&self, ok: bool, summary: &str) {
        self.emit("verify", TuiEvent::Verify(ok, summary.to_string()));
    }
    fn steered(&self, text: &str) {
        self.emit("steered", TuiEvent::Steered(text.to_string()));
    }
    fn steers_returned(&self, texts: &[String]) {
        self.emit("steers_returned", TuiEvent::SteersReturned(texts.to_vec()));
    }
}

/// Run slash command
/// True when a stripped command prefix ends at a word boundary — i.e. the rest
/// is empty or starts with whitespace (an argument). Distinguishes `/think` and
/// `/think high` from `/thinking`.
fn is_cmd_boundary(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with(char::is_whitespace)
}

/// The unified reasoning panel has three editable rows. Capability and prior
/// delivery are report-only rows and are deliberately not selectable.
fn set_reasoning_checked<P: kernel::Provider>(
    model: &mut Model,
    provider: &P,
    config: kernel::ReasoningConfig,
) -> Result<(), String> {
    provider
        .set_reasoning(config.clone())
        .map_err(|error| error.to_string())?;
    model.reasoning = provider.reasoning();
    model.reasoning_support = provider.reasoning_support();
    if config.effort.is_some() && model.reasoning_support == kernel::ReasoningSupport::Unknown {
        model.push_notice(
            "reasoning effort requested; this profile marks model support as unverified",
        );
    }
    Ok(())
}

fn handle_reasoning_picker_key<P: kernel::Provider>(
    model: &mut Model,
    key: KeyEvent,
    provider: &P,
) -> bool {
    let selected = match model.picker.as_ref() {
        Some(Picker {
            kind: PickerKind::Reasoning(_),
            selected,
        }) => *selected,
        _ => return false,
    };

    match key.code {
        KeyCode::Up => {
            if let Some(picker) = model.picker.as_mut() {
                picker.selected = picker.selected.checked_sub(1).unwrap_or(2);
            }
        }
        KeyCode::Down => {
            if let Some(picker) = model.picker.as_mut() {
                picker.selected = (picker.selected + 1) % 3;
            }
        }
        KeyCode::Enter => {
            match selected {
                0 => {
                    let cfg = provider.reasoning();
                    let enabled = cfg.enabled != Some(true);
                    if let Err(error) = set_reasoning_checked(
                        model,
                        provider,
                        kernel::ReasoningConfig {
                            enabled: Some(enabled),
                            effort: if enabled { cfg.effort } else { None },
                        },
                    ) {
                        model.push_notice(format!("reasoning unchanged: {error}"));
                    }
                }
                1 => {
                    model.show_thinking = !model.show_thinking;
                    model.invalidate_all_renders();
                }
                2 => {
                    let cfg = provider.reasoning();
                    let effort = match cfg.effort {
                        None => Some(kernel::ReasoningEffort::Minimal),
                        Some(kernel::ReasoningEffort::Minimal) => {
                            Some(kernel::ReasoningEffort::Low)
                        }
                        Some(kernel::ReasoningEffort::Low) => Some(kernel::ReasoningEffort::Medium),
                        Some(kernel::ReasoningEffort::Medium) => {
                            Some(kernel::ReasoningEffort::High)
                        }
                        Some(kernel::ReasoningEffort::High) => None,
                    };
                    if let Err(error) = set_reasoning_checked(
                        model,
                        provider,
                        kernel::ReasoningConfig {
                            enabled: effort.map(|_| true),
                            effort,
                        },
                    ) {
                        model.push_notice(format!("reasoning unchanged: {error}"));
                    }
                }
                _ => {}
            }
            let state = ReasoningPanelState::from_model(model);
            if let Some(picker) = model.picker.as_mut() {
                picker.kind = PickerKind::Reasoning(state);
            }
            model.dirty = true;
        }
        KeyCode::Esc => model.picker = None,
        _ => {}
    }
    true
}

/// `/clear`: reset the conversation for real. Clearing only the rendered items
/// (as `run_slash` used to) left the full prior history in `transcript`, so the
/// very next turn re-shipped everything to the model. Truncate the transcript to
/// the system prompt AND start a fresh session id so new events don't append to
/// the old thread. Refused mid-turn (the running turn would rewrite transcript
/// on `Done`).
fn do_clear(model: &mut Model, session: &mut Session, transcript: &mut Vec<Message>) {
    if model.running {
        model.push_notice("finish or Esc the current turn before clearing");
        return;
    }
    let system = transcript
        .first()
        .cloned()
        .unwrap_or_else(|| Message::system(""));
    transcript.clear();
    transcript.push(system);
    *session = Session::new();
    model.items.clear();
    model.reasoning_received_this_turn = false;
    model.last_turn_reasoning_received = None;
    model.invalidate_all_renders();
    model.push_notice("(conversation cleared — fresh session)");
}

/// `/skill` with no name: open a selectable picker of installed skills (↑↓ +
/// Enter), so the user never has to remember or type an exact name.
fn open_skill_picker(model: &mut Model) {
    let Some(store) = model.skills.clone() else {
        model.push_notice("skills unavailable in this session");
        return;
    };
    let disc = store.discover(&model.known_tools);
    let unavailable = disc
        .effective()
        .filter(|listing| !listing.available())
        .count();
    let list: Vec<(String, String)> = disc
        .effective()
        .filter(|listing| listing.available())
        .map(|listing| {
            // Trust receipt from the guard verdict recorded at install (only
            // installed user skills have provenance; others show no mark).
            let receipt = match store
                .provenance(&listing.skill.name)
                .and_then(|p| p.scan_verdict)
                .as_deref()
            {
                Some("safe") => "✓ ",
                Some("caution") => "⚠ ",
                _ => "",
            };
            (
                listing.skill.name.clone(),
                format!(
                    "{receipt}[{}] {}",
                    listing.skill.scope.as_str(),
                    listing.skill.description
                ),
            )
        })
        .collect();
    if list.is_empty() {
        if unavailable > 0 {
            model.push_notice(format!(
                "{unavailable} installed skill(s) need tools that are unavailable in this session; use /skills for details"
            ));
        } else {
            model.push_notice(
                "no skills installed yet — pick Search or Install at the top, or add ~/.medha/skills/<name>/SKILL.md",
            );
        }
    } else if unavailable > 0 {
        model.push_notice(format!(
            "{unavailable} unavailable skill(s) are hidden here; use /skills for missing-tool details"
        ));
    }
    // The hub always carries its action rows, so an empty catalog still opens to
    // useful actions (search / install) instead of a dead end.
    model.picker = Some(Picker::new(PickerKind::Skill(list)));
}

/// `/model`: list the persisted profiles. A profile name represents the full
/// connection (endpoint, model id, key reference, and context window), not
/// merely a provider label.
fn open_model_picker(model: &mut Model) {
    if model.running {
        model.push_notice("finish or Esc the current turn before switching models");
        return;
    }
    let Ok(cfg) = model.model_config.lock() else {
        model.push_notice("model configuration is temporarily unavailable");
        return;
    };
    let profiles = cfg.model_profiles();
    drop(cfg);
    if profiles.is_empty() {
        // Nothing saved yet (first run, or an env-only session): the useful
        // next step IS the add form, so open it instead of a dead-end notice.
        begin_model_setup(model);
        return;
    }
    // Cursor starts on the model in use, marked ✓ — Enter without moving
    // keeps the status quo. A one-session override matches no saved row and
    // falls back to the top.
    let active = model.active_profile.clone();
    let selected = profiles.iter().position(|p| p.name == active).unwrap_or(0);
    model.picker = Some(Picker::with_selected(
        PickerKind::Model { profiles, active },
        selected,
    ));
}

fn switch_saved_model<P: ProfileProvider>(model: &mut Model, provider: &P, name: &str) {
    if model.running {
        model.push_notice("finish or Esc the current turn before switching models");
        return;
    }
    let resolved: Result<config::Resolved, String> = match model.model_config.lock() {
        Ok(cfg) => config::resolve_model(&cfg, name).map_err(|e| e.to_string()),
        Err(_) => Err("model configuration is temporarily unavailable".into()),
    };
    match resolved {
        Ok(resolved) => {
            if let Err(error) = provider.switch_profile(&resolved) {
                model.push_notice(format!("could not switch model: {error}"));
                return;
            }
            model.model = resolved.provider.model;
            model.protocol = resolved.provider.protocol;
            model.reasoning_support = resolved.provider.reasoning;
            model.max_ctx = resolved.provider.max_ctx;
            model.ctx_pct = None;
            model.active_profile = resolved.name;
            model.push_notice(format!("switched to model profile '{name}'"));
        }
        Err(e) => model.push_notice(format!("could not switch model: {e}")),
    }
}

fn remove_saved_model(model: &mut Model, name: &str) {
    if name == model.active_profile {
        model.push_notice("switch to another model before removing the active profile");
        return;
    }
    let outcome: Result<(), String> = match model.model_config.lock() {
        Ok(mut cfg) => {
            let previous_default = cfg.default_model.clone();
            match cfg.remove_model(name) {
                Err(e) => Err(e.to_string()),
                Ok(removed) => match config::save(&cfg) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        cfg.models.insert(name.to_string(), removed);
                        cfg.default_model = previous_default;
                        Err(format!("could not write config: {e}"))
                    }
                },
            }
        }
        Err(_) => Err("model configuration is temporarily unavailable".into()),
    };
    match outcome {
        Ok(()) => model.push_notice(format!("removed model profile '{name}'")),
        Err(e) => model.push_notice(format!("could not remove model: {e}")),
    }
}

fn set_default_model(model: &mut Model, name: &str) {
    let outcome: Result<(), String> = match model.model_config.lock() {
        Ok(mut cfg) => {
            let previous = cfg.default_model.clone();
            match cfg.set_default_model(name) {
                Err(e) => Err(e.to_string()),
                Ok(()) => match config::save(&cfg) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        cfg.default_model = previous;
                        Err(format!("could not write config: {e}"))
                    }
                },
            }
        }
        Err(_) => Err("model configuration is temporarily unavailable".into()),
    };
    match outcome {
        Ok(()) => {
            model.picker = None;
            model.push_notice(format!(
                "'{name}' is now the default model for new sessions"
            ));
        }
        Err(e) => model.push_notice(format!("could not set default model: {e}")),
    }
}

/// A distinctive lead shared by every transient skill-hub hint so that clicking
/// hub actions refreshes ONE line via `upsert_notice` instead of stacking
/// identical notices (the notice-wall bug).
const HUB_LEAD: &str = "◈ ";

/// Standard, actionable "no sources yet" line — shared by search and the sources
/// view so the guidance is worded identically everywhere.
const NO_SOURCES: &str =
    "no skill sources yet — add one:  /skill sources add <owner/repo>  (e.g. anthropics/skills)";

/// Show a single, self-replacing skill-hub hint (never stacks).
fn hub_notice(model: &mut Model, text: impl std::fmt::Display) {
    model.upsert_notice(HUB_LEAD, format!("{HUB_LEAD}{text}"));
}

/// The registered skill sources; empty on any error (missing file / bad config).
fn registered_taps() -> Vec<tools::Tap> {
    config::user_taps_path()
        .ok()
        .map(tools::TapStore::new)
        .and_then(|s| s.list().ok())
        .unwrap_or_default()
}

/// Sources to browse/search: the shipped defaults plus the user's own (deduped),
/// so search works out of the box without registering anything.
fn browse_taps() -> Vec<tools::Tap> {
    let mut taps = tools::hub::default_taps();
    for t in registered_taps() {
        if !taps.iter().any(|d| d.key() == t.key()) {
            taps.push(t);
        }
    }
    taps
}

/// Open the interactive sources sub-picker: shipped built-ins (non-removable)
/// plus the user's own (removable), an "Add a source…" row, and "Back".
fn open_sources_picker(model: &mut Model) {
    let defaults = tools::hub::default_taps();
    let mut sources: Vec<(String, String, bool)> = defaults
        .iter()
        .map(|t| (t.repo.clone(), t.path.clone(), false))
        .collect();
    for t in registered_taps() {
        if defaults.iter().any(|d| d.key() == t.key()) {
            continue; // a user source shadowing a default: show once, as built-in
        }
        sources.push((t.repo.clone(), t.path.clone(), true));
    }
    model.picker = Some(Picker::new(PickerKind::SkillSources(sources)));
}

/// Remove a registered source by its `repo/path` key and report the result.
fn remove_source(model: &mut Model, key: &str) {
    let path = match config::user_taps_path() {
        Ok(p) => p,
        Err(e) => return hub_notice(model, format!("sources unavailable: {e}")),
    };
    match tools::TapStore::new(path).remove(key) {
        Ok(0) => hub_notice(model, format!("no source matching '{key}'")),
        Ok(n) => hub_notice(model, format!("✔ removed {n} source(s)")),
        Err(e) => hub_notice(model, format!("could not remove source: {e}")),
    }
}

/// `/skill add <word-or-link>` — the one friendly way to get a skill. A URL or
/// path installs it; anything else searches the catalog. Auto-detected, so a
/// user never has to choose between "install" and "search".
fn add_skill(model: &mut Model, input: &str, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let input = input.trim();
    if looks_like_source(input) {
        install_skill(model, input, tx);
    } else {
        // A word filters; empty browses the whole catalog (search treats an
        // empty query as "list everything from your sources").
        search_skills(model, input, tx);
    }
}

/// True when the input is a URL or filesystem path (→ install); otherwise it is
/// treated as a search term.
fn looks_like_source(s: &str) -> bool {
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.contains("github.com")
        || s.starts_with('/')
        || s.starts_with("~/")
        || s.starts_with("./")
        || s.starts_with("../")
        || std::path::Path::new(s).exists()
}

/// `/skill install <path-or-url>` — install a complete skill package into user
/// scope from a local folder, GitHub `/tree/` folder, or raw `SKILL.md`.
fn install_skill(model: &mut Model, src: &str, tx: &mpsc::UnboundedSender<TuiEvent>) {
    if src.is_empty() {
        model.push_notice("usage: /skill install <source>\n  source: local skill folder · GitHub /tree/ URL · local/raw SKILL.md");
        return;
    }
    let Some(store) = model.skills.clone() else {
        model.push_notice("skills unavailable in this session");
        return;
    };
    let src = src.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = store.install_from(&src).await;
        let _ = tx.send(TuiEvent::SkillInstalled(result));
    });
    model.push_notice("(installing skill …)");
}

/// `/skill sources` — list registered taps; `add`/`remove` to edit them. Config
/// is loaded on demand (sources are session-independent). Backend + validation
/// live in `tools::TapStore`/`Tap`; this only parses the subcommand and reports.
fn skill_sources(model: &mut Model, args: &str) {
    let path = match config::user_taps_path() {
        Ok(p) => p,
        Err(e) => return model.push_notice(format!("skill sources unavailable: {e}")),
    };
    let store = tools::TapStore::new(path);
    let (sub, rest) = match args.split_once(char::is_whitespace) {
        Some((s, r)) => (s, r.trim()),
        None => (args, ""),
    };
    match sub {
        "" | "list" => match store.list() {
            Ok(user) => {
                // Show the shipped default(s) first (marked built-in) so it's
                // clear search works out of the box, then the user's own.
                let defaults = tools::hub::default_taps();
                let mut s = String::from("skill sources:");
                for t in &defaults {
                    s.push_str(&format!("\n  · {}/{}  (built-in)", t.repo, t.path));
                }
                for t in &user {
                    if defaults.iter().any(|d| d.key() == t.key()) {
                        continue; // don't double-list a source that shadows a default
                    }
                    let r = t
                        .git_ref
                        .as_deref()
                        .map(|r| format!(" @{r}"))
                        .unwrap_or_default();
                    s.push_str(&format!("\n  · {}/{}{r}", t.repo, t.path));
                }
                s.push_str(
                    "\n  add: /skill sources add <owner/repo>   ·   remove: /skill sources remove <owner/repo>",
                );
                hub_notice(model, s);
            }
            Err(e) => hub_notice(model, format!("reading sources: {e}")),
        },
        "add" => {
            let (spec, path_arg) = match rest.split_once(char::is_whitespace) {
                Some((a, b)) => (a, Some(b.trim())),
                None => (rest, None),
            };
            match tools::Tap::parse(spec, path_arg) {
                Ok(tap) => match store.add(tap.clone()) {
                    Ok(true) => hub_notice(model, format!("✔ added source {}", tap.key())),
                    Ok(false) => hub_notice(model, format!("updated source {}", tap.key())),
                    Err(e) => hub_notice(model, format!("could not add source: {e}")),
                },
                Err(e) => hub_notice(
                    model,
                    format!("usage: /skill sources add <owner/repo> [subpath] — {e}"),
                ),
            }
        }
        "remove" | "rm" => {
            if rest.is_empty() {
                return hub_notice(model, "usage: /skill sources remove <owner/repo>");
            }
            match store.remove(rest) {
                Ok(0) => hub_notice(model, format!("no source matching '{rest}'")),
                Ok(n) => hub_notice(model, format!("✔ removed {n} source(s) matching '{rest}'")),
                Err(e) => hub_notice(model, format!("could not remove source: {e}")),
            }
        }
        other => hub_notice(
            model,
            format!("unknown: /skill sources {other} — use add / remove"),
        ),
    }
}

/// `/skill search <query>` — search the sources (shipped defaults + the user's)
/// and open a results picker. Metadata only; an empty query browses everything.
/// Async (network).
fn search_skills(model: &mut Model, query: &str, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let taps = browse_taps();
    if taps.is_empty() {
        return hub_notice(model, NO_SOURCES);
    }
    let count = taps.len();
    let q = query.trim().to_string();
    let label = if q.is_empty() {
        format!("browsing skills in {count} source(s) …")
    } else {
        format!("searching for '{q}' in {count} source(s) …")
    };
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = tools::hub::search(&taps, &q).await;
        let _ = tx.send(TuiEvent::SkillSearchResults(result));
    });
    hub_notice(model, format!("({label})"));
}

/// `/skill update [<name> | --all]` — check registered sources for newer
/// revisions of installed user skills. No argument only reports; a name or
/// `--all` applies available updates (guard-gated, atomic). Locally edited
/// skills are always protected. Async (network).
fn update_skills(model: &mut Model, arg: &str, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let Some(store) = model.skills.clone() else {
        return model.push_notice("skills unavailable in this session");
    };
    // Only active, user-scoped skills are updatable (project skills are committed
    // config; provenance exists only for installed user skills).
    let names: Vec<String> = store
        .discover(&model.known_tools)
        .effective()
        .filter(|l| l.skill.scope == tools::SkillScope::User)
        .map(|l| l.skill.name.clone())
        .collect();
    if names.is_empty() {
        return model.push_notice("no installed user skills to update");
    }
    let arg = arg.trim();
    let apply_all = arg == "--all" || arg == "all";
    let single = (!arg.is_empty() && !apply_all).then(|| arg.to_string());
    if let Some(name) = &single {
        if !names.iter().any(|n| n == name) {
            return model.push_notice(format!("no installed user skill named '{name}'"));
        }
    }
    let notice = match &single {
        _ if apply_all => "updating all skills".to_string(),
        Some(n) => format!("updating '{n}'"),
        None => "checking for skill updates".to_string(),
    };
    model.push_notice(format!("({notice} …)"));

    let tx = tx.clone();
    tokio::spawn(async move {
        let targets = single.clone().map(|n| vec![n]).unwrap_or(names);
        let mut lines = Vec::new();
        for name in targets {
            match tools::hub::check_update(&store, &name).await {
                tools::hub::UpdateStatus::UpToDate => lines.push(format!("✓ {name} — up to date")),
                tools::hub::UpdateStatus::ModifiedLocally => lines.push(format!(
                    "✎ {name} — modified locally (protected; not updated)"
                )),
                tools::hub::UpdateStatus::Unmanaged(reason) => {
                    lines.push(format!("· {name} — {reason}"))
                }
                tools::hub::UpdateStatus::Available { to, .. } => {
                    let short = &to[..to.len().min(8)];
                    if apply_all || single.is_some() {
                        match store.provenance(&name).map(|p| p.source) {
                            Some(source) => match store.install_from(&source).await {
                                Ok(r) => {
                                    let flag = if r.scan_verdict == "caution" {
                                        " (⚠ guard flagged — /skill info to review)"
                                    } else {
                                        ""
                                    };
                                    lines.push(format!("✔ updated {name} → {short}{flag}"));
                                }
                                Err(e) => lines.push(format!("✖ {name} — update failed: {e}")),
                            },
                            None => lines.push(format!("✖ {name} — source unavailable")),
                        }
                    } else {
                        lines.push(format!(
                            "↑ {name} — update available → {short}  (apply: /skill update {name})"
                        ));
                    }
                }
            }
        }
        if !apply_all && single.is_none() {
            lines.push("apply: /skill update <name> · /skill update --all".to_string());
        }
        let _ = tx.send(TuiEvent::SkillUpdateReport(lines));
    });
}

/// Put a command stub in the input box (cursor at end) and hint what to type
/// next — used by hub actions that need a free-text argument (a search query,
/// an install source) so the user completes one line instead of guessing syntax.
fn prefill_command(model: &mut Model, cmd: &str, hint: &str) {
    model.input = cmd.to_string();
    model.cursor = model.input.len();
    hub_notice(model, hint);
}

/// `/skill lock` — snapshot every installed user skill (that has a recorded
/// source) into the workspace lockfile for reproducible team setups. Local, sync.
fn lock_skills(model: &mut Model) {
    let Some(store) = model.skills.clone() else {
        return model.push_notice("skills unavailable in this session");
    };
    let names: Vec<String> = store
        .discover(&model.known_tools)
        .effective()
        .filter(|l| l.skill.scope == tools::SkillScope::User)
        .map(|l| l.skill.name.clone())
        .collect();
    let entries = tools::hub::lock_entries(&store, &names);
    let path = match config::skills_lock_path() {
        Ok(p) => p,
        Err(e) => return model.push_notice(format!("could not locate lockfile: {e}")),
    };
    match tools::SkillLock::new(path.clone()).write(entries.clone()) {
        Ok(()) => model.push_notice(format!(
            "✔ locked {} skill(s) → {}\n  commit it so your team can /skill sync the same set",
            entries.len(),
            path.display()
        )),
        Err(e) => model.push_notice(format!("could not write lockfile: {e}")),
    }
}

/// `/skill sync` — install/repair skills so they match the lockfile (pinned to
/// each locked revision). Skips entries already at their locked hash. Async.
fn sync_skills(model: &mut Model, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let Some(store) = model.skills.clone() else {
        return model.push_notice("skills unavailable in this session");
    };
    let path = match config::skills_lock_path() {
        Ok(p) => p,
        Err(e) => return model.push_notice(format!("could not locate lockfile: {e}")),
    };
    let entries = match tools::SkillLock::new(path).read() {
        Ok(e) => e,
        Err(e) => return model.push_notice(format!("could not read lockfile: {e}")),
    };
    if entries.is_empty() {
        return model.push_notice(
            "no lockfile (or it is empty) — run /skill lock first, or commit one from a teammate",
        );
    }
    model.push_notice(format!("(syncing {} locked skill(s) …)", entries.len()));
    let tx = tx.clone();
    tokio::spawn(async move {
        let mut lines = Vec::new();
        for entry in entries {
            // Already at the locked bytes → nothing to do.
            if entry.content_hash.is_some()
                && store.installed_hash(&entry.name) == entry.content_hash
            {
                lines.push(format!("✓ {} — already at locked revision", entry.name));
                continue;
            }
            match store.install_from(&tools::hub::locked_source(&entry)).await {
                Ok(r) => {
                    let flag = if r.scan_verdict == "caution" {
                        " (⚠ guard flagged — /skill info to review)"
                    } else {
                        ""
                    };
                    let verb = if r.replaced { "synced" } else { "installed" };
                    lines.push(format!("✔ {verb} {}{flag}", entry.name));
                }
                Err(e) => lines.push(format!("✖ {} — {e}", entry.name)),
            }
        }
        let _ = tx.send(TuiEvent::SkillUpdateReport(lines));
    });
}

fn show_skill_info(model: &mut Model, name: &str) {
    if name.is_empty() {
        model.push_notice("usage: /skill info <name>\n  Browse names with /skill or /skills");
        return;
    }
    let Some(store) = model.skills.as_ref() else {
        model.push_notice("skills unavailable in this session");
        return;
    };
    match store.inspect(name, &model.known_tools) {
        Ok(info) => {
            let text = |key: &str| {
                info.get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("—")
            };
            let available = info
                .get("available")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let required = info
                .get("required_tools")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "none".into());
            let missing = info
                .get("missing_tools")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let files = info
                .get("bundled_files")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let shown = files
                .iter()
                .filter_map(serde_json::Value::as_str)
                .take(12)
                .map(|file| format!("\n    {file}"))
                .collect::<String>();
            let more = files.len().saturating_sub(12);
            let more = if more > 0 {
                format!("\n    … and {more} more")
            } else {
                String::new()
            };
            let status = if available {
                "available".to_string()
            } else {
                format!("unavailable — missing {missing}")
            };
            let source = info
                .get("source")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("manual/project checkout");
            let revision = info
                .get("revision")
                .and_then(serde_json::Value::as_str)
                .map(|value| format!(" @ {}", &value[..value.len().min(8)]))
                .unwrap_or_default();
            model.push_notice(format!(
                "skill: {}\n  {}\n  scope: {} · status: {} · version: {}\n  required tools: {}\n  source: {}{}\n  path: {}\n  bundled files: {}{}{}",
                text("name"),
                text("description"),
                text("scope"),
                status,
                info.get("version").and_then(serde_json::Value::as_u64).unwrap_or(1),
                required,
                source,
                revision,
                text("path"),
                files.len(),
                shown,
                more,
            ));
        }
        Err(error) => model.push_notice(format!("skill '{name}': {error}")),
    }
}

fn begin_remove_skill(model: &mut Model, name: &str) {
    if name.is_empty() {
        model.push_notice(
            "usage: /skill remove <name>\n  Only user-scoped skills can be removed here",
        );
        return;
    }
    let Some(store) = model.skills.as_ref() else {
        model.push_notice("skills unavailable in this session");
        return;
    };
    let discovery = store.discover(&model.known_tools);
    let has_user_copy = discovery.listings.iter().any(|listing| {
        listing.skill.name == name && listing.skill.scope == tools::SkillScope::User
    });
    if !has_user_copy {
        let project_only = discovery.listings.iter().any(|listing| {
            listing.skill.name == name && listing.skill.scope == tools::SkillScope::Project
        });
        model.push_notice(if project_only {
            format!("'{name}' is a project skill; remove it from the repository instead")
        } else {
            format!("no user skill named '{name}' is installed")
        });
        return;
    }
    model.picker = Some(Picker::new(PickerKind::RemoveSkill(name.to_string())));
}

fn remove_user_skill(model: &mut Model, name: &str, transcript: &mut [Message]) {
    let Some(store) = model.skills.as_ref() else {
        model.push_notice("skills unavailable in this session");
        return;
    };
    match store.remove_user(name) {
        Ok(_) => {
            model.refresh_skill_manifest(transcript);
            model.push_notice(format!("removed user skill '{name}'"));
        }
        Err(error) => model.push_notice(format!("could not remove skill: {error}")),
    }
}

fn human_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Force-load a skill's full procedure into the conversation, deterministically
/// (no reliance on the model choosing to call `skill.load`). A model-independent
/// `/skill-name` trigger: the procedure lands in the transcript, so the next
/// turn the model *has* it. Empty name → open the picker instead. Reached from
/// `/skill <name>` and the skill picker.
fn load_skill_by_name(model: &mut Model, name: &str, transcript: &mut Vec<Message>) {
    if name.is_empty() {
        open_skill_picker(model);
        return;
    }
    let Some(store) = model.skills.clone() else {
        model.push_notice("skills unavailable in this session");
        return;
    };
    match store.load(name, &model.known_tools) {
        Ok(v) => {
            let body = v.get("procedure").and_then(|s| s.as_str()).unwrap_or("");
            let desc = v.get("description").and_then(|s| s.as_str()).unwrap_or("");
            // Inject as a user-role message so the model is guaranteed to see the
            // procedure on the next turn — same content `skill.load` would return.
            transcript.push(Message::user(format!(
                "[Loaded skill: {name}] Follow this procedure for the current and related work:\n\n{body}"
            )));
            model.push_notice(format!(
                "✔ loaded skill '{name}' — {desc}\n  It's in context now; tell me what to do and I'll follow it."
            ));
        }
        Err(e) => model.push_notice(format!("skill '{name}': {e}")),
    }
}

pub(super) fn run_slash<P: kernel::Provider>(
    model: &mut Model,
    cmd: &str,
    transcript: &[Message],
    provider: &P,
) {
    if let Some(rest) = cmd.strip_prefix("reasoning").filter(|r| is_cmd_boundary(r)) {
        apply_reasoning_command(model, provider, rest.trim());
        return;
    }

    if let Some(rest) = cmd.strip_prefix("stream").filter(|r| is_cmd_boundary(r)) {
        apply_stream_command(model, provider, rest.trim());
        return;
    }

    // Compatibility aliases. They are accepted but hidden from autocomplete
    // and help so the product presents one clear reasoning surface.
    if let Some(rest) = cmd.strip_prefix("think").filter(|r| is_cmd_boundary(r)) {
        let rest = rest.trim();
        if rest.is_empty() {
            open_reasoning_panel(model);
        } else if rest == "status" {
            model.push_notice(model.reasoning_status_block());
        } else {
            apply_reasoning_command(model, provider, rest);
        }
        return;
    }
    if let Some(rest) = cmd.strip_prefix("effort").filter(|r| is_cmd_boundary(r)) {
        let rest = rest.trim();
        if rest.is_empty() {
            open_reasoning_panel(model);
        } else {
            apply_reasoning_command(model, provider, &format!("effort {rest}"));
        }
        return;
    }
    match cmd {
        "exit" | "quit" => model.should_quit = true,
        "resume" => {
            // The actual session fetch is async (reads the event log); the caller
            // (handle_key) has the kernel handle and the tx channel, but run_slash
            // only gets the provider. So we set a flag the caller checks after
            // run_slash returns — it then spawns the log query.
            //
            // This is handled inline in handle_key's Enter path for /resume so we
            // have access to kernel.log. Here we just show a notice if somehow
            // reached directly.
            model.push_notice("(use /resume to pick a session — or type it and press Enter)");
        }
        "thinking" => {
            let visibility = if model.show_thinking { "hide" } else { "show" };
            apply_reasoning_command(model, provider, visibility);
        }
        "detail" => {
            model.full_transparency = !model.full_transparency;
            model.invalidate_all_renders();
            model.push_notice(if model.full_transparency {
                "detail: full tool input/output"
            } else {
                "detail: summarized"
            });
        }
        "tasks" => {
            let text = if model.bg_tasks.is_empty() {
                "background tasks: none".to_string()
            } else {
                let mut lines = String::from("background tasks:");
                for t in &model.bg_tasks {
                    let state = if t.running { "running" } else { "done" };
                    lines.push_str(&format!("\n  {} [{state}]  {}", t.id, t.command));
                }
                lines.push_str("\n\n(the agent polls with task.output and stops with task.kill)");
                lines
            };
            // Live status: refresh the previous /tasks block instead of
            // stacking identical copies.
            model.upsert_notice("background tasks", text);
        }
        "skills" => {
            // Re-scan live so a skill saved this session shows up; refresh the
            // previous /skills block instead of stacking copies.
            let text = model.skills_notice();
            model.upsert_notice("skills", text);
        }
        "help" => {
            let mut text = COMMANDS
                .iter()
                .map(|(c, d)| format!("{c}  {d}"))
                .collect::<Vec<_>>()
                .join("\n");
            text.push_str(
                "\n\nshortcuts:\n\n  Esc     interrupt a running turn\n  Ctrl-D  quit\n  ↑/↓     scroll (empty input) · history (while typing)\n  Mouse   drag copies · double-click word · triple-click line",
            );
            model.push_notice(text);
        }
        c if c == "theme" || c.starts_with("theme ") => {
            apply_theme_command(model, c.strip_prefix("theme").unwrap_or("").trim());
        }
        // `/clear` is handled in `handle_key` (via `do_clear`) where the
        // transcript and session are mutable; this arm is only a fallback.
        "clear" => model.push_notice("(use /clear to reset the conversation)"),
        "status" => {
            let toks: usize = transcript.iter().map(|m| m.content.len() / 4).sum();
            let ctx = match model.max_ctx {
                Some(mc) => format!("{mc} window"),
                None => "unknown window".to_string(),
            };
            model.push_notice(format!(
                "model: {} ({})  |  {ctx}  |  ~{toks} est. tokens\n\n{}",
                model.model,
                model.active_profile,
                model.reasoning_status_block()
            ));
        }
        c if c == "pulse" || c.strip_prefix("pulse").is_some_and(is_cmd_boundary) => {
            let fix = c.strip_prefix("pulse").unwrap_or("").trim() == "fix";
            // Snapshot the live profile registry for a prompt-free config read.
            let snapshot = model.model_config.lock().ok().map(|g| g.clone());
            let report = config::pulse(snapshot.as_ref(), None, None);
            let mut text = report.render();
            if fix {
                if let Ok(mut guard) = model.model_config.lock() {
                    let applied = config::apply_safe_fixes(&mut guard);
                    if applied.is_empty() {
                        text.push_str("\nfix: nothing to repair.");
                    } else {
                        match config::save(&guard) {
                            Ok(()) => {
                                text.push_str(&format!(
                                    "\nfix: applied {} repair(s):",
                                    applied.len()
                                ));
                                for line in applied {
                                    text.push_str(&format!("\n  ✔ {line}"));
                                }
                            }
                            Err(error) => {
                                text.push_str(&format!(
                                    "\nfix: could not persist repairs: {error}"
                                ));
                            }
                        }
                    }
                }
            } else if report.has_fixes() {
                text.push_str("\n(run `/pulse fix` to apply the fixable items)");
            }
            // Live health: refresh the previous /pulse block instead of stacking.
            model.upsert_notice("pulse", text);
        }
        other => model.push_notice(format!("unknown command: /{other}")),
    }
}

/// `/theme` opens the picker (↑↓ + Enter). `/theme light|dark|auto|toggle`
/// applies directly without the menu. `auto` re-detects from the terminal.
fn apply_theme_command(model: &mut Model, arg: &str) {
    match arg {
        "" => open_theme_picker(model),
        "light" | "dark" | "auto" => set_theme(model, arg),
        "toggle" => {
            let id = if super::theme::current().is_dark {
                "light"
            } else {
                "dark"
            };
            set_theme(model, id);
        }
        _ => model.push_notice(
            "usage: /theme [light|dark|auto]  (bare /theme opens a picker)".to_string(),
        ),
    }
}

/// Open the `/theme` picker, cursor on the current mode.
pub(super) fn open_theme_picker(model: &mut Model) {
    let sel = if super::theme::current().is_dark {
        0
    } else {
        1
    };
    model.picker = Some(Picker::with_selected(PickerKind::Theme, sel));
}

/// Apply a theme by its [`THEME_MODES`] id and re-colour the UI live.
fn set_theme(model: &mut Model, id: &str) {
    use super::theme;
    let palette = match id {
        "light" => theme::Palette::light(),
        "auto" => theme::detect(),
        _ => theme::Palette::dark(),
    };
    let mode = if palette.is_dark { "dark" } else { "light" };
    theme::set(palette);
    model.invalidate_all_renders();
    model.push_notice(format!("theme: {mode}"));
}

fn open_reasoning_panel(model: &mut Model) {
    let state = ReasoningPanelState::from_model(model);
    model.picker = Some(Picker::new(PickerKind::Reasoning(state)));
}

fn apply_stream_command<P: kernel::Provider>(model: &mut Model, provider: &P, args: &str) {
    let on = match args {
        "" => !provider.streaming(), // bare `/stream` toggles
        "on" => true,
        "off" => false,
        "status" => provider.streaming(),
        _ => {
            model.push_notice("usage: /stream [on|off]  (bare /stream toggles)".to_string());
            return;
        }
    };
    provider.set_streaming(on);
    model.streaming = on;
    let msg = if on {
        "streaming on — replies render token-by-token"
    } else {
        "streaming off — each reply arrives whole when the model finishes (surfaces reasoning on gateways that only send it non-streamed)"
    };
    model.push_notice(format!("✔ {msg}"));
}

fn apply_reasoning_command<P: kernel::Provider>(model: &mut Model, provider: &P, args: &str) {
    let result = match args {
        "" => {
            open_reasoning_panel(model);
            return;
        }
        "status" => Ok(()),
        "on" => {
            let effort = provider.reasoning().effort;
            set_reasoning_checked(
                model,
                provider,
                kernel::ReasoningConfig {
                    enabled: Some(true),
                    effort,
                },
            )
        }
        "off" => set_reasoning_checked(
            model,
            provider,
            kernel::ReasoningConfig {
                enabled: Some(false),
                effort: None,
            },
        ),
        "show" | "hide" => {
            model.show_thinking = args == "show";
            model.invalidate_all_renders();
            Ok(())
        }
        "effort auto" => set_reasoning_checked(
            model,
            provider,
            kernel::ReasoningConfig {
                enabled: None,
                effort: None,
            },
        ),
        "effort minimal" | "effort low" | "effort medium" | "effort high" => {
            let effort = match args.strip_prefix("effort ").unwrap_or("") {
                "minimal" => kernel::ReasoningEffort::Minimal,
                "low" => kernel::ReasoningEffort::Low,
                "medium" => kernel::ReasoningEffort::Medium,
                _ => kernel::ReasoningEffort::High,
            };
            set_reasoning_checked(
                model,
                provider,
                kernel::ReasoningConfig {
                    enabled: Some(true),
                    effort: Some(effort),
                },
            )
        }
        _ => Err(
            "usage: /reasoning [on|off|show|hide|status|effort auto|minimal|low|medium|high]"
                .to_string(),
        ),
    };

    match result {
        Ok(()) => model.push_notice(model.reasoning_status_block()),
        Err(usage) => model.push_notice(usage),
    }
}

#[cfg(test)]
mod fix_tests {
    use super::*;
    use std::collections::HashMap;

    fn model() -> Model {
        let dir = std::env::temp_dir().join(format!("medha-upd-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let sbx = Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap());
        Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            lockfile::UiConfig::default(),
            HashMap::new(),
            sbx,
        )
    }

    fn input_kernel() -> Arc<Kernel<providers::OpenAiCompat, kernel::InMemoryLog>> {
        let dir = std::env::temp_dir().join(format!("medha-input-{}", ulid::Ulid::new()));
        Arc::new(Kernel::new(
            Arc::new(providers::OpenAiCompat::new("http://localhost/v1", "", "m")),
            Arc::new(kernel::InMemoryLog::new()),
            Arc::new(tools::ToolRegistry::new()),
            Arc::new(context::PipelineEngine::default()),
            Arc::new(store::FileArtifactStore::open(dir).unwrap()),
            Arc::new(kernel::AllowAll),
            Arc::new(kernel::AutoDeny),
            Arc::new(kernel::NoVerify),
        ))
    }

    // ── slash ROUTING (the bug that shipped: `/skill` reached only one of the
    //    two Enter paths and fell through to "unknown command"). Both paths now
    //    route through classify_slash; this pins that routing. ──────────────────
    #[test]
    fn classify_slash_routes_steer_with_its_whole_message() {
        // The message is free text and must survive intact — splitting it or
        // trimming inside it would silently alter what the agent is told.
        assert_eq!(
            classify_slash("steer only the parser, skip the lexer"),
            SlashAction::Steer("only the parser, skip the lexer".into())
        );
        assert_eq!(classify_slash("steer"), SlashAction::Steer(String::new()));
        // `/steering` is not `/steer` — a prefix match without a boundary would
        // hijack any future command starting with these letters.
        assert_ne!(
            classify_slash("steering"),
            SlashAction::Steer(String::new())
        );
    }

    #[test]
    fn classify_slash_routes_skill_commands() {
        assert_eq!(classify_slash("skill"), SlashAction::SkillPicker);
        assert_eq!(
            classify_slash("skill frontend-ui-design"),
            SlashAction::LoadSkill("frontend-ui-design".into())
        );
        // `/skills` (list) must NOT be mistaken for `/skill` (load).
        assert_eq!(classify_slash("skills"), SlashAction::Other);
        assert_eq!(classify_slash("model"), SlashAction::ModelPicker);
        assert_eq!(classify_slash("model add"), SlashAction::AddModel);
        assert_eq!(classify_slash("memory"), SlashAction::Memory(String::new()));
        assert_eq!(
            classify_slash("memory quoted-fact"),
            SlashAction::Memory("quoted-fact".into())
        );
        // `/model <name>` switches directly, matching other agent CLIs.
        assert_eq!(
            classify_slash("model fast-local"),
            SlashAction::SwitchModel("fast-local".into())
        );
        // `/skill install <src>` routes to the installer, not skill loading.
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
        // Subcommand prefixes require a word boundary; valid skill names that
        // merely begin with one must still route as names.
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
        // a name that merely starts with "add" still loads as a skill name
        assert_eq!(
            classify_slash("skill adder"),
            SlashAction::LoadSkill("adder".into())
        );
    }

    #[test]
    fn memory_picker_shows_trust_age_and_provenance_action() {
        let dir = std::env::temp_dir().join(format!("medha-memory-notice-{}", ulid::Ulid::new()));
        let store = memory::MemoryProjection::open(dir.join("p.db"), dir.join("u.db")).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        store
            .apply(&memory::MemoryOp::Write {
                entry: memory::MemoryEntry {
                    name: "quoted-fact".into(),
                    claim: "the 'quoted' claim".into(),
                    description: "a hyphenated hook".into(),
                    kind: memory::MemoryKind::Project,
                    scope: memory::Scope::Project,
                    trust: kernel::TrustLabel::User,
                    confidence: memory::ConfidenceRung::UserStated,
                    provenance: vec![ulid::Ulid::new()],
                    sessions: vec![ulid::Ulid::new()],
                    version: 1,
                    pinned: true,
                    links: vec![],
                    created: now - 2.0 * 86_400.0,
                    updated: now - 2.0 * 86_400.0,
                },
            })
            .unwrap();
        let picker = PickerKind::Memory(store.list().unwrap());
        let title = picker.title();
        assert!(title.contains("provenance"));
        assert!(title.contains("p pin/unpin") && title.contains("f forget"));
        let labels = picker.labels();
        assert!(labels[0].contains("[user · 2d · pinned]"));
        assert!(labels[0].contains("quoted-fact — a hyphenated hook"));
    }

    #[test]
    fn add_auto_detects_link_or_path_vs_search_term() {
        // links / paths → install
        assert!(looks_like_source(
            "https://github.com/anthropics/skills/tree/main/skills/pdf"
        ));
        assert!(looks_like_source("github.com/owner/repo"));
        assert!(looks_like_source("/tmp/my-skill"));
        assert!(looks_like_source("~/skills/foo"));
        assert!(looks_like_source("./local"));
        // plain words → search
        assert!(!looks_like_source("pdf"));
        assert!(!looks_like_source("excel spreadsheet"));
    }

    #[test]
    fn skill_hub_lists_actions_then_installed_skills() {
        // The Enter dispatch indexes skills as `selected - SKILL_HUB_ACTIONS.len()`,
        // so the layout must be exactly: every action (in order), then the skills.
        let kind = PickerKind::Skill(vec![("deploy".into(), "[user] ship it".into())]);
        let labels = kind.labels();
        assert_eq!(labels.len(), SKILL_HUB_ACTIONS.len() + 1);
        for (i, (label, _)) in SKILL_HUB_ACTIONS.iter().enumerate() {
            assert_eq!(&labels[i], label, "action row {i} out of order");
        }
        assert_eq!(labels[SKILL_HUB_ACTIONS.len()], "deploy — [user] ship it");
        // …but a name that merely starts with "sources" still loads as a name.
        assert_eq!(
            classify_slash("skill sources-of-truth"),
            SlashAction::LoadSkill("sources-of-truth".into())
        );
        assert_eq!(classify_slash("resume"), SlashAction::Resume);
        assert_eq!(classify_slash("clear"), SlashAction::Clear);
        assert_eq!(classify_slash("lsp"), SlashAction::Lsp);
        assert_eq!(classify_slash("help"), SlashAction::Other);
        assert_eq!(classify_slash("search"), SlashAction::SearchConfig);
        assert_eq!(classify_slash("mode"), SlashAction::ModePicker);
        assert_eq!(
            classify_slash("mode yolo"),
            SlashAction::SwitchMode("yolo".into())
        );
    }

    // ── /search opens the provider picker with the current choice preselected ──
    #[test]
    fn begin_search_setup_opens_provider_picker() {
        let mut m = model();
        begin_search_setup(&mut m);
        assert!(m.search_setup.is_some(), "a draft must be started");
        assert!(
            matches!(
                m.picker.as_ref().map(|p| &p.kind),
                Some(PickerKind::SearchProvider)
            ),
            "the provider picker must be open"
        );
        // Unconfigured → DuckDuckGo row (index 0) preselected.
        assert_eq!(m.picker.as_ref().unwrap().selected, 0);
        // Esc-equivalent cleanup leaves no dangling draft.
        m.search_setup = None;
    }

    // ── a keyed provider advances to the masked Secret step, not a direct commit ─
    #[test]
    fn keyed_provider_needs_a_secret_step() {
        // Tavily/Brave mask input; SearXNG (a URL) does not; DuckDuckGo is not secret.
        let mut s = SearchSetup::new();
        s.provider = tools::SearchProvider::Tavily;
        s.step = SearchSetupStep::Secret;
        assert!(s.is_secret(), "an API key must be masked");
        s.provider = tools::SearchProvider::Searxng;
        assert!(!s.is_secret(), "a SearXNG URL must stay visible");
    }

    // ── /skill <name> force-loads the procedure into the transcript ───────────
    #[test]
    fn load_skill_injects_procedure_into_transcript() {
        let dir = std::env::temp_dir().join(format!("medha-loadskill-{}", ulid::Ulid::new()));
        let user = dir.join("skills");
        std::fs::create_dir_all(user.join("greet")).unwrap();
        std::fs::write(
            user.join("greet").join("SKILL.md"),
            "---\nname = \"greet\"\ndescription = \"say hi\"\n---\n\nStep 1: say hello",
        )
        .unwrap();
        let store = Arc::new(tools::SkillStore::new(dir.join("noproj"), Some(user)));
        let mut m = model().with_skills(store, std::collections::HashSet::new());
        let mut transcript = vec![Message::system("S")];

        load_skill_by_name(&mut m, "greet", &mut transcript);
        let injected = &transcript.last().unwrap().content;
        assert!(
            injected.contains("say hello"),
            "procedure body must be injected"
        );
        assert!(injected.contains("Loaded skill: greet"));

        // Unknown skill → no injection, a clear notice instead.
        let n = transcript.len();
        load_skill_by_name(&mut m, "nope", &mut transcript);
        assert_eq!(
            transcript.len(),
            n,
            "unknown skill must not inject anything"
        );

        // Empty name → opens the picker (no injection), listing the one skill.
        load_skill_by_name(&mut m, "", &mut transcript);
        assert!(matches!(&m.picker, Some(p) if matches!(p.kind, PickerKind::Skill(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── K6: `/think` must not capture `/thinking` ─────────────────────────────
    #[test]
    fn cmd_boundary_distinguishes_think_from_thinking() {
        // `strip_prefix("think")` on "thinking" leaves "ing" (no boundary) → not
        // the think command; on "think"/"think high" it leaves ""/" high".
        assert!(is_cmd_boundary("")); // /think
        assert!(is_cmd_boundary(" high")); // /think high
        assert!(!is_cmd_boundary("ing")); // /thinking → must fall through
    }

    #[test]
    fn unified_reasoning_command_controls_every_setting() {
        let provider = providers::OpenAiCompat::new("http://localhost/v1", "", "m");
        let mut m = model();
        let transcript = vec![Message::system("S")];

        run_slash(&mut m, "reasoning on", &transcript, &provider);
        assert_eq!(m.reasoning.enabled, None);
        assert_eq!(m.reasoning.effort, None);
        assert!(matches!(
            m.items.back().map(|entry| &entry.item),
            Some(Item::Notice(message)) if message.contains("explicit effort")
        ));

        run_slash(&mut m, "reasoning show", &transcript, &provider);
        assert!(m.show_thinking);

        run_slash(&mut m, "reasoning effort minimal", &transcript, &provider);
        assert_eq!(m.reasoning.effort, Some(kernel::ReasoningEffort::Minimal));

        run_slash(&mut m, "reasoning effort high", &transcript, &provider);
        assert_eq!(m.reasoning.effort, Some(kernel::ReasoningEffort::High));

        run_slash(&mut m, "reasoning effort auto", &transcript, &provider);
        assert_eq!(m.reasoning.effort, None);
        assert_eq!(m.reasoning.enabled, None);

        run_slash(&mut m, "reasoning", &transcript, &provider);
        assert!(matches!(
            &m.picker,
            Some(Picker {
                kind: PickerKind::Reasoning(_),
                ..
            })
        ));
        let labels = m.picker.as_ref().unwrap().kind.labels();
        assert_eq!(labels[0], "Mode:       Server default");
        assert_eq!(labels[1], "Visibility: Shown");
        assert_eq!(labels[2], "Effort:     Auto");
        assert_eq!(labels[3], "Support:    unverified");
        assert_eq!(labels[4], "Last turn:  No completed turn yet");
    }

    #[test]
    fn running_reasoning_panel_consumes_first_esc_then_second_cancels() {
        let kernel = input_kernel();
        let mut m = model();
        let mut session = Session::new();
        let mut transcript = vec![Message::system("S")];
        let budget = Budget::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let (handle, queue) = kernel::InterruptQueue::pair();

        m.running = true;
        m.interrupt = Some(handle.clone());
        run_slash(&mut m, "reasoning", &transcript, kernel.provider.as_ref());
        assert!(m.picker.is_some());

        handle_key(
            &mut m,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &kernel,
            &mut session,
            &mut transcript,
            &budget,
            &tx,
        );
        assert!(m.picker.is_none());
        assert!(!queue.cancel_requested());

        handle_key(
            &mut m,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &kernel,
            &mut session,
            &mut transcript,
            &budget,
            &tx,
        );
        assert!(queue.cancel_requested());
    }

    #[test]
    fn ctrl_d_remains_a_global_exit_while_a_running_picker_is_open() {
        let kernel = input_kernel();
        let mut m = model();
        let mut session = Session::new();
        let mut transcript = vec![Message::system("S")];
        let budget = Budget::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let (handle, queue) = kernel::InterruptQueue::pair();

        m.running = true;
        m.interrupt = Some(handle);
        run_slash(&mut m, "reasoning", &transcript, kernel.provider.as_ref());
        assert!(m.picker.is_some());

        handle_key(
            &mut m,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &kernel,
            &mut session,
            &mut transcript,
            &budget,
            &tx,
        );
        assert!(m.should_quit);
        assert!(queue.cancel_requested());
    }

    #[test]
    fn hidden_picker_does_not_steal_esc_from_a_visible_approval() {
        let kernel = input_kernel();
        let mut m = model();
        let mut session = Session::new();
        let mut transcript = vec![Message::system("S")];
        let budget = Budget::default();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (interrupt, queue) = kernel::InterruptQueue::pair();
        let (approval_tx, mut approval_rx) = oneshot::channel();

        m.running = true;
        m.interrupt = Some(interrupt);
        run_slash(&mut m, "reasoning", &transcript, kernel.provider.as_ref());
        m.pending_approvals.push_back(PendingApproval {
            action: "test".into(),
            detail: None,
            escalated: false,
            responder: approval_tx,
        });

        handle_key(
            &mut m,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &kernel,
            &mut session,
            &mut transcript,
            &budget,
            &event_tx,
        );
        assert!(queue.cancel_requested());
        assert!(m.picker.is_none());
        assert!(m.pending_approvals.is_empty());
        assert_eq!(approval_rx.try_recv(), Ok(kernel::Approval::Deny));
    }

    #[test]
    fn reasoning_delivery_is_recorded_when_turn_finishes() {
        let mut m = model();
        let mut session = Session::new();
        let mut transcript = vec![Message::system("S")];

        m.running = true;
        m.reasoning_received_this_turn = false;
        handle_agent_event(
            &mut m,
            TuiEvent::Done(transcript.clone(), StopReason::Finished),
            &mut session,
            &mut transcript,
        );
        assert_eq!(m.last_turn_reasoning_received, Some(false));
        assert_eq!(m.reasoning_trace_label(), "no trace");

        m.running = true;
        m.reasoning_received_this_turn = false;
        handle_agent_event(
            &mut m,
            TuiEvent::Reasoning("planning".into()),
            &mut session,
            &mut transcript,
        );
        assert_eq!(m.reasoning_trace_label(), "receiving");
        handle_agent_event(
            &mut m,
            TuiEvent::Done(transcript.clone(), StopReason::Finished),
            &mut session,
            &mut transcript,
        );
        assert_eq!(m.last_turn_reasoning_received, Some(true));
        assert_eq!(m.reasoning_trace_label(), "received");
    }

    // ── K5: `/clear` truncates the transcript and starts a fresh session ──────
    #[test]
    fn clear_truncates_transcript_and_starts_fresh_session() {
        let mut m = model();
        let mut session = Session::new();
        let old_id = session.id;
        let mut transcript = vec![
            Message::system("SYS"),
            Message::user("hello"),
            Message::assistant_calls("hi", vec![]),
        ];
        m.items.push_back(Entry::new(Item::User("hello".into())));

        do_clear(&mut m, &mut session, &mut transcript);

        assert_eq!(transcript.len(), 1, "only the system prompt survives");
        assert_eq!(transcript[0].content, "SYS");
        // Conversation items are gone (only the "(cleared)" notice remains).
        assert!(
            !m.items
                .iter()
                .any(|e| matches!(&e.item, Item::User(t) if t == "hello")),
            "prior conversation items cleared"
        );
        assert_ne!(session.id, old_id, "a fresh session id is minted");
    }

    #[test]
    fn clear_is_refused_mid_turn() {
        let mut m = model();
        m.running = true;
        let mut session = Session::new();
        let id = session.id;
        let mut transcript = vec![Message::system("SYS"), Message::user("x")];
        do_clear(&mut m, &mut session, &mut transcript);
        assert_eq!(
            transcript.len(),
            2,
            "transcript untouched while a turn runs"
        );
        assert_eq!(session.id, id);
    }

    // ── K8: ending a turn denies queued approvals so the card can't freeze input ──
    #[tokio::test]
    async fn deny_pending_approvals_answers_and_clears_the_queue() {
        let mut m = model();
        let (tx, rx) = oneshot::channel();
        m.pending_approvals.push_back(PendingApproval {
            action: "shell.exec".into(),
            detail: None,
            escalated: false,
            responder: tx,
        });
        m.deny_pending_approvals();
        assert!(m.pending_approvals.is_empty(), "queue drained");
        assert!(
            matches!(rx.await, Ok(kernel::Approval::Deny)),
            "dangling responder got Deny"
        );
    }

    // ── clarify form ─────────────────────────────────────────────────────────
    fn clarify_state(
        multi: bool,
        recommended: Option<usize>,
        responder: oneshot::Sender<Option<Vec<kernel::Answer>>>,
    ) -> ClarifyState {
        let options = vec![
            kernel::QOption {
                label: "Postgres".into(),
                description: String::new(),
                recommended: recommended == Some(0),
            },
            kernel::QOption {
                label: "SQLite".into(),
                description: String::new(),
                recommended: recommended == Some(1),
            },
        ];
        let selected = if !multi {
            recommended.map(|i| vec![i]).unwrap_or_default()
        } else {
            vec![]
        };
        ClarifyState {
            questions: vec![kernel::Question {
                prompt: "Which DB?".into(),
                header: "DB".into(),
                options,
                multi_select: multi,
            }],
            idx: 0,
            drafts: vec![ClarifyDraft {
                selected,
                other: None,
            }],
            cursor: recommended.unwrap_or(0),
            entering_other: false,
            other_input: String::new(),
            other_cursor: 0,
            validation: None,
            responder,
        }
    }
    fn kc(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn clarify_enter_submits_the_recommended_default() {
        let mut m = model();
        let (tx, mut rx) = oneshot::channel();
        m.clarify = Some(clarify_state(false, Some(0), tx)); // Postgres pre-selected
        handle_clarify_key(&mut m, kc(KeyCode::Enter));
        assert!(m.clarify.is_none(), "form closed on submit");
        let ans = rx.try_recv().expect("responder fired").expect("answers");
        assert_eq!(ans[0].selected, vec!["Postgres".to_string()]);
    }

    #[test]
    fn clarify_space_toggles_multi_select() {
        let mut m = model();
        let (tx, mut rx) = oneshot::channel();
        m.clarify = Some(clarify_state(true, None, tx));
        // cursor on option 0 → space selects it; move down, space selects option 1.
        handle_clarify_key(&mut m, kc(KeyCode::Char(' ')));
        handle_clarify_key(&mut m, kc(KeyCode::Down));
        handle_clarify_key(&mut m, kc(KeyCode::Char(' ')));
        handle_clarify_key(&mut m, kc(KeyCode::Enter)); // submit
        let ans = rx.try_recv().unwrap().unwrap();
        assert_eq!(
            ans[0].selected,
            vec!["Postgres".to_string(), "SQLite".to_string()]
        );
    }

    #[test]
    fn clarify_esc_dismisses_with_none() {
        let mut m = model();
        let (tx, mut rx) = oneshot::channel();
        m.clarify = Some(clarify_state(false, Some(0), tx));
        handle_clarify_key(&mut m, kc(KeyCode::Esc));
        assert!(m.clarify.is_none());
        assert!(
            rx.try_recv().expect("responder fired").is_none(),
            "dismiss → None"
        );
    }

    #[test]
    fn clarify_enter_commits_the_focused_radio_option() {
        let mut m = model();
        let (tx, mut rx) = oneshot::channel();
        let mut state = clarify_state(false, Some(0), tx);
        state.cursor = 1; // move focus away from the recommended Postgres row
        m.clarify = Some(state);

        handle_clarify_key(&mut m, kc(KeyCode::Enter));

        let ans = rx.try_recv().expect("responder fired").expect("answers");
        assert_eq!(ans[0].selected, vec!["SQLite".to_string()]);
    }

    #[test]
    fn clarify_rejects_an_unanswered_radio_question() {
        let mut m = model();
        let (tx, _rx) = oneshot::channel();
        m.clarify = Some(clarify_state(false, None, tx));

        submit_clarify(&mut m);

        let state = m.clarify.as_ref().expect("incomplete form stays open");
        assert!(state.validation.is_some(), "validation is shown inline");
        assert_eq!(state.idx, 0);
    }

    #[test]
    fn clarify_other_is_unicode_safe_and_preserves_the_composer() {
        let mut m = model();
        m.input = "keep this steer".into();
        m.cursor = m.input.len();
        let (tx, _rx) = oneshot::channel();
        let mut state = clarify_state(false, None, tx);
        state.cursor = state.other_row();
        m.clarify = Some(state);

        handle_clarify_key(&mut m, kc(KeyCode::Char(' '))); // Space opens Other
        handle_clarify_key(&mut m, kc(KeyCode::Char('é')));
        handle_clarify_key(&mut m, kc(KeyCode::Char('🙂')));
        handle_clarify_key(&mut m, kc(KeyCode::Enter)); // save Other

        assert_eq!(m.input, "keep this steer", "main composer was untouched");
        let state = m.clarify.as_mut().expect("form remains open");
        assert_eq!(state.drafts[0].other.as_deref(), Some("é🙂"));
        state.cursor = state.other_row();

        handle_clarify_key(&mut m, kc(KeyCode::Char(' '))); // Space reopens saved Unicode
        let state = m.clarify.as_ref().unwrap();
        assert_eq!(state.other_cursor, "é🙂".len(), "cursor is a byte offset");
        assert!(state.other_input.is_char_boundary(state.other_cursor));

        // Moving across both multi-byte characters must preserve the invariant.
        handle_clarify_key(&mut m, kc(KeyCode::Left));
        handle_clarify_key(&mut m, kc(KeyCode::Left));
        handle_clarify_key(&mut m, kc(KeyCode::Right));
        let state = m.clarify.as_ref().unwrap();
        assert!(state.other_input.is_char_boundary(state.other_cursor));
        assert_eq!(m.input, "keep this steer");
    }

    #[test]
    fn token_hit_test_uses_terminal_cells() {
        let line = "see src/lib.rs and https://x.dev/a";
        assert_eq!(token_at(line, 0).as_deref(), Some("see"));
        assert_eq!(token_at(line, 4).as_deref(), Some("src/lib.rs"));
        assert_eq!(token_at(line, 20).as_deref(), Some("https://x.dev/a"));
        assert_eq!(token_at(line, 3), None); // the space between tokens
        // A wide glyph earlier in the line must not shift later hit tests.
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
        // Not a locator: a Windows drive letter must survive intact.
        assert_eq!(strip_locator("C:\\src\\lib.rs"), "C:\\src\\lib.rs");
    }

    #[test]
    fn only_urls_and_existing_paths_are_openable() {
        assert!(matches!(
            target_at("visit https://medha.dev/docs now", 6),
            Some(OpenTarget::Url(url)) if url == "https://medha.dev/docs"
        ));
        // A word that is not a path resolves to nothing, so no app is launched.
        assert!(target_at("just some prose here", 5).is_none());
        assert!(target_at("see does/not/exist.rs", 4).is_none());
    }
}
