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
use sandbox::WorkspaceSandbox;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

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
    ("/resume", "switch to a past session"),
    ("/rewind", "time-travel: branch from an earlier turn (undoes later edits)"),
    ("/tasks", "list background shell tasks (running/finished)"),
    ("/clear", "reset the conversation"),
    ("/exit", "quit (also Ctrl-D)"),
];

fn command_matches(input: &str) -> Vec<(&'static str, &'static str)> {
    COMMANDS.iter().filter(|(c, _)| c.starts_with(input)).copied().collect()
}

/// A line is a slash command only when its FIRST TOKEN is a known command.
/// Anything else starting with `/` — a pasted absolute path, "/Users/… open
/// this" — is chat for the model, not an "unknown command" error.
fn is_slash_command(line: &str) -> bool {
    match line.split_whitespace().next() {
        Some(tok) => COMMANDS.iter().any(|(c, _)| *c == tok),
        None => false,
    }
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
    async fn confirm(&self, action: &str, detail: Option<&str>, escalated: bool) -> kernel::Approval {
        let (resp_tx, resp_rx) = oneshot::channel();
        let req = TuiEvent::Approval(action.to_string(), detail.map(str::to_string), escalated, resp_tx);
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
    Compaction(u32, u32, bool, Option<String>),
    /// Compaction is running (true) / finished (false) — drives the live indicator.
    Compacting(bool),
    Usage(u32, u32),
    /// Session cost so far in USD; `true` = indicative list price (shown "est.").
    Cost(f64, bool),
    Verify(bool, String),
    Approval(String, Option<String>, bool, oneshot::Sender<kernel::Approval>),
    Done(Vec<Message>, StopReason),
    Error(String),
    /// A queued steer was applied at a turn boundary — promote its "queued"
    /// notice to a real user line.
    Steered(String),
    /// The session ended with steers never applied (cancel / finish raced
    /// them) — give the text back to the input box, never lose it.
    SteersReturned(Vec<String>),
    /// `/resume` completed loading the session list from the log.
    SessionsLoaded(Vec<kernel::SessionMeta>),
    /// A past session's events were replayed into a transcript; swap to it.
    Resumed(ulid::Ulid, Vec<Message>),
    /// `/rewind` completed loading this session's rewind points from the log.
    RewindPointsLoaded(Vec<RewindPoint>),
    /// A rewind finished. `new_id` is `Some` for conversation scopes (swap to
    /// the forked branch); `None` for code-only (conversation untouched). `msgs`
    /// is the branch's replayed conversation, `rolled` the files reverted,
    /// `prefill` the chosen prompt to drop back into the input box.
    Rewound {
        new_id: Option<ulid::Ulid>,
        msgs: Vec<Message>,
        rolled: usize,
        scope: RewindScope,
        prefill: Option<String>,
    },
}

/// One rewind point offered by `/rewind` — a past user prompt. Rewinding *to* a
/// message goes back to the state right BEFORE that message ran: the message and
/// everything after it leave the conversation, the code reverts to before that
/// turn's edits, and the message text is put back in the input box to edit and
/// re-send (an edit-and-resubmit affordance). `label` is the prompt (truncated) for the
/// picker; `at_event` is that user-message event (the cut is before it); `files`
/// is how many files a code rollback from here would revert, so the scope menu
/// can show the count and hide the code options when it's zero.
#[derive(Clone, Debug)]
pub(crate) struct RewindPoint {
    pub at_event: ulid::Ulid,
    pub label: String,
    pub files: usize,
}

/// What a `/rewind` restores once the user picks a scope — three independent
/// restore actions. Conversation-touching scopes fork the session (the original
/// is preserved) and prefill the chosen prompt; `Code` alone is a pure file
/// revert that leaves the conversation as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RewindScope {
    /// Rewind the conversation only; leave the working files as they are now.
    Conversation,
    /// Rewind the conversation *and* roll the files back to before the turn.
    ConversationAndCode,
    /// Roll the files back only; keep the conversation intact (no fork/prefill).
    Code,
}

