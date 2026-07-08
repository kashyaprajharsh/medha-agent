//! TEA-based TUI using ratatui::run — The Elm Architecture implementation
//!
//! Model (app state) → Update(model, message) → model → View(model) → frame
//! View is a pure function of model — same state always renders identically.
//! Message-passing, not shared mutable state.

use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use futures::StreamExt;
use kernel::{Budget, EventLog, Kernel, Message, Provider, Session, StopReason, ToolCategory};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Maximum lines to keep in scrollback buffer
const MAX_SCROLLBACK_LINES: usize = 5000;
/// Maximum diff lines to display inline
const MAX_DIFF_LINES: usize = 60;
/// Maximum lines of raw tool I/O rendered inline per tool call (PART 4)
const MAX_TOOL_OUTPUT_LINES: usize = 500;
/// Pastes longer than this are collapsed to a placeholder in the input box (PART 2)
const PASTE_COLLAPSE_THRESHOLD: usize = 1000;
/// Redraw interval (16-33ms for 60-30fps)
const REDRAW_INTERVAL: Duration = Duration::from_millis(16);

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show commands"),
    ("/status", "model, context window, current pressure"),
    ("/think", "enable/disable reasoning: on|off|status"),
    ("/effort", "set reasoning depth (bare = arrow-key picker)"),
    ("/thinking", "show/hide the model's live reasoning"),
    ("/detail", "expand/collapse full tool input & output"),
    ("/clear", "reset the conversation"),
    ("/exit", "quit (also Ctrl-D)"),
];

fn command_matches(input: &str) -> Vec<(&'static str, &'static str)> {
    COMMANDS.iter().filter(|(c, _)| c.starts_with(input)).copied().collect()
}

/// Channel shared by sink (agent → UI events) and human gate (agent → UI approval requests)
pub(crate) fn channel() -> (mpsc::UnboundedSender<TuiEvent>, mpsc::UnboundedReceiver<TuiEvent>) {
    mpsc::unbounded_channel()
}

/// Human gate for TUI: approval request sent as TuiEvent with oneshot responder
pub(crate) struct TuiGate {
    pub(crate) tx: mpsc::UnboundedSender<TuiEvent>,
}

#[async_trait::async_trait]
impl kernel::HumanGate for TuiGate {
    async fn confirm(&self, action: &str, detail: Option<&str>) -> kernel::Approval {
        let (resp_tx, resp_rx) = oneshot::channel();
        let req = TuiEvent::Approval(action.to_string(), detail.map(str::to_string), resp_tx);
        if self.tx.send(req).is_err() {
            return kernel::Approval::Deny;
        }
        resp_rx.await.unwrap_or(kernel::Approval::Deny)
    }
}

/// Events from agent to UI
#[derive(Debug)]
pub(crate) enum TuiEvent {
    Text(String),
    Reasoning(String),
    ToolStarted(String, Option<String>),
    ToolCall(String, serde_json::Value),
    ToolResult(String, bool, serde_json::Value),
    Compaction(u32, u32, bool),
    Usage(u32, u32),
    Verify(bool, String),
    Approval(String, Option<String>, oneshot::Sender<kernel::Approval>),
    Done(Vec<Message>, StopReason),
    Error(String),
    Interrupted,
}

/// One rendered line-group in the transcript
#[derive(Debug)]
enum Item {
    User(String),
    Assistant(String),
    ToolCall { tool: String, args: serde_json::Value },
    ToolResult { tool: String, ok: bool, payload: serde_json::Value },
    Compaction { before: u32, after: u32, summarized: bool },
    Verify { ok: bool, summary: String },
    Notice(String),
    Thinking(String),
}

/// A transcript item plus its memoized render. `render_item` (which computes
/// diffs, parses markdown, wraps lines) runs ONCE per item and is reused every
/// frame; only the item that actually changes is re-rendered. This is what keeps
/// scrolling and post-tool streaming smooth — previously a 2000-line diff was
/// recomputed on every 16ms frame.
struct Entry {
    item: Item,
    lines: Option<Vec<Line<'static>>>,
    height: usize,
}

impl Entry {
    fn new(item: Item) -> Self {
        Self { item, lines: None, height: 0 }
    }
    fn invalidate(&mut self) {
        self.lines = None;
    }
    /// Render this item to PHYSICAL rows (each already wrapped to `width`, so one
    /// stored line = one screen row). Runs once; reused until invalidated. `height`
    /// is the exact row count — no separate wrap measurement, so scroll math and
    /// the rendered slice can never drift.
    fn ensure(&mut self, cx: &RenderCtx<'_>, width: u16) {
        if self.lines.is_none() {
            let mut rows: Vec<Line<'static>> = Vec::new();
            for logical in render_item(&self.item, cx) {
                rows.extend(wrap_line(&logical, width as usize));
            }
            self.height = rows.len();
            self.lines = Some(rows);
        }
    }
}

/// Word-wrap one logical line into physical rows of at most `width` columns,
/// preserving each span's style. Long words hard-break. This is the single source
/// of truth for layout — the renderer draws these rows directly (no ratatui wrap),
/// so measured height always equals rendered rows (essential for virtualization).
fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    // Flatten to (char, style); fast-path lines that already fit.
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        let st = span.style;
        for c in span.content.chars() {
            chars.push((c, st));
        }
    }
    if chars.len() <= width {
        return vec![line.clone()];
    }

    // Greedy word wrap: fill up to `width`, breaking at the last space when possible.
    let n = chars.len();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        let hard_end = (i + width).min(n);
        if hard_end < n {
            if let Some(sp) = (i..hard_end).rev().find(|&k| chars[k].0 == ' ') {
                if sp > i {
                    ranges.push((i, sp)); // drop the breaking space
                    i = sp + 1;
                    continue;
                }
            }
        }
        ranges.push((i, hard_end));
        i = hard_end;
    }

    // Rebuild each row, coalescing consecutive same-style chars back into spans.
    ranges
        .into_iter()
        .map(|(a, b)| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut buf = String::new();
            let mut cur: Option<Style> = None;
            for &(c, st) in &chars[a..b] {
                if cur != Some(st) {
                    if let Some(ps) = cur {
                        spans.push(Span::styled(std::mem::take(&mut buf), ps));
                    }
                    cur = Some(st);
                }
                buf.push(c);
            }
            if let Some(ps) = cur {
                spans.push(Span::styled(buf, ps));
            }
            Line::from(spans)
        })
        .collect()
}

/// Pending approval for inline rendering
struct PendingApproval {
    action: String,
    detail: Option<String>,
    responder: oneshot::Sender<kernel::Approval>,
}

/// Reasoning control picker kind
#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Think,
    Effort,
}

impl PickerKind {
    fn title(&self) -> &'static str {
        match self {
            PickerKind::Think => " thinking — ↑↓ select, Enter apply, Esc cancel ",
            PickerKind::Effort => " effort — ↑↓ select, Enter apply, Esc cancel ",
        }
    }
    fn options(&self) -> &'static [&'static str] {
        match self {
            PickerKind::Think => &["on", "off"],
            PickerKind::Effort => &["low", "medium", "high"],
        }
    }
    fn apply<P: kernel::Provider>(&self, provider: &P, choice: &str) -> String {
        match self {
            PickerKind::Think => crate::apply_think_command(provider, choice),
            PickerKind::Effort => crate::apply_effort_command(provider, choice),
        }
    }
}

/// Arrow-key picker state
struct Picker {
    kind: PickerKind,
    selected: usize,
}

impl Picker {
    fn new(kind: PickerKind) -> Self {
        Self { kind, selected: 0 }
    }
}

/// TEA Model — all application state
struct Model {
    /// Transcript items (capped, with overflow to session log)
    items: VecDeque<Entry>,
    /// Tool name → its declared presentation (glyph + category), from the
    /// executor's specs. The surface renders each tool's own glyph — no
    /// name→glyph table here.
    tool_viz: HashMap<String, ToolViz>,
    /// Input buffer
    input: String,
    /// Cursor position in input as a BYTE offset (always on a UTF-8 char boundary)
    cursor: usize,
    /// Command history
    history: Vec<String>,
    history_idx: Option<usize>,
    /// Scroll offset (0 = top)
    scroll_offset: usize,
    /// Whether auto-scrolling to bottom
    auto_scroll: bool,
    /// Viewport height (rows)
    viewport_height: usize,
    /// Content height (rows)
    content_height: usize,
    /// Item heights/total need recomputing (content changed). Rendering is
    /// virtualized either way — only the visible window is built each frame.
    dirty: bool,
    /// Total physical rows across all items (+ approval), excluding the spinner.
    /// Drives scroll math; recomputed only when `dirty`.
    total_rows: usize,
    /// Pre-wrapped physical rows of the pending approval card (not per-item cached).
    approval_rows: Vec<Line<'static>>,
    /// Terminal width the caches were laid out for (re-wrap on resize).
    cached_width: u16,
    /// The pending approval card has been rendered at least once, so its options
    /// are on screen and selection input is safe to accept (blocks blind-Enter).
    approval_ready: bool,
    /// Context percentage
    ctx_pct: Option<u32>,
    /// Model name
    model: String,
    /// Max context
    max_ctx: Option<u32>,
    /// Whether agent turn is running
    running: bool,
    /// Tool whose call is currently streaming: (name, optional target file/command).
    /// Drives the "writing medha.html…" activity label instead of a vague "thinking".
    current_tool: Option<(String, Option<String>)>,
    /// When the current turn started — for the elapsed-time counter in the status.
    turn_started: Option<Instant>,
    /// Queued messages (sent during agent turn)
    queued: Vec<String>,
    /// Cancel token for current turn
    cancel_token: Option<CancellationToken>,
    /// Pending inline approval queue (PART 3). The kernel may emit several
    /// tool calls concurrently (buffered up to `max_parallel_tools`), and each
    /// out-of-workspace path or Human-gated tool spawns a separate `confirm()`
    /// request. A single slot would clobber the prior request and drop its
    /// `oneshot::Sender` — which the kernel's `TuiGate::confirm` turns into a
    /// silent `Approval::Deny` (the "rejected by human" after a real approval).
    /// Queue them instead and advance as each is answered.
    pending_approvals: VecDeque<PendingApproval>,
    /// Selected approval option (0=Yes, 1=Yes-all, 2=No)
    approval_sel: usize,
    /// Auto-approved tool classes for session
    auto_approve: std::collections::HashSet<String>,
    /// Current reasoning config
    reasoning: kernel::ReasoningConfig,
    /// Active picker
    picker: Option<Picker>,
    /// Autocomplete selection
    ac_sel: usize,
    /// Show welcome splash
    welcome: bool,
    /// Show thinking/reasoning
    show_thinking: bool,
    /// Full transparency (expanded tool I/O)
    full_transparency: bool,
    /// Animation frame
    anim_frame: u64,
    /// Intro frame for veena animation
    intro_frame: Option<u64>,
    /// Quit flag
    should_quit: bool,
    /// Last redraw time for throttling
    last_redraw: Instant,
    /// Full content of large pastes, kept out of the rendered input box (PART 2).
    /// The input holds a compact placeholder token that indexes into this vec.
    pastes: Vec<String>,
}

