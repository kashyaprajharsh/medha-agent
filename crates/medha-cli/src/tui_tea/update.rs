//! Update/input/async logic: the TEA update fn, key handling, and the
//! StreamSink→TuiEvent bridge. Split out of the old monolithic tui_tea.rs.
#![allow(clippy::too_many_arguments)]
use super::*;
use sandbox::WorkspaceSandbox;

pub(super) fn update<P, L>(model: &mut Model, msg: Msg, kernel: &Arc<Kernel<P, L>>, session: &mut Session, transcript: &mut Vec<Message>, budget: &Budget, tx: &mpsc::UnboundedSender<TuiEvent>) 
where
    P: Provider + 'static,
    L: EventLog + 'static,
{
    match msg {
        Msg::KeyPress(key) => handle_key(model, key, kernel, session, transcript, budget, tx),
        Msg::MouseScroll(delta) => model.scroll_by(delta),
        Msg::Paste(data) => handle_paste(model, data),
        Msg::Resize(height) => {
            model.viewport_height = height as usize;
            model.auto_scroll = model.scroll_offset >= model.max_scroll();
        }
        Msg::AgentEvent(ev) => handle_agent_event(model, ev, session, transcript),
        Msg::Tick => {
            model.anim_frame = model.anim_frame.wrapping_add(1);
            if let Some(f) = model.intro_frame {
                model.intro_frame = if f >= 40 { None } else { Some(f + 1) };
            }
            // Refresh the live background-task list a few times a second (cheap
            // mutex read) so the status-line indicator tracks reality.
            if model.anim_frame % 16 == 0 {
                model.bg_tasks = kernel.executor.background_tasks();
            }
        }
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
    P: Provider + 'static,
    L: EventLog + 'static,
{
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Esc is reserved for stopping a running turn, not for answering an
    // approval. Intercept it before the approval branch so Esc always does
    // what the user expects — interrupt — instead of silently denying
    // whichever prompt happens to be on screen. The cancel is GRACEFUL: the
    // kernel lets in-flight tools settle (bounded) and returns Done with
    // StopReason::Interrupted — the handle stays until then.
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
        return;
    }

    // Inline approval handling (PART 3) — input captured when last item is approval
    if model.pending_approval().is_some() {
        handle_approval_key(model, key);
        return;
    }

    if handle_reasoning_picker_key(model, key, kernel.provider.as_ref()) {
        return;
    }

    // Picker handling
    if let Some(picker) = model.picker.as_mut() {
        let labels = picker.kind.labels();
        match key.code {
            KeyCode::Up => picker.selected = picker.selected.checked_sub(1).unwrap_or(labels.len().saturating_sub(1)),
            KeyCode::Down => picker.selected = (picker.selected + 1) % labels.len().max(1),
            KeyCode::Enter => {
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
                            let _ = tx.send(TuiEvent::Resumed(id, msgs));
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
                    let scope = point.scope_options().get(picker.selected).and_then(|(_, s)| *s);
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
                // Skill picker: a name was chosen → force-load its procedure into
                // the transcript (same as `/skill <name>`).
                if let PickerKind::Skill(skills) = &picker.kind {
                    let name = skills.get(picker.selected).map(|(n, _)| n.clone());
                    if let Some(name) = name {
                        model.picker = None;
                        load_skill_by_name(model, &name, transcript);
                    } else {
                        model.picker = None;
                    }
                    return;
                }
            }
            KeyCode::Esc => model.picker = None,
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
                KeyCode::Enter if !key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
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
            model.push_notice(if model.show_summary { "summaries: expanded (^E)" } else { "summaries: collapsed (^E)" });
        }
        KeyCode::Esc if model.running => {
            // Unreachable in practice (the top-of-handler intercept fires
            // first) — kept as a safety arm with the same graceful path.
            if let Some(h) = &model.interrupt {
                h.cancel_turn();
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            model.should_quit = true;
        }
        // Shift/Alt+Enter or Ctrl+J for newline (PART 2)
        KeyCode::Enter if key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
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
                spawn_turn(model, kernel, session, transcript, budget, tx, line);
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
                let idx = model.history_idx.map(|i| i.saturating_sub(1)).unwrap_or(model.history.len() - 1);
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

/// Handle agent events. `session` is swapped when a `/resume` completes.
pub(super) fn handle_agent_event(model: &mut Model, ev: TuiEvent, session: &mut Session, transcript: &mut Vec<Message>) {
    match ev {
        TuiEvent::ToolStarted(tool, target) => model.current_tool = Some((tool, target)),
        TuiEvent::Text(delta) => { model.current_tool = None; model.push_text_delta(&delta); }
        TuiEvent::Reasoning(delta) => model.push_thinking_delta(&delta),
        TuiEvent::ToolCall(tool, args) => { model.current_tool = None; model.push_item(Item::ToolCall { tool, args }); }
        TuiEvent::ToolResult(tool, ok, payload) => { model.current_tool = None; model.push_item(Item::ToolResult { tool, ok, payload }); }
        TuiEvent::Compaction(before, after, summarized, summary) => {
            model.compacting = false;
            model.push_item(Item::Compaction { before, after, summarized, summary });
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
                model.pending_approvals.push_back(PendingApproval { action, detail, escalated, responder });
                if was_empty {
                    model.approval_sel = 0;
                    model.approval_ready = false;
                    model.dirty = true;
                    model.scroll_to_bottom();
                }
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
        TuiEvent::Resumed(id, msgs) => {
            session.id = id;
            // Preserve transcript[0] (the system prompt); replace the rest.
            let system = transcript.first().cloned().unwrap_or_else(|| Message::system(""));
            transcript.clear();
            transcript.push(system);
            transcript.extend(msgs.clone());
            repaint_history(model, &msgs);
            model.reasoning_received_this_turn = false;
            model.last_turn_reasoning_received = None;
            model.push_notice(format!("(resumed session {id})"));
        }
        // `/rewind` cut points arrived from the log — open the rewind picker.
        TuiEvent::RewindPointsLoaded(points) => {
            if points.is_empty() {
                model.push_notice("(nothing to rewind to — no earlier turns in this session)");
            } else {
                model.picker = Some(Picker::new(PickerKind::Rewind(points)));
            }
        }
        // A rewind finished. For conversation scopes `new_id` is the forked
        // branch: swap to it, rebuild the transcript (keeping the system message
        // at [0]), and drop the chosen prompt back into the input box to edit or
        // re-send (edit-and-resubmit). The original session stays in the log
        // (non-destructive). Code-only (`new_id == None`) leaves the conversation
        // untouched and just reports the files reverted.
        TuiEvent::Rewound { new_id, msgs, rolled, scope, prefill } => {
            // "tracked" is honest (K18): only snapshot-carrying writes revert —
            // files mutated via shell (`sed -i`, `git checkout`) are not rolled back.
            let files = |n: usize| {
                if n == 1 { "1 tracked file".to_string() } else { format!("{n} tracked files") }
            };
            if let Some(id) = new_id {
                session.id = id;
                let system = transcript.first().cloned().unwrap_or_else(|| Message::system(""));
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
                    "(rewound · code kept · prompt ready to edit · new branch, original kept)".to_string()
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
/// and each of that turn's tool calls → an `Item::ToolCall` card so the resumed
/// history shows *what the agent did*, not just what it said. Raw tool results
/// are skipped (verbose JSON; the model still has them in the transcript).
pub(super) fn repaint_history(model: &mut Model, msgs: &[Message]) {
    model.items.clear();
    for m in msgs {
        match m.role {
            kernel::Role::User => model.push_item(Item::User(m.content.clone())),
            kernel::Role::Assistant => {
                if !m.content.trim().is_empty() {
                    model.push_item(Item::Assistant(m.content.clone()));
                }
                for tc in &m.tool_calls {
                    model.push_item(Item::ToolCall { tool: tc.tool.clone(), args: tc.args.clone() });
                }
            }
            // system/tool messages are not shown as transcript rows.
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
    SkillPicker,
    LoadSkill(String),
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
        "skill" => SlashAction::SkillPicker,
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
    P: Provider + 'static,
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
        SlashAction::SkillPicker => open_skill_picker(model),
        SlashAction::LoadSkill(name) => load_skill_by_name(model, &name, transcript),
        SlashAction::Other => run_slash(model, cmd, transcript, kernel.provider.as_ref()),
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
                RewindPoint { at_event: e.id, label, files }
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
        let Some(idx) = kernel::cut_index(&events, at_event) else { return };

        // Code rollback: revert every file written from this prompt onward,
        // returning the workspace to its state before the turn ran.
        let mut rolled = 0usize;
        if scope.touches_code() {
            for fr in kernel::rollback_plan(&events, at_event) {
                if restore.restore(&fr.path, fr.snapshot.as_deref()).await.is_ok() {
                    rolled += 1;
                }
            }
        }

        // Conversation rewind: fork before the prompt (non-destructive), replay
        // the kept history, and prefill the prompt for editing/re-sending.
        let (new_id, msgs, prefill) = if scope.touches_conversation() {
            let new_id = match log.fork(session_id, at_event).await {
                Ok(id) => id,
                Err(_) => return,
            };
            let prefill = events
                .get(idx)
                .and_then(|e| e.payload.get("text"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            (Some(new_id), kernel::project_messages(&events[..idx]), prefill)
        } else {
            (None, Vec::new(), None)
        };
        let _ = tx.send(TuiEvent::Rewound { new_id, msgs, rolled, scope, prefill });
    });
}

/// Spawn the async session-list fetch for `/resume`. Reads `log.sessions()`
/// off the main loop and sends `SessionsLoaded` back through the channel.
fn spawn_sessions_fetch<L: EventLog + 'static>(kernel: &Arc<Kernel<impl Provider + 'static, L>>, tx: &mpsc::UnboundedSender<TuiEvent>) {
    let log = kernel.log.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let sessions = log.sessions().await;
        let _ = tx.send(TuiEvent::SessionsLoaded(sessions));
    });
}

/// Spawn agent turn as background task
pub(super) fn spawn_turn<P, L>(
    model: &mut Model,
    kernel: &Arc<Kernel<P, L>>,
    session: &Session,
    transcript: &mut Vec<Message>,
    budget: &Budget,
    tx: &mpsc::UnboundedSender<TuiEvent>,
    line: String,
) where
    P: Provider + 'static,
    L: EventLog + 'static,
{
    model.welcome = false;
    model.push_item(Item::User(line.clone()));
    model.auto_scroll = true;
    model.running = true;
    model.reasoning_received_this_turn = false;
    model.turn_started = Some(Instant::now());
    // Pick up any skill saved/edited since startup so the model's manifest is
    // current this turn (not just next session).
    model.refresh_skill_manifest(transcript);
    transcript.push(Message::user(line));

    // Graceful interruption: the kernel owns cancellation now. Esc trips the
    // handle; run_session ALWAYS returns (settled history + StopReason), so
    // there is no select! race dropping the session future mid-tool anymore.
    let (handle, queue) = kernel::InterruptQueue::pair();
    model.interrupt = Some(handle);
    model.cancelling = false;

    let kernel = kernel.clone();
    let session = session.clone();
    let messages = transcript.clone();
    let budget = budget.clone();
    let tx = tx.clone();

    tokio::spawn(async move {
        let sink = TuiSink { tx: tx.clone() };
        match kernel.run_session(&session, messages, budget, &sink, Some(queue)).await {
            Ok((updated, reason)) => { let _ = tx.send(TuiEvent::Done(updated, reason)); }
            Err(e) => { let _ = tx.send(TuiEvent::Error(e.to_string())); }
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
    fn tool_started(&self, tool: &str, target: Option<&str>) { self.emit("tool_started", TuiEvent::ToolStarted(tool.to_string(), target.map(str::to_string))); }
    fn text(&self, delta: &str) { self.emit("text", TuiEvent::Text(delta.to_string())); }
    fn reasoning(&self, delta: &str) { self.emit("reasoning", TuiEvent::Reasoning(delta.to_string())); }
    fn tool_call(&self, tool: &str, args: &serde_json::Value) { self.emit("tool_call", TuiEvent::ToolCall(tool.to_string(), args.clone())); }
    fn tool_result(&self, tool: &str, ok: bool, payload: &serde_json::Value) { self.emit("tool_result", TuiEvent::ToolResult(tool.to_string(), ok, payload.clone())); }
    fn compacting(&self, active: bool) { self.emit("compacting", TuiEvent::Compacting(active)); }
    fn compaction(&self, before: u32, after: u32, summarized: bool, summary: Option<&str>) { self.emit("compaction", TuiEvent::Compaction(before, after, summarized, summary.map(str::to_string))); }
    fn usage(&self, prompt_tokens: u32, total_tokens: u32) { self.emit("usage", TuiEvent::Usage(prompt_tokens, total_tokens)); }
    fn cost(&self, total_usd: f64, indicative: bool) { self.emit("cost", TuiEvent::Cost(total_usd, indicative)); }
    fn verify(&self, ok: bool, summary: &str) { self.emit("verify", TuiEvent::Verify(ok, summary.to_string())); }
    fn steered(&self, text: &str) { self.emit("steered", TuiEvent::Steered(text.to_string())); }
    fn steers_returned(&self, texts: &[String]) { self.emit("steers_returned", TuiEvent::SteersReturned(texts.to_vec())); }
}

/// Run slash command
/// True when a stripped command prefix ends at a word boundary — i.e. the rest
/// is empty or starts with whitespace (an argument). Distinguishes `/think` and
/// `/think high` from `/thinking`.
fn is_cmd_boundary(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with(char::is_whitespace)
}

/// The unified reasoning panel has three editable rows. The fourth row reports
/// delivery for the previous turn and is deliberately not selectable.
fn handle_reasoning_picker_key<P: kernel::Provider>(
    model: &mut Model,
    key: KeyEvent,
    provider: &P,
) -> bool {
    let selected = match model.picker.as_ref() {
        Some(Picker { kind: PickerKind::Reasoning(_), selected }) => *selected,
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
                    provider.set_reasoning(kernel::ReasoningConfig {
                        enabled: Some(enabled),
                        effort: if enabled { cfg.effort } else { None },
                    });
                    model.reasoning = provider.reasoning();
                }
                1 => {
                    model.show_thinking = !model.show_thinking;
                    model.invalidate_all_renders();
                }
                2 => {
                    let cfg = provider.reasoning();
                    let effort = match cfg.effort {
                        None => Some(kernel::ReasoningEffort::Low),
                        Some(kernel::ReasoningEffort::Low) => Some(kernel::ReasoningEffort::Medium),
                        Some(kernel::ReasoningEffort::Medium) => Some(kernel::ReasoningEffort::High),
                        Some(kernel::ReasoningEffort::High) => None,
                    };
                    provider.set_reasoning(kernel::ReasoningConfig {
                        enabled: Some(true),
                        effort,
                    });
                    model.reasoning = provider.reasoning();
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
    let system = transcript.first().cloned().unwrap_or_else(|| Message::system(""));
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
    let list: Vec<(String, String)> = disc
        .effective()
        .map(|l| (l.skill.name.clone(), l.skill.description.clone()))
        .collect();
    if list.is_empty() {
        model.push_notice("no skills installed — add one under ~/.medha/skills/<name>/SKILL.md");
        return;
    }
    model.picker = Some(Picker::new(PickerKind::Skill(list)));
}

/// Force-load a skill's full procedure into the conversation, deterministically
/// (no reliance on the model choosing to call `skill.load`). This is the
/// model-independent trigger Hermes exposes via `/skill-name`: the procedure
/// lands in the transcript, so the next turn the model *has* it. Empty name →
/// open the picker instead. Reached from `/skill <name>` and the skill picker.
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

pub(super) fn run_slash<P: kernel::Provider>(model: &mut Model, cmd: &str, transcript: &[Message], provider: &P) {
    if let Some(rest) = cmd.strip_prefix("reasoning").filter(|r| is_cmd_boundary(r)) {
        apply_reasoning_command(model, provider, rest.trim());
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
            model.push_notice(if model.full_transparency { "detail: full tool input/output" } else { "detail: summarized" });
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
            let mut text = COMMANDS.iter().map(|(c, d)| format!("{c}  {d}")).collect::<Vec<_>>().join("\n");
            text.push_str("\n\nshortcuts:\n\n  Esc     interrupt a running turn\n  Ctrl-D  quit\n  ↑/↓     scroll (empty input) · history (while typing)");
            model.push_notice(text);
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
                "model: {}  |  {ctx}  |  ~{toks} est. tokens\n\n{}",
                model.model,
                model.reasoning_status_block()
            ));
        }
        other => model.push_notice(format!("unknown command: /{other}")),
    }
}

fn open_reasoning_panel(model: &mut Model) {
    let state = ReasoningPanelState::from_model(model);
    model.picker = Some(Picker::new(PickerKind::Reasoning(state)));
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
            provider.set_reasoning(kernel::ReasoningConfig { enabled: Some(true), effort });
            model.reasoning = provider.reasoning();
            Ok(())
        }
        "off" => {
            provider.set_reasoning(kernel::ReasoningConfig { enabled: Some(false), effort: None });
            model.reasoning = provider.reasoning();
            Ok(())
        }
        "show" | "hide" => {
            model.show_thinking = args == "show";
            model.invalidate_all_renders();
            Ok(())
        }
        "effort auto" => {
            let enabled = provider.reasoning().enabled;
            provider.set_reasoning(kernel::ReasoningConfig { enabled, effort: None });
            model.reasoning = provider.reasoning();
            Ok(())
        }
        "effort low" | "effort medium" | "effort high" => {
            let effort = match args.strip_prefix("effort ").unwrap_or("") {
                "low" => kernel::ReasoningEffort::Low,
                "medium" => kernel::ReasoningEffort::Medium,
                _ => kernel::ReasoningEffort::High,
            };
            provider.set_reasoning(kernel::ReasoningConfig {
                enabled: Some(true),
                effort: Some(effort),
            });
            model.reasoning = provider.reasoning();
            Ok(())
        }
        _ => Err("usage: /reasoning [on|off|show|hide|status|effort auto|low|medium|high]"),
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
        Model::new("m".into(), None, kernel::ReasoningConfig::default(),
                   lockfile::UiConfig::default(), HashMap::new(), sbx)
    }

    // ── slash ROUTING (the bug that shipped: `/skill` reached only one of the
    //    two Enter paths and fell through to "unknown command"). Both paths now
    //    route through classify_slash; this pins that routing. ──────────────────
    #[test]
    fn classify_slash_routes_skill_commands() {
        assert_eq!(classify_slash("skill"), SlashAction::SkillPicker);
        assert_eq!(
            classify_slash("skill frontend-ui-design"),
            SlashAction::LoadSkill("frontend-ui-design".into())
        );
        // `/skills` (list) must NOT be mistaken for `/skill` (load).
        assert_eq!(classify_slash("skills"), SlashAction::Other);
        assert_eq!(classify_slash("resume"), SlashAction::Resume);
        assert_eq!(classify_slash("clear"), SlashAction::Clear);
        assert_eq!(classify_slash("help"), SlashAction::Other);
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
        assert!(injected.contains("say hello"), "procedure body must be injected");
        assert!(injected.contains("Loaded skill: greet"));

        // Unknown skill → no injection, a clear notice instead.
        let n = transcript.len();
        load_skill_by_name(&mut m, "nope", &mut transcript);
        assert_eq!(transcript.len(), n, "unknown skill must not inject anything");

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
        assert!(is_cmd_boundary(""));            // /think
        assert!(is_cmd_boundary(" high"));       // /think high
        assert!(!is_cmd_boundary("ing"));        // /thinking → must fall through
    }

    #[test]
    fn unified_reasoning_command_controls_every_setting() {
        let provider = providers::OpenAiCompat::new("http://localhost/v1", "", "m");
        let mut m = model();
        let transcript = vec![Message::system("S")];

        run_slash(&mut m, "reasoning on", &transcript, &provider);
        assert_eq!(m.reasoning.enabled, Some(true));

        run_slash(&mut m, "reasoning show", &transcript, &provider);
        assert!(m.show_thinking);

        run_slash(&mut m, "reasoning effort high", &transcript, &provider);
        assert_eq!(m.reasoning.effort, Some(kernel::ReasoningEffort::High));

        run_slash(&mut m, "reasoning effort auto", &transcript, &provider);
        assert_eq!(m.reasoning.effort, None);

        run_slash(&mut m, "reasoning", &transcript, &provider);
        assert!(matches!(
            &m.picker,
            Some(Picker { kind: PickerKind::Reasoning(_), .. })
        ));
        let labels = m.picker.as_ref().unwrap().kind.labels();
        assert_eq!(labels[0], "Mode:       On");
        assert_eq!(labels[1], "Visibility: Shown");
        assert_eq!(labels[2], "Effort:     Auto");
        assert_eq!(labels[3], "Last turn:  No completed turn yet");
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
            !m.items.iter().any(|e| matches!(&e.item, Item::User(t) if t == "hello")),
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
        assert_eq!(transcript.len(), 2, "transcript untouched while a turn runs");
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
        assert!(matches!(rx.await, Ok(kernel::Approval::Deny)), "dangling responder got Deny");
    }
}
