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

mod update;
mod view;
use update::*;
use view::*;

/// Shared palette for the TUI's visual identity (amber accent + indigo depth).
pub(crate) mod theme {
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