impl Model {
    fn new(
        model: String,
        max_ctx: Option<u32>,
        reasoning: kernel::ReasoningConfig,
        ui: lockfile::UiConfig,
        tool_viz: HashMap<String, ToolViz>,
    ) -> Self {
        Self {
            items: VecDeque::with_capacity(MAX_SCROLLBACK_LINES),
            tool_viz,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            scroll_offset: 0,
            auto_scroll: true,
            viewport_height: 0,
            content_height: 0,
            dirty: true,
            total_rows: 0,
            approval_rows: Vec::new(),
            cached_width: 0,
            approval_ready: false,
            ctx_pct: None,
            model,
            max_ctx,
            running: false,
            current_tool: None,
            turn_started: None,
            queued: Vec::new(),
            cancel_token: None,
            pending_approvals: VecDeque::new(),
            approval_sel: 0,
            auto_approve: std::collections::HashSet::new(),
            reasoning,
            picker: None,
            ac_sel: 0,
            welcome: true,
            show_thinking: ui.show_thinking,
            full_transparency: ui.full_transparency,
            should_quit: false,
            anim_frame: 0,
            intro_frame: Some(0),
            last_redraw: Instant::now(),
            pastes: Vec::new(),
        }
    }

    /// The front of the approval queue (the one currently rendered/answerable),
    /// or `None` if no approval is pending. Callers that used to read a single
    /// `pending_approval` field go through here so the queue is transparent.
    fn pending_approval(&self) -> Option<&PendingApproval> {
        self.pending_approvals.front()
    }

    /// The declared category of a tool (from the executor specs), or `Other` if
    /// the surface hasn't been told about it.
    fn category(&self, tool: &str) -> ToolCategory {
        self.tool_viz.get(tool).map(|v| v.category).unwrap_or(ToolCategory::Other)
    }

    /// Expand paste placeholder tokens back into their full content before a line
    /// is submitted to the agent (PART 2). The compact token stays in history/input.
    fn resolve_pastes(&self, s: &str) -> String {
        expand_paste_tokens(&self.pastes, s)
    }

    fn max_scroll(&self) -> usize {
        self.content_height.saturating_sub(self.viewport_height)
    }

    fn scroll_by(&mut self, delta: i32) {
        let max = self.max_scroll();
        let next = (self.scroll_offset as i32).saturating_add(delta).clamp(0, max as i32) as usize;
        self.scroll_offset = next;
        self.auto_scroll = next >= max;
    }

    fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
        self.auto_scroll = self.max_scroll() == 0;
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = self.max_scroll();
        self.auto_scroll = true;
    }

    fn push_notice(&mut self, s: impl Into<String>) {
        self.push_item(Item::Notice(s.into()));
    }

    fn push_item(&mut self, item: Item) {
        self.items.push_back(Entry::new(item));
        // Cap scrollback
        while self.items.len() > MAX_SCROLLBACK_LINES {
            self.items.pop_front();
        }
        self.dirty = true;
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    fn push_text_delta(&mut self, delta: &str) {
        let appended = matches!(self.items.back().map(|e| &e.item), Some(Item::Assistant(_)));
        if appended {
            let e = self.items.back_mut().unwrap();
            if let Item::Assistant(buf) = &mut e.item {
                buf.push_str(delta);
            }
            e.invalidate(); // only the streaming item re-renders next frame
            self.dirty = true;
            if self.auto_scroll {
                self.scroll_to_bottom();
            }
        } else {
            self.push_item(Item::Assistant(delta.to_string()));
        }
    }

    fn push_thinking_delta(&mut self, delta: &str) {
        let appended = matches!(self.items.back().map(|e| &e.item), Some(Item::Thinking(_)));
        if appended {
            let e = self.items.back_mut().unwrap();
            if let Item::Thinking(buf) = &mut e.item {
                buf.push_str(delta);
            }
            e.invalidate();
            self.dirty = true;
            if self.auto_scroll {
                self.scroll_to_bottom();
            }
        } else {
            self.push_item(Item::Thinking(delta.to_string()));
        }
    }

    /// Invalidate every item's memoized render (width changed, or a display
    /// toggle like /detail or /thinking flipped how items render).
    fn invalidate_all_renders(&mut self) {
        for e in self.items.iter_mut() {
            e.invalidate();
        }
        self.dirty = true;
    }

    // ---- Input editing (byte-safe: `cursor` is a byte offset always on a UTF-8
    // char boundary, so multi-byte input like "café" or emoji never panics). ----

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn insert_text(&mut self, s: &str) {
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn backspace(&mut self) {
        if let Some(c) = self.input[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.input.remove(self.cursor);
        }
    }

    fn move_left(&mut self) {
        if let Some(c) = self.input[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    fn move_right(&mut self) {
        if let Some(c) = self.input[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }
}

/// TEA Messages — all events that can update the model
#[derive(Debug)]
enum Msg {
    // Input events
    KeyPress(KeyEvent),
    MouseScroll(i32),
    Paste(String),
    Resize(u16),
    // Agent events
    AgentEvent(TuiEvent),
    // Internal
    Tick,
}

/// Update function — pure state transition
fn update<P, L>(model: &mut Model, msg: Msg, kernel: &Arc<Kernel<P, L>>, session: &Session, transcript: &mut Vec<Message>, budget: &Budget, tx: &mpsc::UnboundedSender<TuiEvent>) 
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
fn strip_paste_markers(s: &str) -> String {
    s.replace("\u{1b}[200~", "").replace("\u{1b}[201~", "")
}

/// Replace `[paste #N: M chars]` placeholder tokens with the full content stored in
/// `pastes[N]` (PART 2). Non-token text is passed through untouched.
fn expand_paste_tokens(pastes: &[String], s: &str) -> String {
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
fn handle_paste(model: &mut Model, data: String) {
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
fn handle_key<P, L>(
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
fn handle_approval_key(model: &mut Model, key: KeyEvent) {
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
fn handle_agent_event(model: &mut Model, ev: TuiEvent, transcript: &mut Vec<Message>) {
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
fn spawn_turn<P, L>(
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
struct TuiSink {
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
fn run_slash<P: kernel::Provider>(model: &mut Model, cmd: &str, transcript: &[Message], provider: &P) {
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

// ===== Rendering (View) =====

mod theme {
    use ratatui::style::Color;
    pub const ACCENT: Color = Color::Rgb(230, 176, 84);
    pub const TEXT: Color = Color::Rgb(223, 226, 231);
    pub const DIM: Color = Color::Rgb(124, 132, 144);
    pub const FAINT: Color = Color::Rgb(78, 85, 96);
    pub const OK: Color = Color::Rgb(126, 200, 141);
    pub const ERR: Color = Color::Rgb(232, 122, 122);
    pub const WARN: Color = Color::Rgb(226, 188, 112);
    pub const LINENO: Color = Color::Rgb(86, 94, 108);
    pub const ADD_BG: Color = Color::Rgb(20, 46, 32);
    pub const DEL_BG: Color = Color::Rgb(52, 26, 30);
    pub const ADD_FG: Color = Color::Rgb(150, 214, 165);
    pub const DEL_FG: Color = Color::Rgb(232, 140, 140);
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
fn spinner_frame(frame: u64) -> &'static str { SPINNER[(frame as usize) % SPINNER.len()] }

/// Human-readable verb for a tool name (used both for the live activity label and
/// the in-progress tool-call line).
/// Live-activity verb for a tool's *category*.
fn cat_verb(cat: ToolCategory) -> &'static str {
    match cat {
        ToolCategory::Read => "reading",
        ToolCategory::Write => "writing",
        ToolCategory::Search => "searching files",
        ToolCategory::Web => "searching the web",
        ToolCategory::Shell => "running command",
        ToolCategory::Vcs => "inspecting git",
        ToolCategory::Diagnostic => "checking",
        ToolCategory::Plan => "planning",
        ToolCategory::Other => "working",
    }
}

/// Elapsed time since the current turn started, e.g. "8s" or "1m03s".
fn elapsed_str(model: &Model) -> String {
    match model.turn_started {
        Some(t) => {
            let s = t.elapsed().as_secs();
            if s >= 60 { format!("{}m{:02}s", s / 60, s % 60) } else { format!("{s}s") }
        }
        None => String::new(),
    }
}

/// Compact display of a tool target: a file's basename, or a clipped command.
fn short_target(t: &str) -> String {
    let base = t.rsplit(['/', '\\']).next().unwrap_or(t);
    let base = if base.is_empty() { t } else { base };
    if base.chars().count() > 32 {
        format!("{}…", base.chars().take(31).collect::<String>())
    } else {
        base.to_string()
    }
}

/// The live activity label, e.g. "writing medha.html", "reading", "thinking".
/// A streaming tool call wins so the user sees what's actually happening.
fn activity_label(model: &Model) -> String {
    if let Some((tool, target)) = &model.current_tool {
        let verb = cat_verb(model.category(tool));
        return match target {
            Some(t) => format!("{verb} {}", short_target(t)),
            None => verb.to_string(),
        };
    }
    // Between actions the model is producing its next output; we only *know* it's
    // "thinking" when reasoning is actually enabled/streaming. With reasoning off,
    // saying "thinking" is a lie — it's generating a reply or a tool call → "working".
    let between = if model.reasoning.enabled == Some(false) { "working" } else { "thinking" };
    match model.items.back().map(|e| &e.item) {
        Some(Item::ToolCall { tool, .. }) => cat_verb(model.category(tool)).to_string(),
        Some(Item::ToolResult { .. }) => between.to_string(),
        Some(Item::Assistant(_)) => "generating".to_string(),
        Some(Item::Thinking(_)) => "thinking".to_string(),
        _ => between.to_string(),
    }
}

/// A Saraswati veena — the instrument Medha/Saraswati holds: a large resonator
/// gourd (kudam) with a soundhole, a long fretted neck (dandi), a small upper
/// gourd (tumba) and a pegbox curl. Playing the veena means tuning the intellect
/// into harmony, so the animation is a *pluck*: a bright resonance comet sweeps
/// down the neck, pauses while the string settles, then re-plucks — looping
/// continuously (driven by the always-advancing `anim_frame`, not the one-shot
/// intro clock). The gourds glow near-white throughout — Saraswati's white, the
/// colour of purity and true-knowledge discrimination.
fn veena_line(frame: u64) -> Line<'static> {
    const FRETS: usize = 8;
    let mut glyphs: Vec<&'static str> = Vec::new();
    glyphs.extend(["◖", "◉", "◗"]); // kudam — large resonator + soundhole
    for _ in 0..FRETS {
        glyphs.extend(["━", "┿"]); // fretted neck (dandi)
    }
    glyphs.push("━");
    glyphs.push("○"); // tumba — small upper gourd
    glyphs.push("╮"); // pegbox curl

    let n = glyphs.len();
    // The resonance travels the neck, then a short gap lets the string settle
    // before the next pluck. `/3` slows the sweep to a graceful ~1.5s cadence.
    const GAP: usize = 8;
    let head = (frame / 3) as usize % (n + GAP);

    let white = Style::default().fg(Color::Rgb(255, 246, 214)).add_modifier(Modifier::BOLD);
    let bright_gold = Style::default().fg(Color::Rgb(247, 208, 120)).add_modifier(Modifier::BOLD);
    let gold = Style::default().fg(theme::ACCENT);
    let dim = Style::default().fg(Color::Rgb(150, 120, 70));
    let faint = Style::default().fg(theme::FAINT);

    let mut spans = Vec::with_capacity(n);
    for (i, g) in glyphs.iter().enumerate() {
        // Comet: brightest at the head, a short fading tail; nothing during the gap.
        let comet = match i.abs_diff(head) {
            0 => Some(white),
            1 => Some(bright_gold),
            2 => Some(gold),
            _ => None,
        };
        let style = match *g {
            "◉" | "○" => white,                // gourds always glow (purity)
            "◖" | "◗" => gold,                 // gourd rim
            "╮" => dim,                         // pegbox
            "┿" => comet.unwrap_or(faint),      // frets: faint, lift as resonance passes
            _ => comet.unwrap_or(dim),          // neck: warm gold, flares with the pluck
        };
        spans.push(Span::styled(*g, style));
    }
    Line::from(spans)
}

const LOGO: &str = r#"███╗   ███╗ ███████╗ ██████╗  ██╗  ██╗  █████╗
████╗ ████║ ██╔════╝ ██╔══██╗ ██║  ██║ ██╔══██╗
██╔████╔██║ █████╗   ██║  ██║ ███████║ ███████║
██║╚██╔╝██║ ██╔══╝   ██║  ██║ ██╔══██║ ██╔══██║
██║ ╚═╝ ██║ ███████╗ ██████╔╝ ██║  ██║ ██║  ██║
╚═╝     ╚═╝ ╚══════╝ ╚═════╝  ╚═╝  ╚═╝ ╚═╝  ╚═╝"#;

/// MEDHA's identity palette, grounded in Saraswati's iconography: **white**
/// (purity, true knowledge) crowning **gold/yellow** (intellect, the Vasant
/// spring colour). The wordmark is lit from the top — a near-white crown, warm
/// gold body, deep bronze base — so the six rows read as a solid form receding
/// into shadow, not flat text. All warm: no cool/blue tones.
const LOGO_GRADIENT: [(u8, u8, u8); 6] = [
    (255, 248, 224), (247, 208, 120), (230, 176, 84),
    (206, 150, 78), (176, 126, 66), (150, 108, 56),
];

/// Darken an rgb toward its shadow (num/den of full brightness). Used to bevel
/// the logo's box-drawing outline beneath the bright block fill.
fn shade(rgb: (u8, u8, u8), num: u16, den: u16) -> Color {
    let m = |c: u8| ((c as u16 * num) / den.max(1)) as u8;
    Color::Rgb(m(rgb.0), m(rgb.1), m(rgb.2))
}

/// Build one logo row: the solid `█` fill in the row's gold, the box-drawing
/// outline (╔╗╚╝║═ …) a few shades darker so each letter looks raised/engraved
/// rather than flat — a lightweight bevel using only per-glyph color.
fn logo_row(line: &str, rgb: (u8, u8, u8)) -> Vec<Span<'static>> {
    let fill = Style::default()
        .fg(Color::Rgb(rgb.0, rgb.1, rgb.2))
        .add_modifier(Modifier::BOLD);
    let edge = Style::default().fg(shade(rgb, 52, 100)).add_modifier(Modifier::BOLD);
    // A glyph is either solid fill (█, or a space — no visible ink) or an
    // outline edge; coalesce consecutive same-class glyphs into one span.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut buf_fill = true;
    for ch in line.chars() {
        let is_fill = ch == '█' || ch == ' ';
        if is_fill != buf_fill && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), if buf_fill { fill } else { edge }));
        }
        buf_fill = is_fill;
        buf.push(ch);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, if buf_fill { fill } else { edge }));
    }
    spans
}

fn center_line(spans: Vec<Span<'static>>, width: u16) -> Line<'static> {
    let content: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = (width as usize).saturating_sub(content) / 2;
    let mut out = Vec::with_capacity(spans.len() + 1);
    out.push(Span::raw(" ".repeat(pad)));
    out.extend(spans);
    Line::from(out)
}

fn lerp_color(a: (u8, u8, u8), b: (u8, u8, u8), num: i32, den: i32) -> Color {
    let mix = |x: u8, y: u8| (x as i32 + (y as i32 - x as i32) * num / den.max(1)) as u8;
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

fn draw_welcome(f: &mut Frame, model: &Model, area: Rect) {
    let w = area.width;
    let mut body: Vec<Line> = Vec::new();
    let t = (model.anim_frame % 60) as i32;
    let level = if t < 30 { t } else { 60 - t };
    // Devanagari wordmark breathes between deep gold and Saraswati's white —
    // knowledge-light pulsing over the intellect-gold (no cool/blue tones).
    let word = lerp_color((214, 158, 74), (255, 248, 224), level, 30);
    body.push(center_line(vec![Span::styled("◆  मेधा  ◆", Style::default().fg(word).add_modifier(Modifier::BOLD))], w));
    body.push(Line::from(""));
    for (i, line) in LOGO.lines().enumerate() {
        let rgb = LOGO_GRADIENT[i.min(LOGO_GRADIENT.len() - 1)];
        body.push(center_line(logo_row(line, rgb), w));
    }
    body.push(Line::from(""));
    body.push(center_line(vec![Span::styled("verification-first · open-first agent harness", Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC))], w));
    body.push(Line::from(""));
    let veena = veena_line(model.anim_frame);
    body.push(center_line(veena.spans, w));
    body.push(Line::from(""));
    body.push(center_line(vec![Span::styled("type below to begin · /help for commands · Ctrl-D to quit", Style::default().fg(theme::FAINT))], w));
    let top = (area.height as usize).saturating_sub(body.len()) / 2;
    let mut lines: Vec<Line> = (0..top).map(|_| Line::from("")).collect();
    lines.extend(body);
    f.render_widget(Paragraph::new(lines), area);
}

/// A tool's presentation, resolved once from its declared spec: its own glyph
/// plus the category that drives colour/verb. The surface holds no name→glyph
/// table — the glyph is the tool's, so each stays distinct.
#[derive(Clone)]
struct ToolViz {
    icon: String,
    category: ToolCategory,
}

/// Colour for a tool's *category* (glyph is the tool's own, from `ToolViz`).
fn cat_color(cat: ToolCategory) -> Color {
    let blue = Color::Rgb(120, 170, 235);
    let purple = Color::Rgb(186, 148, 236);
    let cyan = Color::Rgb(110, 196, 208);
    match cat {
        ToolCategory::Read => blue,
        ToolCategory::Write => theme::WARN,
        ToolCategory::Search => purple,
        ToolCategory::Web => cyan,
        ToolCategory::Shell => theme::ERR,
        ToolCategory::Vcs => Color::Rgb(226, 142, 90),
        ToolCategory::Diagnostic => theme::WARN,
        ToolCategory::Plan => theme::ACCENT,
        ToolCategory::Other => theme::DIM,
    }
}

/// Prettify a tool name for display without a per-tool table: take the last
/// dotted segment, turn `_` into spaces, capitalize. `fs.read`→"Read",
/// `code_outline`→"Code outline", `web.search`→"Search". Always reasonable for
/// any future tool, zero maintenance.
fn tool_label(tool: &str) -> String {
    let seg = tool.rsplit('.').next().unwrap_or(tool).replace('_', " ");
    let mut chars = seg.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => seg,
    }
}

fn render_plan(payload: &serde_json::Value) -> Vec<Line<'static>> {
    let steps = payload.get("steps").and_then(|v| v.as_array());
    let Some(steps) = steps else {
        return vec![Line::from(Span::styled("  ☰ plan updated", Style::default().fg(theme::DIM)))];
    };
    let total = steps.len();
    let is_done = |s: &&serde_json::Value| matches!(s.get("status").and_then(|v| v.as_str()), Some("completed" | "done"));
    let done = steps.iter().filter(is_done).count();
    // A tiny progress bar so completion is readable at a glance.
    let bar_w = 10usize;
    let filled = (done * bar_w).checked_div(total).unwrap_or(0);
    let bar: String = "█".repeat(filled).chars().chain("░".repeat(bar_w - filled).chars()).collect();
    let mut lines = vec![Line::from(vec![
        Span::styled("☰ ", Style::default().fg(theme::ACCENT)),
        Span::styled("Plan", Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {bar} {done}/{total}"), Style::default().fg(theme::DIM)),
    ])];
    // Optional one-line note about this update (Codex-style "explanation", inline).
    if let Some(exp) = payload.get("explanation").and_then(|v| v.as_str()) {
        if !exp.trim().is_empty() {
            lines.push(Line::from(Span::styled(format!("  {}", exp.trim()), Style::default().fg(theme::FAINT).add_modifier(Modifier::ITALIC))));
        }
    }
    for s in steps {
        let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let (mark, style) = match s.get("status").and_then(|v| v.as_str()) {
            Some("completed" | "done") => ("✔", Style::default().fg(theme::OK)),
            // Active step: accent bar + bold, and an arrow so "what's happening now"
            // is unmistakable even when the list scrolls by.
            Some("in_progress") => ("▶", Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)),
            _ => ("○", Style::default().fg(theme::TEXT)),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {mark} "), style),
            Span::styled(title.to_string(), style),
        ]));
    }
    lines
}

struct RenderCtx<'a> {
    width: u16,
    full_transparency: bool,
    show_thinking: bool,
    /// Tool name → its declared presentation, so rendering uses each tool's own
    /// glyph + category colour (static per session; borrowed).
    viz: &'a HashMap<String, ToolViz>,
}