impl RewindScope {
    /// True for scopes that rewind the conversation (fork + prefill + swap).
    fn touches_conversation(self) -> bool {
        matches!(self, RewindScope::Conversation | RewindScope::ConversationAndCode)
    }
    /// True for scopes that roll files back.
    fn touches_code(self) -> bool {
        matches!(self, RewindScope::Code | RewindScope::ConversationAndCode)
    }
}

impl RewindPoint {
    /// The scope menu shown after this point is chosen (step 2 of `/rewind`),
    /// ordered most-destructive first: code+conversation, conversation, code.
    /// The two code options are omitted when no files were edited from here on
    /// (nothing to roll back). `None` action = cancel.
    fn scope_options(&self) -> Vec<(String, Option<RewindScope>)> {
        // "tracked" (K18): rollback reverts only snapshot-carrying writes —
        // shell-side mutations (`sed -i`, `git checkout`) are not undone.
        let plural = if self.files == 1 { "tracked file" } else { "tracked files" };
        let mut opts = Vec::new();
        if self.files > 0 {
            opts.push((
                format!("⏪ restore code + conversation — roll back {} {plural}", self.files),
                Some(RewindScope::ConversationAndCode),
            ));
        }
        opts.push((
            "↩ restore conversation only — keep current files".to_string(),
            Some(RewindScope::Conversation),
        ));
        if self.files > 0 {
            opts.push((
                format!("⟲ restore code only — keep conversation, roll back {} {plural}", self.files),
                Some(RewindScope::Code),
            ));
        }
        opts.push(("✕ cancel".to_string(), None));
        opts
    }
}

/// One rendered line-group in the transcript
#[derive(Debug)]
enum Item {
    User(String),
    Assistant(String),
    ToolCall { tool: String, args: serde_json::Value },
    ToolResult { tool: String, ok: bool, payload: serde_json::Value },
    Compaction { before: u32, after: u32, summarized: bool, summary: Option<String> },
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
    /// K15: streaming-assistant cache — the rendered+wrapped rows of the text
    /// prefix up to the last '\n', which per-line rendering guarantees can
    /// never change as more deltas append. Only the tail line re-renders per
    /// frame (previously the whole message did: quadratic over the stream).
    stream_cache: Option<StreamCache>,
}

struct StreamCache {
    prefix_bytes: usize,
    width: u16,
    rows: Vec<Line<'static>>,
}

impl Entry {
    fn new(item: Item) -> Self {
        Self { item, lines: None, height: 0, stream_cache: None }
    }
    fn invalidate(&mut self) {
        self.lines = None;
    }
    /// Render this item to PHYSICAL rows (each already wrapped to `width`, so one
    /// stored line = one screen row). Runs once; reused until invalidated. `height`
    /// is the exact row count — no separate wrap measurement, so scroll math and
    /// the rendered slice can never drift.
    fn ensure(&mut self, cx: &RenderCtx<'_>, width: u16) {
        if self.lines.is_some() {
            return;
        }
        // Incremental path for streaming assistant text (K15): render_assistant
        // is strictly per-line (no cross-line state), so rows for the prefix up
        // to the last newline are stable and cached; only the tail re-renders.
        if let Item::Assistant(s) = &self.item {
            let split = s.rfind('\n').map(|i| i + 1).unwrap_or(0);
            let reuse = self
                .stream_cache
                .as_ref()
                .is_some_and(|c| c.prefix_bytes == split && c.width == width);
            if !reuse {
                let mut rows: Vec<Line<'static>> = Vec::new();
                for logical in view::render_assistant(&s[..split]) {
                    rows.extend(wrap_line(&logical, width as usize));
                }
                self.stream_cache = Some(StreamCache { prefix_bytes: split, width, rows });
            }
            let mut rows = self.stream_cache.as_ref().unwrap().rows.clone();
            for logical in view::render_assistant(&s[split..]) {
                rows.extend(wrap_line(&logical, width as usize));
            }
            self.height = rows.len();
            self.lines = Some(rows);
            return;
        }
        let mut rows: Vec<Line<'static>> = Vec::new();
        for logical in render_item(&self.item, cx) {
            rows.extend(wrap_line(&logical, width as usize));
        }
        self.height = rows.len();
        self.lines = Some(rows);
    }
}

