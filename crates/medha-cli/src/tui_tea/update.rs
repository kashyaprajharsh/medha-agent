//! Update/input/async logic: the TEA update fn, key handling, and the
//! StreamSink→TuiEvent bridge. Split out of the old monolithic tui_tea.rs.
#![allow(clippy::too_many_arguments)]
use super::*;

pub(super) fn update<P, L>(model: &mut Model, msg: Msg, kernel: &Arc<Kernel<P, L>>, session: &Session, transcript: &mut Vec<Message>, budget: &Budget, tx: &mpsc::UnboundedSender<TuiEvent>) 
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
        Msg::AgentEvent(ev) => handle_agent_event(model, ev, transcript),
        Msg::Tick => {
            model.anim_frame = model.anim_frame.wrapping_add(1);
            if let Some(f) = model.intro_frame {
                model.intro_frame = if f >= 40 { None } else { Some(f + 1) };
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
    session: &Session,
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

    // Esc is reserved for stopping a running turn (cancel token), not for
    // answering an approval. Intercept it before the approval branch so Esc
    // always does what the user expects — interrupt — instead of silently
    // denying whichever prompt happens to be on screen (which the kernel would
    // then read as "rejected by human").
    if key.code == KeyCode::Esc && model.running {
        if let Some(token) = model.cancel_token.take() {
            token.cancel();
        }
        return;
    }

    // Inline approval handling (PART 3) — input captured when last item is approval
    if model.pending_approval().is_some() {
        handle_approval_key(model, key);
        return;
    }

    // Picker handling
    if let Some(picker) = model.picker.as_mut() {
        let opts = picker.kind.options();
        match key.code {
            KeyCode::Up => picker.selected = picker.selected.checked_sub(1).unwrap_or(opts.len() - 1),
            KeyCode::Down => picker.selected = (picker.selected + 1) % opts.len(),
            KeyCode::Enter => {
                let choice = opts[picker.selected];
                let msg = picker.kind.apply(kernel.provider.as_ref(), choice);
                model.reasoning = kernel.provider.reasoning();
                model.picker = None;
                model.push_notice(msg);
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
                    run_slash(model, &cmd, transcript, kernel.provider.as_ref());
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
        KeyCode::Esc if model.running => {
            if let Some(token) = model.cancel_token.take() {
                token.cancel();
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
            // Slash commands
            if model.input.starts_with('/') {
                let line = std::mem::take(&mut model.input);
                model.cursor = 0;
                model.ac_sel = 0;
                model.history.push(line.clone());
                model.history_idx = None;
                run_slash(model, line.trim_start_matches('/').trim(), transcript, kernel.provider.as_ref());
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
                let preview: String = raw.chars().take(60).collect();
                model.queued.push(line);
                model.push_notice(format!("⏳ queued (sends after current turn): {preview}"));
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
            // permission layer persists it to medha.lock (PART 1).
            if choice == 1 {
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

/// Handle agent events
pub(super) fn handle_agent_event(model: &mut Model, ev: TuiEvent, transcript: &mut Vec<Message>) {
    match ev {
        TuiEvent::ToolStarted(tool, target) => model.current_tool = Some((tool, target)),
        TuiEvent::Text(delta) => { model.current_tool = None; model.push_text_delta(&delta); }
        TuiEvent::Reasoning(delta) => model.push_thinking_delta(&delta),
        TuiEvent::ToolCall(tool, args) => { model.current_tool = None; model.push_item(Item::ToolCall { tool, args }); }
        TuiEvent::ToolResult(tool, ok, payload) => { model.current_tool = None; model.push_item(Item::ToolResult { tool, ok, payload }); }
        TuiEvent::Compaction(before, after, summarized) => model.push_item(Item::Compaction { before, after, summarized }),
        TuiEvent::Usage(prompt_tokens, _total) => {
            if let Some(mc) = model.max_ctx {
                let usable = context::ContextBudget::from_max_ctx(mc).usable().max(1);
                model.ctx_pct = Some((prompt_tokens as f32 / usable as f32 * 100.0).round() as u32);
            }
        }
        TuiEvent::Verify(ok, summary) => model.push_item(Item::Verify { ok, summary }),
        TuiEvent::Approval(action, detail, responder) => {
            if model.auto_approve.contains(&action) {
                let _ = responder.send(kernel::Approval::Once);
            } else {
                tracing::debug!(action = %action, "approval created");
                // Queue, don't clobber: the kernel runs tool calls concurrently
                // (buffered up to `max_parallel_tools`), so several `confirm()`
                // requests can arrive in the same turn. Replacing a pending one
                // would drop its `oneshot::Sender` and the kernel would read that
                // as `Approval::Deny` (the spurious "rejected by human").
                let was_empty = model.pending_approvals.is_empty();
                model.pending_approvals.push_back(PendingApproval { action, detail, responder });
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
            model.running = false;
            model.current_tool = None;
            model.turn_started = None;
            model.cancel_token = None;
            if let StopReason::Budget(stop) = reason {
                model.push_notice(format!("(stopped: {} reached)", stop.label()));
            }
        }
        TuiEvent::Error(e) => {
            model.push_notice(format!("error: {e}"));
            model.running = false;
            model.current_tool = None;
            model.turn_started = None;
            model.cancel_token = None;
        }
        TuiEvent::Interrupted => {
            model.push_notice("⏹ stopped (Esc)");
            model.running = false;
            model.current_tool = None;
            model.turn_started = None;
            model.cancel_token = None;
        }
    }
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
    model.turn_started = Some(Instant::now());
    transcript.push(Message::user(line));

    let cancel_token = CancellationToken::new();
    model.cancel_token = Some(cancel_token.clone());
    
    let kernel = kernel.clone();
    let session = session.clone();
    let messages = transcript.clone();
    let budget = budget.clone();
    let tx = tx.clone();
    
    tokio::spawn(async move {
        let sink = TuiSink { tx: tx.clone() };
        tokio::select! {
            result = kernel.run_session(&session, messages, budget, &sink) => {
                match result {
                    Ok((updated, reason)) => { let _ = tx.send(TuiEvent::Done(updated, reason)); }
                    Err(e) => { let _ = tx.send(TuiEvent::Error(e.to_string())); }
                }
            }
            _ = cancel_token.cancelled() => {
                let _ = tx.send(TuiEvent::Interrupted);
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
    fn tool_started(&self, tool: &str, target: Option<&str>) { self.emit("tool_started", TuiEvent::ToolStarted(tool.to_string(), target.map(str::to_string))); }
    fn text(&self, delta: &str) { self.emit("text", TuiEvent::Text(delta.to_string())); }
    fn reasoning(&self, delta: &str) { self.emit("reasoning", TuiEvent::Reasoning(delta.to_string())); }
    fn tool_call(&self, tool: &str, args: &serde_json::Value) { self.emit("tool_call", TuiEvent::ToolCall(tool.to_string(), args.clone())); }
    fn tool_result(&self, tool: &str, ok: bool, payload: &serde_json::Value) { self.emit("tool_result", TuiEvent::ToolResult(tool.to_string(), ok, payload.clone())); }
    fn compaction(&self, before: u32, after: u32, summarized: bool) { self.emit("compaction", TuiEvent::Compaction(before, after, summarized)); }
    fn usage(&self, prompt_tokens: u32, total_tokens: u32) { self.emit("usage", TuiEvent::Usage(prompt_tokens, total_tokens)); }
    fn verify(&self, ok: bool, summary: &str) { self.emit("verify", TuiEvent::Verify(ok, summary.to_string())); }
}

/// Run slash command
pub(super) fn run_slash<P: kernel::Provider>(model: &mut Model, cmd: &str, transcript: &[Message], provider: &P) {
    if let Some(rest) = cmd.strip_prefix("think") {
        let rest = rest.trim();
        if rest.is_empty() {
            model.picker = Some(Picker::new(PickerKind::Think));
        } else {
            model.push_notice(crate::apply_think_command(provider, rest));
            model.reasoning = provider.reasoning();
        }
        return;
    }
    if let Some(rest) = cmd.strip_prefix("effort") {
        let rest = rest.trim();
        if rest.is_empty() {
            model.picker = Some(Picker::new(PickerKind::Effort));
        } else {
            model.push_notice(crate::apply_effort_command(provider, rest));
            model.reasoning = provider.reasoning();
        }
        return;
    }
    match cmd {
        "exit" | "quit" => model.should_quit = true,
        "thinking" => {
            model.show_thinking = !model.show_thinking;
            model.invalidate_all_renders();
            model.push_notice(if model.show_thinking { "reasoning: shown" } else { "reasoning: hidden" });
        }
        "detail" => {
            model.full_transparency = !model.full_transparency;
            model.invalidate_all_renders();
            model.push_notice(if model.full_transparency { "detail: full tool input/output" } else { "detail: summarized" });
        }
        "help" => {
            let mut text = COMMANDS.iter().map(|(c, d)| format!("{c}  {d}")).collect::<Vec<_>>().join("\n");
            text.push_str("\n\nshortcuts:\n\n  Esc     interrupt a running turn\n  Ctrl-D  quit\n  ↑/↓     scroll (empty input) · history (while typing)");
            model.push_notice(text);
        }
        "clear" => {
            model.items.clear();
            model.push_notice("(conversation cleared)");
        }
        "status" => {
            let toks: usize = transcript.iter().map(|m| m.content.len() / 4).sum();
            let ctx = match model.max_ctx {
                Some(mc) => format!("{mc} window"),
                None => "unknown window".to_string(),
            };
            let think = crate::apply_think_command(provider, "status");
            model.push_notice(format!("model: {}  |  {ctx}  |  ~{toks} est. tokens  |  {think}", model.model));
        }
        other => model.push_notice(format!("unknown command: /{other}")),
    }
}