fn render_item(item: &Item, cx: &RenderCtx<'_>) -> Vec<Line<'static>> {
    match item {
        Item::User(s) => {
            let mut lines = vec![Line::from("")];
            for (i, l) in s.lines().enumerate() {
                let bar = if i == 0 { "▌ " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(bar, Style::default().fg(theme::ACCENT)),
                    Span::styled(l.to_string(), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
                ]));
            }
            lines
        }
        Item::Assistant(s) => render_assistant(s),
        Item::ToolCall { tool, .. } if tool == "update_plan" => Vec::new(),
        Item::ToolCall { tool, args } => {
            let v = cx.viz.get(tool);
            let icon = v.map(|v| v.icon.as_str()).unwrap_or("•");
            let color = cat_color(v.map(|v| v.category).unwrap_or(ToolCategory::Other));
            let arg = crate::salient_arg(tool, args);
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{icon} "), Style::default().fg(color)),
                Span::styled(tool_label(tool), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
                Span::styled(arg, Style::default().fg(theme::DIM)),
            ])];
            if cx.full_transparency {
                lines.extend(json_block(args, "in"));
            }
            lines
        }
        Item::ToolResult { tool, ok, payload } => {
            if tool == "update_plan" && *ok { return render_plan(payload); }
            if let (Some(old), Some(new)) = (payload.get("old").and_then(|v| v.as_str()), payload.get("new").and_then(|v| v.as_str())) {
                let path = payload.get("path").and_then(|v| v.as_str()).unwrap_or("");
                return render_diff(old, new, path, cx.width);
            }
            let (mark, color, summary) = if !*ok {
                // Failures carry {"error": …}; policy denials carry {"reason": …}. Show
                // whichever is present so the user sees WHY, not a bare "error".
                let msg = payload.get("error").or_else(|| payload.get("reason"))
                    .and_then(|v| v.as_str()).unwrap_or("error").to_string();
                ("╰ ✗", theme::ERR, msg)
            } else { ("╰", theme::DIM, crate::result_summary(tool, payload)) };
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("  {mark} "), Style::default().fg(theme::FAINT)),
                Span::styled(summary, Style::default().fg(color)),
            ])];
            if cx.full_transparency { lines.extend(json_block(payload, "out")); }
            lines
        }
        Item::Compaction { before, after, summarized } => {
            let how = if *summarized { "summarized" } else { "pruned" };
            vec![Line::from(Span::styled(format!("  ↯ {how} context · {before} → {after} tokens"), Style::default().fg(theme::WARN)))]
        }
        Item::Verify { ok, summary } => {
            let (mark, color) = if *ok { ("✔", theme::OK) } else { ("✗", theme::ERR) };
            vec![Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(format!("verify · {summary}"), Style::default().fg(color)),
            ])]
        }
        Item::Notice(s) => s.lines().map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(theme::DIM)))).collect(),
        Item::Thinking(s) => {
            let style = Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC);
            if !cx.show_thinking {
                return vec![Line::from(Span::styled("  · thinking (hidden — /thinking)", Style::default().fg(theme::FAINT).add_modifier(Modifier::ITALIC)))];
            }
            let mut lines = vec![Line::from(Span::styled("  · thinking", style))];
            lines.extend(s.lines().map(|l| Line::from(Span::styled(format!("  {l}"), style))));
            lines
        }
    }
}

/// Inline approval rendering (PART 3: appended to the transcript stream, not a modal).
/// Rendered as a plain block in the same scrollable region — heading, diff hunk, then
/// options as plain numbered lines. Never a floating overlay, so it can never be clipped.
fn render_approval(action: &str, detail: Option<&str>, sel: usize) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Allow ", Style::default().fg(theme::TEXT)),
            Span::styled(tool_label(action).to_string(), Style::default().fg(theme::WARN).add_modifier(Modifier::BOLD)),
            Span::styled("?", Style::default().fg(theme::TEXT)),
        ]),
    ];
    if let Some(detail) = detail {
        lines.push(Line::from(""));
        for l in detail.lines().take(18) {
            let style = if l.starts_with('+') && !l.starts_with("+++") { Style::default().fg(theme::ADD_FG) }
                else if l.starts_with('-') && !l.starts_with("---") { Style::default().fg(theme::DEL_FG) }
                else { Style::default().fg(theme::DIM) };
            lines.push(Line::from(Span::styled(l.to_string(), style)));
        }
        let extra = detail.lines().count().saturating_sub(18);
        if extra > 0 {
            lines.push(Line::from(Span::styled(format!("… {extra} more lines"), Style::default().fg(theme::FAINT))));
        }
    }
    lines.push(Line::from(""));
    let opts = ["Yes, allow once", "Yes, always allow", "No, deny"];
    for (i, label) in opts.iter().enumerate() {
        if i == sel {
            lines.push(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(theme::ACCENT)),
                Span::styled(format!("{}. ", i + 1), Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(label.to_string(), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("  {}. ", i + 1), Style::default().fg(theme::DIM)),
                Span::styled(label.to_string(), Style::default().fg(theme::DIM)),
            ]));
        }
    }
    // Explicit ready signal — this line only exists once the options above are built,
    // so seeing it means "ready for input", not "still generating / stuck".
    lines.push(Line::from(vec![
        Span::styled("› ", Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled("waiting for your input", Style::default().fg(theme::ACCENT)),
    ]));
    lines.push(Line::from(Span::styled("↑↓ + enter · or press 1/2/3 · n to deny", Style::default().fg(theme::FAINT))));
    lines
}

fn render_assistant(s: &str) -> Vec<Line<'static>> {
    s.lines().map(|l| checklist_line(l).unwrap_or_else(|| Line::from(l.to_string()))).collect()
}

fn checklist_line(line: &str) -> Option<Line<'static>> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let rest = &line[indent_len..];
    let body = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* "))?;
    let (mark, color, text, done) = if let Some(t) = body.strip_prefix("[x]").or_else(|| body.strip_prefix("[X]")) {
        ("✔", theme::OK, t.trim_start(), true)
    } else if let Some(t) = body.strip_prefix("[ ]") {
        ("○", theme::DIM, t.trim_start(), false)
    } else { return None; };
    let text_style = if done { Style::default().fg(theme::DIM) } else { Style::default().fg(theme::TEXT) };
    Some(Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(format!("{mark} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(text.to_string(), text_style),
    ]))
}