/// Word-wrap one logical line into physical rows of at most `width` columns,
/// preserving each span's style. Long words hard-break. This is the single source
/// of truth for layout — the renderer draws these rows directly (no ratatui wrap),
/// so measured height always equals rendered rows (essential for virtualization).
fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    // Flatten to (char, style); measure in terminal CELLS, not chars — CJK and
    // emoji occupy 2 columns (K14), so char counting clips and mis-cursors.
    let cell_w = |c: char| c.width().unwrap_or(0);
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        let st = span.style;
        for c in span.content.chars() {
            chars.push((c, st));
        }
    }
    // Fast-path lines that already fit.
    if chars.iter().map(|&(c, _)| cell_w(c)).sum::<usize>() <= width {
        return vec![line.clone()];
    }

    // Greedy word wrap: fill up to `width` cells, breaking at the last space
    // when possible.
    let n = chars.len();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        // Extend while the row's cell width fits (always take ≥1 char so a
        // double-width char on a width-1 row can't loop forever).
        let mut used = 0usize;
        let mut hard_end = i;
        while hard_end < n {
            let cw = cell_w(chars[hard_end].0);
            if used + cw > width && hard_end > i {
                break;
            }
            used += cw;
            hard_end += 1;
        }
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
    /// True for a trust-flow-escalated (web-tainted) action — never auto-approved
    /// and never remembered via "always" (K9).
    escalated: bool,
    responder: oneshot::Sender<kernel::Approval>,
}

/// Reasoning control / session picker kind. Not `Copy` — `Session` owns a `Vec`.
#[derive(Clone)]
enum PickerKind {
    Think,
    Effort,
    /// Browse past sessions to resume. Holds the list from `log.sessions()`.
    Session(Vec<kernel::SessionMeta>),
    /// Time-travel cut points in the current session. Holds the list from
    /// `log.events()`, one entry per past user turn.
    Rewind(Vec<RewindPoint>),
    /// Step 2 of `/rewind`: having chosen a cut point, pick the scope
    /// (conversation only · conversation + code · cancel).
    RewindMode(RewindPoint),
}