fn json_block(v: &serde_json::Value, label: &str) -> Vec<Line<'static>> {
    let text = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    let style = Style::default().fg(Color::Rgb(110, 120, 130));
    let mut lines = vec![Line::from(Span::styled(format!("    ┌ {label}"), style))];
    // Cap lines per tool call so a huge payload never builds thousands of spans (PART 4).
    let total = text.lines().count();
    for l in text.lines().take(MAX_TOOL_OUTPUT_LINES) {
        lines.push(Line::from(Span::styled(format!("    │ {l}"), style)));
    }
    if total > MAX_TOOL_OUTPUT_LINES {
        let hidden = total - MAX_TOOL_OUTPUT_LINES;
        lines.push(Line::from(Span::styled(format!("    └ [+{hidden} more lines — toggle /detail]"), Style::default().fg(theme::FAINT))));
    }
    lines
}

const MIN_SIDE_BY_SIDE: u16 = 96;

/// Lines of unchanged context kept above/below each change (PART 5: hunk-based).
const DIFF_CONTEXT: usize = 3;

/// One display row of a hunk-filtered diff.
enum DiffRow {
    /// Unchanged context line (old_index, new_index, text).
    Ctx(usize, usize, String),
    /// Deleted line (old_index, text).
    Del(usize, String),
    /// Inserted line (new_index, text).
    Ins(usize, String),
    /// A collapsed run of `n` unchanged lines between hunks.
    Gap(usize),
}

/// Reduce a full unified diff to hunks: keep changed lines plus `DIFF_CONTEXT` lines
/// of surrounding context, collapsing longer unchanged runs into `Gap` markers.
/// This is what stops a 1000-line file with 3 edits from rendering 1000 lines (PART 5).
fn hunk_rows(old: &str, new: &str) -> Vec<DiffRow> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let raw: Vec<(ChangeTag, Option<usize>, Option<usize>, String)> = diff
        .iter_all_changes()
        .map(|c| (c.tag(), c.old_index(), c.new_index(), c.value().trim_end_matches(['\n', '\r']).to_string()))
        .collect();

    let is_change: Vec<bool> = raw.iter().map(|(t, ..)| !matches!(t, ChangeTag::Equal)).collect();
    let keep: Vec<bool> = (0..raw.len())
        .map(|i| {
            if is_change[i] { return true; }
            let lo = i.saturating_sub(DIFF_CONTEXT);
            let hi = (i + DIFF_CONTEXT).min(raw.len().saturating_sub(1));
            (lo..=hi).any(|j| is_change[j])
        })
        .collect();

    let mut rows: Vec<DiffRow> = Vec::new();
    let mut dropped = 0usize;
    for (i, (tag, oi, ni, text)) in raw.into_iter().enumerate() {
        if !keep[i] {
            dropped += 1;
            continue;
        }
        if dropped > 0 {
            rows.push(DiffRow::Gap(dropped));
            dropped = 0;
        }
        match tag {
            ChangeTag::Equal => rows.push(DiffRow::Ctx(oi.unwrap_or(0), ni.unwrap_or(0), text)),
            ChangeTag::Delete => rows.push(DiffRow::Del(oi.unwrap_or(0), text)),
            ChangeTag::Insert => rows.push(DiffRow::Ins(ni.unwrap_or(0), text)),
        }
    }
    if dropped > 0 {
        rows.push(DiffRow::Gap(dropped));
    }
    rows
}

fn gap_line(n: usize) -> Line<'static> {
    let plural = if n == 1 { "" } else { "s" };
    Line::from(Span::styled(format!("  ⋯ {n} unchanged line{plural}"), Style::default().fg(theme::FAINT)))
}

fn render_diff(old: &str, new: &str, path: &str, width: u16) -> Vec<Line<'static>> {
    let rows = hunk_rows(old, new);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if !path.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  ✎ ", Style::default().fg(theme::FAINT)),
            Span::styled(path.to_string(), Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD)),
        ]));
    }
    let ctx_num = Style::default().fg(theme::LINENO);
    let clip = |s: &str, w: usize| -> String {
        let t = s.trim_end_matches(['\n', '\r']);
        let n = t.chars().count();
        if n > w { let mut out: String = t.chars().take(w.saturating_sub(1)).collect(); out.push('…'); out } else { format!("{t:<w$}") }
    };
    // Side-by-side only helps when there are BOTH deletions and insertions to compare.
    // For a one-sided change (new file / pure addition or pure deletion) it wastes a
    // whole column and wraps badly — use the single-column unified layout instead.
    let has_del = rows.iter().any(|r| matches!(r, DiffRow::Del(..)));
    let has_ins = rows.iter().any(|r| matches!(r, DiffRow::Ins(..)));
    let unified = width < MIN_SIDE_BY_SIDE || !(has_del && has_ins);
    if unified {
        let body_w = (width as usize).saturating_sub(9).max(1);
        for row in &rows {
            let (sign, fg, bg, num, text) = match row {
                DiffRow::Gap(n) => { lines.push(gap_line(*n)); continue; }
                DiffRow::Del(oi, t) => ("-", theme::DEL_FG, Some(theme::DEL_BG), *oi, t),
                DiffRow::Ins(ni, t) => ("+", theme::ADD_FG, Some(theme::ADD_BG), *ni, t),
                DiffRow::Ctx(_, ni, t) => (" ", theme::DIM, None, *ni, t),
            };
            let n = format!("{:>4}", num + 1);
            let text = clip(text, body_w);
            let mut rowst = Style::default().fg(fg);
            let mut numst = ctx_num;
            if let Some(bg) = bg { rowst = rowst.bg(bg); numst = numst.bg(bg); }
            lines.push(Line::from(vec![
                Span::styled(format!("  {n} "), numst),
                Span::styled(format!("{sign} {text}"), rowst),
            ]));
        }
        return cap_diff(lines);
    }
    let col = ((width as usize).saturating_sub(14)) / 2;
    let push_row = |lines: &mut Vec<Line<'static>>, ln: Option<usize>, left: Option<&str>, rn: Option<usize>, right: Option<&str>, changed: bool| {
        let (lfg, lbg) = if changed && left.is_some() { (theme::DEL_FG, Some(theme::DEL_BG)) } else { (theme::DIM, None) };
        let (rfg, rbg) = if changed && right.is_some() { (theme::ADD_FG, Some(theme::ADD_BG)) } else { (theme::DIM, None) };
        let mut lst = Style::default().fg(lfg);
        let mut rst = Style::default().fg(rfg);
        if let Some(b) = lbg { lst = lst.bg(b); }
        if let Some(b) = rbg { rst = rst.bg(b); }
        let lnum = ln.map(|i| format!("{:>4}", i + 1)).unwrap_or_else(|| "    ".into());
        let rnum = rn.map(|i| format!("{:>4}", i + 1)).unwrap_or_else(|| "    ".into());
        let ltext = clip(left.unwrap_or(""), col); let rtext = clip(right.unwrap_or(""), col);
        lines.push(Line::from(vec![
            Span::styled(format!("  {lnum} "), ctx_num),
            Span::styled(format!("{ltext} "), lst),
            Span::styled("│ ", Style::default().fg(theme::FAINT)),
            Span::styled(format!("{rnum} "), ctx_num),
            Span::styled(rtext, rst),
        ]));
    };
    let mut dels: Vec<(usize, String)> = Vec::new();
    let mut inss: Vec<(usize, String)> = Vec::new();
    let flush = |lines: &mut Vec<Line<'static>>, dels: &mut Vec<(usize, String)>, inss: &mut Vec<(usize, String)>| {
        let n = dels.len().max(inss.len());
        for i in 0..n {
            let d = dels.get(i); let ins = inss.get(i);
            push_row(lines, d.map(|(n, _)| *n), d.map(|(_, s)| s.as_str()), ins.map(|(n, _)| *n), ins.map(|(_, s)| s.as_str()), true);
        }
        dels.clear(); inss.clear();
    };
    for row in rows {
        match row {
            DiffRow::Del(oi, text) => dels.push((oi, text)),
            DiffRow::Ins(ni, text) => inss.push((ni, text)),
            DiffRow::Ctx(oi, ni, text) => { flush(&mut lines, &mut dels, &mut inss); push_row(&mut lines, Some(oi), Some(&text), Some(ni), Some(&text), false); }
            DiffRow::Gap(n) => { flush(&mut lines, &mut dels, &mut inss); lines.push(gap_line(n)); }
        }
    }
    flush(&mut lines, &mut dels, &mut inss);
    cap_diff(lines)
}

fn cap_diff(mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if lines.len() > MAX_DIFF_LINES {
        let hidden = lines.len() - MAX_DIFF_LINES;
        lines.truncate(MAX_DIFF_LINES);
        lines.push(Line::from(Span::styled(format!("  … {hidden} more diff lines"), Style::default().fg(theme::FAINT))));
    }
    lines
}

fn draw_status(f: &mut Frame, model: &Model, area: Rect) {
    let mut left = vec![
        Span::styled("▌ ", Style::default().fg(theme::ACCENT)),
        Span::styled("medha", Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", model.model), Style::default().fg(theme::DIM)),
    ];
    if model.running {
        left.push(Span::styled(
            format!("  {} {} · {}", spinner_frame(model.anim_frame), activity_label(model), elapsed_str(model)),
            Style::default().fg(theme::WARN),
        ));
    }
    let ctx = match model.ctx_pct { Some(pct) => format!("ctx {pct}%"), None => "ctx —".to_string() };
    let think = match model.reasoning.enabled {
        Some(true) => format!("think {}", crate::effort_label(model.reasoning.effort)),
        Some(false) => "think off".to_string(),
        None => "think —".to_string(),
    };
    let hints = if model.running { "esc interrupt" } else { "/thinking · /detail · /help" };
    let right = format!("{ctx} · {think}   {hints}");
    let left_w: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let pad = (area.width as usize).saturating_sub(left_w + right.chars().count());
    let mut spans = left; spans.push(Span::raw(" ".repeat(pad))); spans.push(Span::styled(right, Style::default().fg(theme::FAINT)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn layout_input(text: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    let cur = cursor.min(chars.len());
    let mut rows: Vec<String> = vec![String::new()];
    let (mut crow, mut ccol) = (0usize, 0usize);
    for (i, &ch) in chars.iter().enumerate() {
        if i == cur { crow = rows.len() - 1; ccol = rows.last().unwrap().chars().count(); }
        if ch == '\n' { rows.push(String::new()); }
        else { if rows.last().unwrap().chars().count() >= width { rows.push(String::new()); } rows.last_mut().unwrap().push(ch); }
    }
    if cur >= chars.len() { crow = rows.len() - 1; ccol = rows.last().unwrap().chars().count(); }
    (rows, crow, ccol)
}

fn input_text_width(outer_width: u16) -> usize { outer_width.saturating_sub(6).max(1) as usize }
fn input_rows(model: &Model, outer_width: u16) -> usize {
    if model.input.is_empty() { return 1; }
    layout_input(&model.input, 0, input_text_width(outer_width)).0.len()
}

fn draw_input(f: &mut Frame, model: &Model, area: Rect) {
    let (accent, glyph) = if model.running { (theme::FAINT, "…") } else { (theme::ACCENT, "❯") };
    let block = Block::default().borders(Borders::ALL).border_type(ratatui::widgets::BorderType::Rounded).border_style(Style::default().fg(accent)).padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if model.input.is_empty() && !model.running {
        let line = Line::from(vec![
            Span::styled(format!("{glyph} "), Style::default().fg(accent).add_modifier(Modifier::BOLD)),
            Span::styled("Ask medha to build, fix, or explain something…   ( / for commands · \\ + enter or ctrl+j for newline )", Style::default().fg(theme::FAINT)),
        ]);
        f.render_widget(Paragraph::new(line), inner);
        f.set_cursor_position(ratatui::layout::Position::new(inner.x + 2, inner.y));
        return;
    }
    let tw = inner.width.saturating_sub(2).max(1) as usize;
    // `cursor` is a byte offset; layout_input positions by char index.
    let cursor_chars = model.input[..model.cursor.min(model.input.len())].chars().count();
    let (rows, crow, ccol) = layout_input(&model.input, cursor_chars, tw);
    let lines: Vec<Line> = rows.into_iter().enumerate().map(|(i, row)| {
        let gutter = if i == 0 { Span::styled(format!("{glyph} "), Style::default().fg(accent).add_modifier(Modifier::BOLD)) } else { Span::raw("  ") };
        Line::from(vec![gutter, Span::styled(row, Style::default().fg(theme::TEXT))])
    }).collect();
    f.render_widget(Paragraph::new(lines), inner);
    if !model.running { f.set_cursor_position(ratatui::layout::Position::new(inner.x + 2 + ccol as u16, inner.y + crow as u16)); }
}

fn draw_autocomplete(f: &mut Frame, model: &Model, input_area: Rect) {
    let matches = command_matches(&model.input);
    if matches.is_empty() { return; }
    let height = matches.len() as u16;
    let y = input_area.y.saturating_sub(height + 1);
    let area = Rect::new(input_area.x, y, input_area.width, height + 1);
    f.render_widget(ratatui::widgets::Clear, area);
    let sel = model.ac_sel.min(matches.len() - 1);
    let mut lines: Vec<Line> = Vec::with_capacity(matches.len());
    for (i, (c, d)) in matches.iter().enumerate() {
        if i == sel { lines.push(Line::from(vec![Span::styled("▌ ", Style::default().fg(theme::ACCENT)), Span::styled(c.to_string(), Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)), Span::styled(format!("  {d}"), Style::default().fg(theme::DIM))])); }
        else { lines.push(Line::from(vec![Span::styled("  ", Style::default()), Span::styled(c.to_string(), Style::default().fg(theme::TEXT)), Span::styled(format!("  {d}"), Style::default().fg(theme::FAINT))])); }
    }
    lines.push(Line::from(Span::styled("  ↑↓ select · tab/enter accept · esc dismiss", Style::default().fg(theme::FAINT))));
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_picker(f: &mut Frame, picker: &Picker, input_area: Rect) {
    let options = picker.kind.options();
    let height = options.len() as u16 + 1;
    let y = input_area.y.saturating_sub(height);
    let area = Rect::new(input_area.x, y, input_area.width, height);
    f.render_widget(ratatui::widgets::Clear, area);
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(format!("  {}", picker.kind.title().trim()), Style::default().fg(theme::FAINT)))];
    for (i, label) in options.iter().enumerate() {
        if i == picker.selected { lines.push(Line::from(vec![Span::styled("▌ ", Style::default().fg(theme::ACCENT)), Span::styled(label.to_string(), Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD))])); }
        else { lines.push(Line::from(Span::styled(format!("  {label}"), Style::default().fg(theme::TEXT)))); }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// View function — pure rendering from model
fn view(f: &mut Frame, model: &mut Model) {
    let area = f.area();
    model.viewport_height = area.height as usize;
    
    let text_rows = input_rows(model, area.width.saturating_sub(4)) as u16;
    let box_h = text_rows.clamp(1, 8) + 2;
    let chunks = Layout::default().direction(Direction::Vertical).constraints([
        Constraint::Min(3), Constraint::Length(1), Constraint::Length(box_h), Constraint::Length(1),
    ]).split(area);
    
    let pad_h = |area: Rect| { let pad = 2.min(area.width / 2); Rect { x: area.x + pad, width: area.width.saturating_sub(pad * 2), ..area } };
    
    draw_transcript(f, model, pad_h(chunks[0]));
    draw_input(f, model, pad_h(chunks[2]));
    draw_status(f, model, pad_h(chunks[3]));
    
    if model.input.starts_with('/') { draw_autocomplete(f, model, pad_h(chunks[2])); }
    if let Some(picker) = &model.picker { draw_picker(f, picker, pad_h(chunks[2])); }
    // Approval is now inline in transcript (PART 3) — no modal
}

fn draw_transcript(f: &mut Frame, model: &mut Model, area: Rect) {
    // Scroll math must use the TRANSCRIPT pane height, not the full frame height
    // (set in view()), or the last lines — e.g. the approval options — get clipped
    // off the bottom even when auto-scrolled.
    model.viewport_height = area.height as usize;
    if model.welcome {
        model.content_height = area.height as usize;
        draw_welcome(f, model, area);
        return;
    }
    let vw = area.width.max(1);
    // Width change re-wraps every item (physical rows depend on width).
    if model.cached_width != area.width {
        model.invalidate_all_renders();
        model.cached_width = area.width;
    }
    // Recompute per-item physical rows + total height only when content changed.
    // Each item is wrapped ONCE and memoized; a big diff is never re-laid-out per
    // frame. (This block is the only O(items) work, and only on change.)
    if model.dirty {
        let cx = RenderCtx { width: area.width, full_transparency: model.full_transparency, show_thinking: model.show_thinking, viz: &model.tool_viz };
        let mut total = 0usize;
        for e in model.items.iter_mut() {
            e.ensure(&cx, vw);
            total += e.height;
        }
        // Approval card: pre-wrap into physical rows too (PART 3, inline in stream).
        model.approval_rows = if let Some(pending) = model.pending_approval() {
            let mut rows = render_approval(&pending.action, pending.detail.as_deref(), model.approval_sel);
            // If more approvals are queued behind the current one, say so — so the
            // user knows to expect another prompt right after this one.
            if model.pending_approvals.len() > 1 {
                rows.push(Line::from(Span::styled(
                    format!("+{} more approval{} waiting", model.pending_approvals.len() - 1, if model.pending_approvals.len() > 2 { "s" } else { "" }),
                    Style::default().fg(theme::FAINT),
                )));
            }
            rows
                .iter()
                .flat_map(|l| wrap_line(l, vw as usize))
                .collect()
        } else {
            Vec::new()
        };
        total += model.approval_rows.len();
        model.total_rows = total;
        model.dirty = false;
    }

    // No spinner while an approval is pending — the user is being asked, nothing
    // is "working". The spinner is one virtual row appended at the very end.
    let show_spinner = model.running && model.pending_approval().is_none();
    model.content_height = model.total_rows + if show_spinner { 1 } else { 0 };
    // A pending approval must always be on screen — pin to the bottom.
    if model.pending_approval().is_some() {
        model.auto_scroll = true;
    }
    if model.auto_scroll { model.scroll_offset = model.max_scroll(); }

    // VIRTUALIZE: build only the physical rows inside the visible window
    // [top, bot). Per-frame cost is O(screen height), independent of transcript
    // size — this is what makes it scale to a large repo / long session.
    let top = model.scroll_offset;
    let bot = top + model.viewport_height;
    let mut visible: Vec<Line<'static>> = Vec::with_capacity(model.viewport_height);
    let mut off = 0usize;
    let push_block = |rows: &[Line<'static>], off: &mut usize, visible: &mut Vec<Line<'static>>| {
        let start = *off;
        let end = start + rows.len();
        *off = end;
        if end <= top || start >= bot {
            return;
        }
        let a = top.saturating_sub(start);
        let b = bot.min(end) - start;
        visible.extend(rows[a..b].iter().cloned());
    };
    for e in &model.items {
        if let Some(rows) = &e.lines {
            push_block(rows, &mut off, &mut visible);
        }
    }
    push_block(&model.approval_rows, &mut off, &mut visible);
    if show_spinner {
        let spinner = vec![Line::from(vec![
            Span::styled(spinner_frame(model.anim_frame), Style::default().fg(theme::ACCENT)),
            Span::styled(format!(" {}…", activity_label(model)), Style::default().fg(theme::DIM)),
        ])];
        push_block(&spinner, &mut off, &mut visible);
    }

    // Rows are already wrapped to width — render them directly (no ratatui wrap,
    // no full-buffer scroll).
    let p = Paragraph::new(visible).style(Style::default().fg(theme::TEXT));
    f.render_widget(p, area);

    // The approval card is now guaranteed on screen — safe to accept selection input.
    if model.pending_approval().is_some() && !model.approval_ready {
        model.approval_ready = true;
        tracing::debug!("approval card rendered");
    }
}

/// Main entry point (PART 0). Uses `ratatui::init`/`restore` (which install a
/// panic hook that restores the terminal) rather than `ratatui::run` + `block_on`:
/// we are already inside the `#[tokio::main]` runtime, so the async event loop is
/// driven with `.await` directly. Calling `block_on` here would panic ("cannot
/// start a runtime from within a runtime").
#[allow(clippy::too_many_arguments)]
pub async fn run_tea<P, L>(
    kernel: Arc<Kernel<P, L>>,
    session: Session,
    system: String,
    model_name: String,
    max_ctx: Option<u32>,
    budget: Budget,
    ui: lockfile::UiConfig,
    resumed: Vec<Message>,
    tx: mpsc::UnboundedSender<TuiEvent>,
    mut rx: mpsc::UnboundedReceiver<TuiEvent>,
) -> anyhow::Result<()>
where
    P: Provider + 'static,
    L: EventLog + 'static,
{
    use crossterm::event::{EnableBracketedPaste, DisableBracketedPaste};

    // Terminal setup with panic-safe restore hook (PART 0/2).
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);

    // Presentation flows from the tools' declared metadata (glyph + category) —
    // the single source of truth — so adding a tool needs no TUI edit.
    let tool_viz: HashMap<String, ToolViz> = kernel
        .executor
        .specs()
        .into_iter()
        .map(|s| (s.name, ToolViz { icon: s.icon, category: s.category }))
        .collect();
    let mut model = Model::new(model_name, max_ctx, kernel.provider.reasoning(), ui, tool_viz);
    let mut transcript = vec![Message::system(system)];
    transcript.extend(resumed); // prior conversation when resuming (else empty)
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(REDRAW_INTERVAL);
    let mut redraw_needed = true;

    // Initial draw
    terminal.draw(|f| view(f, &mut model)).ok();

    // Async event loop driven directly on the current runtime (PART 0.4).
    loop {
        if model.should_quit { break; }

        // Input (scroll/keys) redraws IMMEDIATELY (bypasses the frame throttle) so
        // scrolling steps evenly and feels responsive, instead of coalescing a burst
        // of wheel events into one big jump on the next tick. Agent-stream redraws
        // stay throttled (coalesced) so token floods don't thrash the screen.
        let mut immediate = false;

        tokio::select! {
            // Terminal events (input, resize, paste)
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(CtEvent::Key(key))) => { update(&mut model, Msg::KeyPress(key), &kernel, &session, &mut transcript, &budget, &tx); redraw_needed = true; immediate = true; }
                    Some(Ok(CtEvent::Mouse(m))) => match m.kind {
                        MouseEventKind::ScrollUp => { update(&mut model, Msg::MouseScroll(-2), &kernel, &session, &mut transcript, &budget, &tx); redraw_needed = true; immediate = true; }
                        MouseEventKind::ScrollDown => { update(&mut model, Msg::MouseScroll(2), &kernel, &session, &mut transcript, &budget, &tx); redraw_needed = true; immediate = true; }
                        _ => {}
                    },
                    Some(Ok(CtEvent::Paste(data))) => { update(&mut model, Msg::Paste(data), &kernel, &session, &mut transcript, &budget, &tx); redraw_needed = true; immediate = true; }
                    Some(Ok(CtEvent::Resize(_, h))) => { update(&mut model, Msg::Resize(h), &kernel, &session, &mut transcript, &budget, &tx); redraw_needed = true; immediate = true; }
                    _ => {}
                }
            }
            // Agent events — process this one, then DRAIN the rest of the channel this
            // wake so a burst of streaming tokens can't pile up and dump "3 at once".
            // Consecutive text deltas coalesce naturally (appended to one buffer).
            Some(ev) = rx.recv() => {
                update(&mut model, Msg::AgentEvent(ev), &kernel, &session, &mut transcript, &budget, &tx);
                let mut drained = 0u32;
                while let Ok(ev) = rx.try_recv() {
                    update(&mut model, Msg::AgentEvent(ev), &kernel, &session, &mut transcript, &budget, &tx);
                    drained += 1;
                    if drained >= 10_000 { break; } // safety bound per wake
                }
                if drained > 0 { tracing::trace!(drained, "coalesced agent events"); }
                redraw_needed = true;
            }
            // Tick for animations — only forces a redraw when something is actually
            // moving (spinner/intro) or content changed; idle ticks cost nothing.
            _ = ticker.tick() => {
                update(&mut model, Msg::Tick, &kernel, &session, &mut transcript, &budget, &tx);
                // The welcome splash breathes/resonates continuously; a live turn
                // has a spinner; otherwise idle ticks cost nothing.
                if model.running || model.welcome || model.intro_frame.is_some() || model.dirty {
                    redraw_needed = true;
                }
            }
        }

        // Redraw when: a user input just happened (immediate — snappy scroll/typing),
        // an unshown approval must appear, or the frame interval elapsed (throttled
        // path for streaming).
        let force = model.pending_approval().is_some() && !model.approval_ready;
        if redraw_needed && (immediate || force || model.last_redraw.elapsed() >= REDRAW_INTERVAL) {
            terminal.draw(|f| view(f, &mut model)).ok();
            model.last_redraw = Instant::now();
            redraw_needed = false;
        }

        // Drain queued messages after turn completes
        if !model.running && !model.queued.is_empty() {
            let line = model.queued.remove(0);
            spawn_turn(&mut model, &kernel, &session, &mut transcript, &budget, &tx, line);
        }
    }

    // Restore terminal on exit (PART 2).
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    Ok(())
}