impl PickerKind {
    fn title(&self) -> String {
        match self {
            PickerKind::Think => " thinking — ↑↓ select, Enter apply, Esc cancel ".into(),
            PickerKind::Effort => " effort — ↑↓ select, Enter apply, Esc cancel ".into(),
            PickerKind::Session(_) => " resume a session — ↑↓ select, Enter open, Esc cancel ".into(),
            PickerKind::Rewind(_) => " rewind to a turn — ↑↓ select, Enter choose, Esc cancel ".into(),
            PickerKind::RewindMode(p) => {
                format!(" rewind → “{}” — ↑↓ select, Enter apply, Esc back ", p.label)
            }
        }
    }
    /// Dynamic labels for each row. For `Session`, each row is a one-line
    /// summary (date · events · title) matching the `--sessions` headless format.
    fn labels(&self) -> Vec<String> {
        match self {
            PickerKind::Think => vec!["on".into(), "off".into()],
            PickerKind::Effort => vec!["low".into(), "medium".into(), "high".into()],
            PickerKind::Session(sessions) => sessions
                .iter()
                .map(|s| {
                    let when = chrono::DateTime::from_timestamp(s.last_ts as i64, 0)
                        .map(|d| d.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "?".into());
                    let title = if s.title.is_empty() { "(no messages)" } else { &s.title };
                    format!("{when} · {} events · {title}", s.events)
                })
                .collect(),
            PickerKind::Rewind(points) => points
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let edits = if p.files == 0 {
                        String::new()
                    } else if p.files == 1 {
                        "  · 1 edit since".to_string()
                    } else {
                        format!("  · {} edits since", p.files)
                    };
                    format!("{}. {}{edits}", i + 1, p.label)
                })
                .collect(),
            PickerKind::RewindMode(p) => p.scope_options().into_iter().map(|(l, _)| l).collect(),
        }
    }
    fn apply<P: kernel::Provider>(&self, provider: &P, choice: &str) -> String {
        match self {
            PickerKind::Think => crate::apply_think_command(provider, choice),
            PickerKind::Effort => crate::apply_effort_command(provider, choice),
            // Session/Rewind kinds are handled by Enter in handle_key (they spawn
            // async log work or open a sub-picker), not via apply — unreachable.
            PickerKind::Session(_) | PickerKind::Rewind(_) | PickerKind::RewindMode(_) => {
                String::new()
            }
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
    /// Session cost so far (USD, `true` = indicative "est." figure), when known.
    cost_usd: Option<(f64, bool)>,
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
    /// Interrupt handle for the running turn: Esc → graceful cancel_turn,
    /// Enter mid-turn → steer (applied at the next turn boundary).
    interrupt: Option<kernel::InterruptHandle>,
    /// An Esc was sent and the kernel is settling in-flight tools — used to
    /// show one "stopping…" notice instead of one per Esc press.
    cancelling: bool,
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
    /// Workspace sandbox handle, used only to roll files back on `/rewind`
    /// (code time-travel). File ops still go through the executor for turns;
    /// this is the out-of-band restore path.
    restore: Arc<WorkspaceSandbox>,
    /// Live background shell tasks (promoted `shell.exec` runs), polled from the
    /// executor so the *user* sees what's running — not just the model.
    bg_tasks: Vec<kernel::BackgroundTask>,
    /// Running-task count last reflected on screen, so the status line only
    /// forces an idle redraw when the number actually changes.
    bg_shown_running: usize,
    /// A compaction (summarize pass) is currently running — shows a live
    /// "compacting…" indicator.
    compacting: bool,
    /// Expand compaction cards to show their full summary text (toggled by ^E).
    show_summary: bool,
}

impl Model {
    fn new(
        model: String,
        max_ctx: Option<u32>,
        reasoning: kernel::ReasoningConfig,
        ui: lockfile::UiConfig,
        tool_viz: HashMap<String, ToolViz>,
        restore: Arc<WorkspaceSandbox>,
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
            cost_usd: None,
            model,
            max_ctx,
            running: false,
            current_tool: None,
            turn_started: None,
            interrupt: None,
            cancelling: false,
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
            restore,
            bg_tasks: Vec::new(),
            bg_shown_running: 0,
            compacting: false,
            show_summary: false,
        }
    }

    /// How many background tasks are still running.
    fn bg_running(&self) -> usize {
        self.bg_tasks.iter().filter(|t| t.running).count()
    }

    /// The front of the approval queue (the one currently rendered/answerable),
    /// or `None` if no approval is pending. Callers that used to read a single
    /// `pending_approval` field go through here so the queue is transparent.
    fn pending_approval(&self) -> Option<&PendingApproval> {
        self.pending_approvals.front()
    }

    /// Deny and clear every queued approval — called whenever a turn ends
    /// (Done/Error/Interrupted). An Esc-cancel drops the gate's receiver but the
    /// card stayed on screen, and because it intercepts all keys, every keystroke
    /// then routed to a prompt whose responder is dead — the UI froze. Denying is
    /// safe: the turn is already over, so the decisions no longer matter.
    fn deny_pending_approvals(&mut self) {
        while let Some(p) = self.pending_approvals.pop_front() {
            let _ = p.responder.send(kernel::Approval::Deny);
        }
        self.approval_sel = 0;
        self.approval_ready = false;
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

    /// Remove the most recent notice starting with `prefix` — e.g. a "queued"
    /// steer marker being promoted to a real user line once it applies.
    fn remove_last_notice(&mut self, prefix: &str) {
        if let Some(idx) = self
            .items
            .iter()
            .rposition(|e| matches!(&e.item, Item::Notice(n) if n.starts_with(prefix)))
        {
            self.items.remove(idx);
            self.dirty = true;
        }
    }

    /// Live-status notices (/tasks): when the most recent item is already a
    /// notice with this `prefix`, update it in place — re-running the command
    /// must refresh one block, not stack identical copies in the scrollback.
    fn upsert_notice(&mut self, prefix: &str, text: String) {
        if let Some(e) = self.items.back_mut() {
            if matches!(&e.item, Item::Notice(n) if n.starts_with(prefix)) {
                e.item = Item::Notice(text);
                e.invalidate();
                self.dirty = true;
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
                return;
            }
        }
        self.push_item(Item::Notice(text));
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
    mut session: Session,
    system: String,
    model_name: String,
    max_ctx: Option<u32>,
    budget: Budget,
    ui: lockfile::UiConfig,
    resumed: Vec<Message>,
    restore: Arc<WorkspaceSandbox>,
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
    let mut model = Model::new(model_name, max_ctx, kernel.provider.reasoning(), ui, tool_viz, restore);
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
                    Some(Ok(CtEvent::Key(key))) => { update(&mut model, Msg::KeyPress(key), &kernel, &mut session, &mut transcript, &budget, &tx); redraw_needed = true; immediate = true; }
                    Some(Ok(CtEvent::Mouse(m))) => match m.kind {
                        MouseEventKind::ScrollUp => { update(&mut model, Msg::MouseScroll(-2), &kernel, &mut session, &mut transcript, &budget, &tx); redraw_needed = true; immediate = true; }
                        MouseEventKind::ScrollDown => { update(&mut model, Msg::MouseScroll(2), &kernel, &mut session, &mut transcript, &budget, &tx); redraw_needed = true; immediate = true; }
                        _ => {}
                    },
                    Some(Ok(CtEvent::Paste(data))) => { update(&mut model, Msg::Paste(data), &kernel, &mut session, &mut transcript, &budget, &tx); redraw_needed = true; immediate = true; }
                    Some(Ok(CtEvent::Resize(_, h))) => { update(&mut model, Msg::Resize(h), &kernel, &mut session, &mut transcript, &budget, &tx); redraw_needed = true; immediate = true; }
                    _ => {}
                }
            }
            // Agent events — process this one, then DRAIN the rest of the channel this
            // wake so a burst of streaming tokens can't pile up and dump "3 at once".
            // Consecutive text deltas coalesce naturally (appended to one buffer).
            Some(ev) = rx.recv() => {
                update(&mut model, Msg::AgentEvent(ev), &kernel, &mut session, &mut transcript, &budget, &tx);
                let mut drained = 0u32;
                while let Ok(ev) = rx.try_recv() {
                    update(&mut model, Msg::AgentEvent(ev), &kernel, &mut session, &mut transcript, &budget, &tx);
                    drained += 1;
                    if drained >= 10_000 { break; } // safety bound per wake
                }
                if drained > 0 { tracing::trace!(drained, "coalesced agent events"); }
                redraw_needed = true;
            }
            // Tick for animations — only forces a redraw when something is actually
            // moving (spinner/intro) or content changed; idle ticks cost nothing.
            _ = ticker.tick() => {
                update(&mut model, Msg::Tick, &kernel, &mut session, &mut transcript, &budget, &tx);
                // The welcome splash breathes/resonates continuously; a live turn
                // has a spinner; a running background task animates its indicator;
                // otherwise idle ticks cost nothing.
                let running = model.bg_running();
                if model.running || model.welcome || model.intro_frame.is_some() || model.dirty
                    || running > 0
                    || running != model.bg_shown_running
                {
                    redraw_needed = true;
                }
                model.bg_shown_running = running;
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
    /// A throwaway jailed sandbox for constructing a `Model` in tests (the
    /// rewind restore handle; unused by the assertions here).
    fn test_sbx() -> Arc<WorkspaceSandbox> {
        let dir = std::env::temp_dir().join(format!("medha-tui-test-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap())
    }

    // ---- /rewind scope menu (step 2) ----

    #[test]
    fn rewind_scope_menu_hides_code_options_when_nothing_to_undo() {
        // No file edits from this point on → only "restore conversation" + cancel;
        // offering a code rollback that reverts nothing would be misleading.
        let p = RewindPoint { at_event: ulid::Ulid::new(), label: "hi".into(), files: 0 };
        let opts = p.scope_options();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].1, Some(RewindScope::Conversation));
        assert_eq!(opts[1].1, None, "last row is cancel");
        assert!(!opts.iter().any(|(_, s)| matches!(s, Some(RewindScope::ConversationAndCode | RewindScope::Code))));
    }

    #[test]
    fn rewind_scope_menu_offers_all_three_with_count_when_edits_exist() {
        // Order: code+conversation, conversation, code, then cancel.
        let p = RewindPoint { at_event: ulid::Ulid::new(), label: "hi".into(), files: 3 };
        let opts = p.scope_options();
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0].1, Some(RewindScope::ConversationAndCode));
        // "tracked" is deliberate (K18): only snapshot-tracked writes revert.
        assert!(opts[0].0.contains("3 tracked files"), "count shown: {}", opts[0].0);
        assert_eq!(opts[1].1, Some(RewindScope::Conversation));
        assert_eq!(opts[2].1, Some(RewindScope::Code));
        assert!(opts[2].0.contains("3 tracked files"), "count shown: {}", opts[2].0);
        assert_eq!(opts[3].1, None, "last row is cancel");
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
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, HashMap::new(), test_sbx());
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
    fn slash_parsing_only_fires_on_known_commands() {
        // Known commands, with and without args.
        assert!(is_slash_command("/help"));
        assert!(is_slash_command("/think on"));
        assert!(is_slash_command("/effort high"));
        // A pasted absolute path is CHAT, not an unknown-command error.
        assert!(!is_slash_command("/Users/x/medha/learn.html open this in browser"));
        assert!(!is_slash_command("/tmp/foo.txt"));
        // Near-miss typo goes to the model too (autocomplete guides while typing).
        assert!(!is_slash_command("/claer"));
        assert!(!is_slash_command("/"));
    }

    #[test]
    fn steer_events_promote_queued_notice_and_return_unsent_text() {
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, HashMap::new(), test_sbx());
        let mut session = kernel::Session::new();
        let mut transcript: Vec<Message> = Vec::new();

        // Steer queued in the TUI → notice; kernel applies it → Steered
        // promotes the notice to a real user line (exactly one).
        m.push_notice("↳ queued for this task: check the tests");
        update::handle_agent_event(&mut m, TuiEvent::Steered("check the tests".into()), &mut session, &mut transcript);
        assert!(!m.items.iter().any(|e| matches!(&e.item, Item::Notice(n) if n.starts_with("↳ queued"))));
        assert!(m.items.iter().any(|e| matches!(&e.item, Item::User(s) if s == "check the tests")));

        // Cancel raced a steer → the text lands back in the input box.
        m.push_notice("↳ queued for this task: do Y instead");
        update::handle_agent_event(&mut m, TuiEvent::SteersReturned(vec!["do Y instead".into()]), &mut session, &mut transcript);
        assert_eq!(m.input, "do Y instead", "returned steer must be editable, not lost");
        assert_eq!(m.cursor, m.input.len());
        assert!(!m.items.iter().any(|e| matches!(&e.item, Item::Notice(n) if n.starts_with("↳ queued"))));
    }

    #[test]
    fn text_and_reasoning_stream_into_separate_transcript_items() {
        // The display side of the "answer hidden behind thinking" bug: text
        // deltas must build ONE visible Assistant item; only Reasoning deltas
        // may create a (collapsible) Thinking item.
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, HashMap::new(), test_sbx());
        let mut session = kernel::Session::new();
        let mut transcript: Vec<Message> = Vec::new();

        // A pure-text answer that mentions think tags — streamed in deltas.
        for d in ["Shape 2: `<think>", "` tags) and the rest ", "of the answer"] {
            update::handle_agent_event(&mut m, TuiEvent::Text(d.to_string()), &mut session, &mut transcript);
        }
        let thinking = m.items.iter().filter(|e| matches!(e.item, Item::Thinking(_))).count();
        assert_eq!(thinking, 0, "no Thinking item for a text-only answer");
        assert!(
            m.items.iter().any(|e| matches!(&e.item, Item::Assistant(s) if s == "Shape 2: `<think>` tags) and the rest of the answer")),
            "the full answer is one visible Assistant item"
        );

        // Genuine reasoning deltas DO make a Thinking item, answer separate.
        update::handle_agent_event(&mut m, TuiEvent::Reasoning("planning".into()), &mut session, &mut transcript);
        update::handle_agent_event(&mut m, TuiEvent::Text("done.".into()), &mut session, &mut transcript);
        assert!(m.items.iter().any(|e| matches!(&e.item, Item::Thinking(s) if s == "planning")));
        assert!(m.items.iter().any(|e| matches!(&e.item, Item::Assistant(s) if s == "done.")));
    }

    #[test]
    fn upsert_notice_replaces_the_previous_matching_block() {
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, HashMap::new(), test_sbx());
        m.upsert_notice("background tasks", "background tasks:\n  t1 [running] find".into());
        m.upsert_notice("background tasks", "background tasks:\n  t1 [done] find".into());
        let notices: Vec<&Item> = m.items.iter().map(|e| &e.item).collect();
        assert_eq!(notices.len(), 1, "re-running /tasks must refresh, not stack");
        assert!(matches!(notices[0], Item::Notice(n) if n.contains("[done]")));
        // A different item in between → a fresh block is appended, not merged.
        m.push_item(Item::User("hi".into()));
        m.upsert_notice("background tasks", "background tasks: none".into());
        assert_eq!(m.items.len(), 3);
    }

    #[test]
    fn streaming_incremental_render_matches_full_render() {
        // K15: the prefix-cache path must produce byte-identical rows to a
        // from-scratch render at every step of a simulated stream.
        let cats = HashMap::new();
        let cx = RenderCtx { width: 12, full_transparency: false, show_thinking: true, show_summary: false, viz: &cats };
        let mut streamed = Entry::new(Item::Assistant(String::new()));
        let mut acc = String::new();
        for delta in ["- [x] do", "ne\nnow a long", " line that wraps\n", "tail"] {
            acc.push_str(delta);
            if let Item::Assistant(buf) = &mut streamed.item { buf.push_str(delta); }
            streamed.invalidate();
            streamed.ensure(&cx, 12);
            let mut fresh = Entry::new(Item::Assistant(acc.clone()));
            fresh.ensure(&cx, 12);
            let flat = |e: &Entry| -> Vec<String> {
                e.lines.as_ref().unwrap().iter().map(text).collect()
            };
            assert_eq!(flat(&streamed), flat(&fresh), "diverged after {acc:?}");
            assert_eq!(streamed.height, fresh.height);
        }
        // A width change invalidates the cached prefix (no stale-width rows).
        streamed.invalidate();
        streamed.ensure(&cx, 7);
        let mut fresh = Entry::new(Item::Assistant(acc.clone()));
        fresh.ensure(&cx, 7);
        assert_eq!(streamed.height, fresh.height, "cache must not survive a width change");
    }

    #[test]
    fn wrap_line_measures_terminal_cells_not_chars() {
        use unicode_width::UnicodeWidthStr;
        // K14: 4 CJK chars = 8 cells. At width 4 that's 2 rows of 2 chars each —
        // char-counting would cram all 4 into one row and clip the terminal.
        let rows = wrap_line(&Line::from("你好世界"), 4);
        let texts: Vec<String> = rows.iter().map(text).collect();
        assert_eq!(texts, vec!["你好", "世界"]);
        assert!(texts.iter().all(|t| UnicodeWidthStr::width(t.as_str()) <= 4));
        // Cursor/layout side: emoji before the cursor offsets it by 2 cells.
        let (rows, crow, ccol) = view::layout_input("🙂ab", 3, 80);
        assert_eq!(rows, vec!["🙂ab".to_string()]);
        assert_eq!((crow, ccol), (0, 4), "cursor lands after 2-cell emoji + 2 chars");
    }

    #[test]
    fn entry_height_equals_wrapped_rows() {
        // The virtualization invariant: an item's reported height is exactly the
        // number of physical rows it renders (so scroll math can't drift).
        let cats = HashMap::new();
        let cx = RenderCtx { width: 20, full_transparency: false, show_thinking: true, show_summary: false, viz: &cats };
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
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, cats, test_sbx());
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
        let cx = RenderCtx { width: 80, full_transparency: false, show_thinking: true, show_summary: false, viz: &cats };
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
        push_approval_ex(model, action, false)
    }

    fn push_approval_ex(model: &mut Model, action: &str, escalated: bool) -> oneshot::Receiver<kernel::Approval> {
        let (tx, rx) = oneshot::channel();
        let ev = TuiEvent::Approval(action.to_string(), None, escalated, tx);
        // Drive the same path handle_agent_event uses, minus the generic plumbing.
        match ev {
            TuiEvent::Approval(action, detail, escalated, responder) => {
                if !escalated && model.auto_approve.contains(&action) {
                    let _ = responder.send(kernel::Approval::Once);
                } else {
                    let was_empty = model.pending_approvals.is_empty();
                    model.pending_approvals.push_back(PendingApproval { action, detail, escalated, responder });
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
    fn escalated_approval_is_never_auto_approved() {
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, HashMap::new(), test_sbx());
        // The user previously chose "always" for this exact action.
        m.auto_approve.insert("shell.exec: curl example.com".to_string());

        // A normal request for it auto-approves (no card queued).
        let mut rx = push_approval_ex(&mut m, "shell.exec: curl example.com", false);
        assert!(m.pending_approvals.is_empty(), "remembered action auto-approves");
        assert_eq!(rx.try_recv(), Ok(kernel::Approval::Once));

        // The SAME action, but trust-flow ESCALATED (web-tainted), must NOT be
        // auto-approved — it queues a fresh card for review (K9).
        let _rx2 = push_approval_ex(&mut m, "shell.exec: curl example.com", true);
        assert_eq!(m.pending_approvals.len(), 1, "escalated action is always re-asked, never waved through");
    }

    #[test]
    fn concurrent_approvals_queue_without_dropping_responders() {
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, HashMap::new(), test_sbx());

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
        let mut m = Model::new("m".into(), None, kernel::ReasoningConfig::default(), ui, HashMap::new(), test_sbx());
        let _rx = push_approval(&mut m, "Read access to /outside/a");
        m.approval_ready = true;
        handle_approval_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(m.pending_approvals.len(), 1, "Esc must not consume a pending approval");
    }
}