use crossterm::execute;

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten a rendered line to its plain text (drops styling) for assertions.
    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }
    fn block(lines: &[Line]) -> String {
        lines.iter().map(text).collect::<Vec<_>>().join("\n")
    }

    // ---- PART 3: inline approval card ----

    #[test]
    fn approval_renders_heading_and_three_plain_options() {
        let lines = render_approval("fs_write", Some("+ added line\n- removed line"), 0);
        let out = block(&lines);
        assert!(out.contains("Allow"), "missing heading: {out}");
        // Exactly the three arrow-selectable options, as plain numbered text.
        assert!(out.contains("1. Yes, allow once"));
        assert!(out.contains("2. Yes, always allow"));
        assert!(out.contains("3. No, deny"));
        // Hint must advertise arrow-key selection, not only number keys.
        assert!(out.contains("↑↓"));
        // Explicit ready signal so the card can't be mistaken for "still generating".
        assert!(out.contains("waiting for your input"));
        assert!(!out.contains('┌') && !out.contains('│') && !out.contains('╭'), "approval must not draw a box");
    }

    #[test]
    fn approval_selection_marker_tracks_index() {
        let sel0 = block(&render_approval("fs_write", None, 0));
        let sel2 = block(&render_approval("fs_write", None, 2));
        // The accent marker sits on the selected option's line.
        assert!(sel0.contains("▌ 1. Yes, allow once"));
        assert!(sel2.contains("▌ 3. No, deny"));
    }

    // ---- input editing is byte-safe on multi-byte UTF-8 (no panic) ----

    #[test]
    fn typing_and_editing_multibyte_does_not_panic() {
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, HashMap::new());
        // Type "café" then a trailing char — the classic char-vs-byte panic case.
        for c in "café".chars() { m.insert_char(c); }
        m.insert_char('!');
        assert_eq!(m.input, "café!");
        assert_eq!(m.cursor, m.input.len()); // byte offset, on a boundary
        m.backspace(); // remove '!'
        m.backspace(); // remove 'é' (2 bytes)
        assert_eq!(m.input, "caf");
        m.move_left();
        m.move_left();
        m.insert_char('é'); // insert into the middle
        assert_eq!(m.input, "céaf");
        // Paste with a multibyte char already present must not panic either.
        m.insert_text("😀x");
        assert!(m.input.contains('😀'));
    }

    // ---- PART 5: hunk-based diff ----

    #[test]
    fn diff_collapses_unchanged_runs_into_gaps() {
        // 40 identical lines with a single change in the middle.
        let old: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        let mut new = old.clone();
        new[20] = "line 20 CHANGED".to_string();
        let rows = hunk_rows(&old.join("\n"), &new.join("\n"));
        // Far-away unchanged lines must be collapsed, not emitted one-by-one.
        assert!(rows.iter().any(|r| matches!(r, DiffRow::Gap(n) if *n > 0)), "expected a gap marker");
        assert!(rows.iter().any(|r| matches!(r, DiffRow::Ins(_, t) if t.contains("CHANGED"))));
        // Only context (3) around the change on each side survives, never all 40 lines.
        let ctx = rows.iter().filter(|r| matches!(r, DiffRow::Ctx(..))).count();
        assert!(ctx <= 8, "kept too much context: {ctx}");
    }

    #[test]
    fn diff_one_sided_uses_unified_layout() {
        // All-additions (new file) at wide width must NOT go side-by-side (which
        // would waste the whole left column) — single column instead.
        let out = block(&render_diff("", "line a\nline b\nline c", "f.rs", 200));
        assert!(!out.contains('│'), "one-sided diff should be single-column: {out}");
        assert!(out.contains("line a"));
    }

    #[test]
    fn diff_modification_uses_side_by_side_when_wide() {
        // A real modification (deletion + insertion) at wide width uses side-by-side.
        let out = block(&render_diff("old line\ncommon", "new line\ncommon", "f.rs", 200));
        assert!(out.contains('│'), "modification should be side-by-side when wide: {out}");
    }

    #[test]
    fn wrap_line_hard_wraps_long_run_and_preserves_text() {
        let line = Line::from(Span::styled("abcdefghij", Style::default().fg(theme::TEXT)));
        let rows = wrap_line(&line, 4);
        assert_eq!(rows.len(), 3, "10 chars / width 4 = 3 rows");
        let joined: String = rows.iter().flat_map(|l| l.spans.iter()).map(|s| s.content.to_string()).collect();
        assert_eq!(joined, "abcdefghij");
        assert!(rows.iter().all(|r| text(r).chars().count() <= 4));
    }

    #[test]
    fn wrap_line_breaks_at_spaces() {
        let rows = wrap_line(&Line::from("hello world foo"), 8);
        let texts: Vec<String> = rows.iter().map(text).collect();
        assert!(texts.iter().all(|t| t.chars().count() <= 8), "{texts:?}");
        assert!(texts.iter().all(|t| !t.starts_with(' ')), "breaking space should be dropped: {texts:?}");
        assert_eq!(texts, vec!["hello", "world", "foo"]);
    }

    #[test]
    fn wrap_line_fast_path_when_it_fits() {
        assert_eq!(wrap_line(&Line::from("short"), 80).len(), 1);
        assert_eq!(wrap_line(&Line::from(""), 80).len(), 1);
    }

    #[test]
    fn entry_height_equals_wrapped_rows() {
        // The virtualization invariant: an item's reported height is exactly the
        // number of physical rows it renders (so scroll math can't drift).
        let cats = HashMap::new();
        let cx = RenderCtx { width: 20, full_transparency: false, show_thinking: true, viz: &cats };
        let mut e = Entry::new(Item::Assistant("a fairly long line that must wrap across several rows here".into()));
        e.ensure(&cx, 20);
        assert_eq!(e.height, e.lines.as_ref().unwrap().len());
        assert!(e.height > 1, "long line should wrap to multiple physical rows");
    }

    #[test]
    fn activity_label_shows_streaming_tool_and_target() {
        let ui = lockfile::UiConfig::default();
        // The surface learns tool presentation from the executor specs; simulate it.
        let viz = |icon: &str, c: ToolCategory| ToolViz { icon: icon.into(), category: c };
        let cats = HashMap::from([
            ("fs.write".to_string(), viz("✎", ToolCategory::Write)),
            ("fs.read".to_string(), viz("◇", ToolCategory::Read)),
            ("shell.exec".to_string(), viz("❯", ToolCategory::Shell)),
        ]);
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, cats);
        // A streaming write shows the file basename — "writing medha.html", not "thinking".
        m.current_tool = Some(("fs.write".into(), Some("/Users/x/medha/medha.html".into())));
        assert_eq!(activity_label(&m), "writing medha.html");
        // Name-only (target not sniffed yet) still shows the verb.
        m.current_tool = Some(("fs.read".into(), None));
        assert_eq!(activity_label(&m), "reading");
        m.current_tool = Some(("shell.exec".into(), Some("cargo build".into())));
        assert_eq!(activity_label(&m), "running command cargo build");
    }

    #[test]
    fn short_target_uses_basename_and_clips() {
        assert_eq!(short_target("/Users/x/medha/medha.html"), "medha.html");
        assert_eq!(short_target("cargo build"), "cargo build");
        assert_eq!(short_target(&"a".repeat(50)).chars().count(), 32);
    }

    #[test]
    fn entry_memoizes_render() {
        let cats = HashMap::new();
        let cx = RenderCtx { width: 80, full_transparency: false, show_thinking: true, viz: &cats };
        let mut e = Entry::new(Item::User("hi".into()));
        assert!(e.lines.is_none());
        e.ensure(&cx, 80);
        assert!(e.lines.is_some(), "render should be cached after ensure");
        let h = e.height;
        e.ensure(&cx, 80); // reuse — no recompute
        assert_eq!(e.height, h);
        e.invalidate();
        assert!(e.lines.is_none(), "invalidate clears the cache");
    }

    #[test]
    fn diff_render_shows_gap_and_change() {
        let old = (0..30).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
        let mut v: Vec<String> = (0..30).map(|i| format!("l{i}")).collect();
        v[15] = "l15!".to_string();
        let new = v.join("\n");
        let out = block(&render_diff(&old, &new, "f.rs", 80));
        assert!(out.contains("unchanged line"), "should show a collapsed-context marker: {out}");
        assert!(out.contains("l15!"));
    }

    // ---- PART 2: paste helpers ----

    #[test]
    fn strip_markers_removes_guards_but_keeps_content() {
        // Content deliberately contains the digits/brackets the old trim logic would eat.
        let raw = "\u{1b}[200~code[200] = 0~ok\u{1b}[201~";
        assert_eq!(strip_paste_markers(raw), "code[200] = 0~ok");
    }

    #[test]
    fn expand_tokens_round_trips_and_ignores_plain_text() {
        let pastes = vec!["FULL CONTENT".to_string()];
        assert_eq!(expand_paste_tokens(&pastes, "before [paste #0: 12 chars] after"), "before FULL CONTENT after");
        // A bare bracket that isn't a real token is left untouched.
        assert_eq!(expand_paste_tokens(&pastes, "arr[0] = 1"), "arr[0] = 1");
        // No pastes → identity.
        assert_eq!(expand_paste_tokens(&[], "[paste #0: 5 chars]"), "[paste #0: 5 chars]");
    }

    // ---- approval queue: concurrent approvals must not clobber each other ----
    //
    // Regression for the "rejected by human after I clicked yes" bug: the kernel
    // dispatches tool calls concurrently (buffered), so several `confirm()`
    // requests can arrive in one turn. The old single-slot `pending_approval`
    // overwrote the prior request, dropping its `oneshot::Sender`. The kernel's
    // `TuiGate::confirm` reads a dropped sender as `Approval::Deny` → the spurious
    // "rejected by human". The queue must hold all of them.

    fn push_approval(model: &mut Model, action: &str) -> oneshot::Receiver<kernel::Approval> {
        let (tx, rx) = oneshot::channel();
        let ev = TuiEvent::Approval(action.to_string(), None, tx);
        // Drive the same path handle_agent_event uses, minus the generic plumbing.
        match ev {
            TuiEvent::Approval(action, detail, responder) => {
                if model.auto_approve.contains(&action) {
                    let _ = responder.send(kernel::Approval::Once);
                } else {
                    let was_empty = model.pending_approvals.is_empty();
                    model.pending_approvals.push_back(PendingApproval { action, detail, responder });
                    if was_empty {
                        model.approval_sel = 0;
                        model.approval_ready = false;
                        model.dirty = true;
                    }
                }
            }
            _ => unreachable!(),
        }
        rx
    }

    #[test]
    fn concurrent_approvals_queue_without_dropping_responders() {
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, HashMap::new());

        // Two approvals arrive back-to-back (the concurrent-dispatch case).
        let mut rx1 = push_approval(&mut m, "Read access to /outside/a");
        let mut rx2 = push_approval(&mut m, "Read access to /outside/b");

        // Both are queued; the first is the one rendered/answerable.
        assert_eq!(m.pending_approvals.len(), 2);
        assert_eq!(m.pending_approval().map(|p| p.action.as_str()), Some("Read access to /outside/a"));

        // Answer the visible one (Yes, allow once). Its responder must fire.
        m.approval_ready = true;
        handle_approval_key(&mut m, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(rx1.try_recv(), Ok(kernel::Approval::Once));

        // The second approval is now front-and-center — NOT dropped.
        assert_eq!(m.pending_approvals.len(), 1);
        assert_eq!(m.pending_approval().map(|p| p.action.as_str()), Some("Read access to /outside/b"));

        // Answer it too; its responder fires as well (this was the dropped one).
        m.approval_ready = true;
        handle_approval_key(&mut m, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(rx2.try_recv(), Ok(kernel::Approval::Deny));
        assert!(m.pending_approvals.is_empty());
    }

    #[test]
    fn esc_is_not_wired_to_deny_an_approval() {
        // Esc must cancel a running turn, not answer "No" on a pending approval.
        // Verify the deny arm no longer matches Esc: with a pending approval and
        // approval_ready set, Esc must leave the queue untouched (the top-level
        // handler intercepts it before handle_approval_key ever runs).
        let lines = render_approval("fs_write", None, 0);
        let help = block(&lines);
        assert!(!help.contains("esc to reject"), "help text still ties Esc to deny: {help}");

        // And at the queue level: routing Esc through handle_approval_key must not
        // pop or answer anything (Esc is handled by the caller, not here).
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, HashMap::new());
        let _rx = push_approval(&mut m, "Read access to /outside/a");
        m.approval_ready = true;
        handle_approval_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(m.pending_approvals.len(), 1, "Esc must not consume a pending approval");
    }
}