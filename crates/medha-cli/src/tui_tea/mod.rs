//! TEA-based TUI using ratatui::run — The Elm Architecture implementation
//!
//! Model (app state) → Update(model, message) → model → View(model) → frame
//! View is a pure function of model — same state always renders identically.
//! Message-passing, not shared mutable state.

use crate::config;
use crossterm::event::{
    Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use futures::StreamExt;
use kernel::{Budget, EventLog, Kernel, Message, Provider, Session, StopReason, ToolCategory};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use sandbox::WorkspaceSandbox;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

mod tty;
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
    (
        "/reasoning",
        "configure mode, visibility, effort, and inspect delivery",
    ),
    (
        "/model",
        "switch models, or /model <name>; add presets/custom endpoints and keys",
    ),
    (
        "/search",
        "choose the web-search provider (Tavily/Brave/SearXNG) & key; DuckDuckGo fallback",
    ),
    (
        "/mode",
        "autonomy: how much runs without asking (careful · normal · yolo)",
    ),
    ("/detail", "expand/collapse full tool input & output"),
    ("/resume", "switch to a past session"),
    (
        "/rewind",
        "time-travel: branch from an earlier turn (undoes later edits)",
    ),
    ("/tasks", "list background shell tasks (running/finished)"),
    (
        "/skills",
        "list installed skills (scope, availability, shadowing)",
    ),
    (
        "/skill",
        "pick a skill to load into context (or /skill <name>)",
    ),
    (
        "/skill install",
        "install a complete skill folder from disk, GitHub, or raw SKILL.md",
    ),
    (
        "/skill sources",
        "list or edit skill sources (add/remove a GitHub repo to search)",
    ),
    (
        "/skill search",
        "search registered sources for installable skills",
    ),
    (
        "/skill update",
        "check (or apply with a name / --all) updates to installed skills",
    ),
    (
        "/skill lock",
        "write the skills lockfile for reproducible team setups",
    ),
    (
        "/skill sync",
        "install/repair skills to match the committed lockfile",
    ),
    (
        "/skill info",
        "inspect a skill's files, scope, tools, and source",
    ),
    (
        "/skill remove",
        "remove an installed user skill (with confirmation)",
    ),
    ("/clear", "reset the conversation"),
    ("/exit", "quit (also Ctrl-D)"),
];

/// Providers used by the interactive TUI must be able to atomically apply a
/// saved profile between turns. The kernel remains provider-neutral; this small
/// surface exists only because the TUI owns the explicit user action.
pub(crate) trait ProfileProvider: Provider {
    fn switch_profile(&self, profile: &config::Resolved);
}

impl ProfileProvider for providers::OpenAiCompat {
    fn switch_profile(&self, profile: &config::Resolved) {
        self.switch_connection_with_context(
            profile.base_url.clone(),
            profile.api_key.clone(),
            profile.model.clone(),
            profile.max_ctx,
        );
    }
}

/// Accepted for compatibility, but intentionally omitted from autocomplete and
/// `/help`: one visible `/reasoning` surface replaces these overlapping names.
const LEGACY_REASONING_COMMANDS: &[&str] = &["/think", "/thinking", "/effort"];

fn command_matches(input: &str) -> Vec<(&'static str, &'static str)> {
    COMMANDS
        .iter()
        .filter(|(c, _)| c.starts_with(input))
        .copied()
        .collect()
}

/// A line is a slash command only when its FIRST TOKEN is a known command.
/// Anything else starting with `/` — a pasted absolute path, "/Users/… open
/// this" — is chat for the model, not an "unknown command" error.
fn is_slash_command(line: &str) -> bool {
    match line.split_whitespace().next() {
        Some(tok) => {
            COMMANDS.iter().any(|(c, _)| *c == tok) || LEGACY_REASONING_COMMANDS.contains(&tok)
        }
        None => false,
    }
}

/// Channel shared by sink (agent → UI events) and human gate (agent → UI approval requests)
pub(crate) fn channel() -> (
    mpsc::UnboundedSender<TuiEvent>,
    mpsc::UnboundedReceiver<TuiEvent>,
) {
    mpsc::unbounded_channel()
}

/// Human gate for TUI: approval request sent as TuiEvent with oneshot responder
pub(crate) struct TuiGate {
    pub(crate) tx: mpsc::UnboundedSender<TuiEvent>,
}

#[async_trait::async_trait]
impl kernel::HumanGate for TuiGate {
    async fn confirm(
        &self,
        action: &str,
        detail: Option<&str>,
        escalated: bool,
    ) -> kernel::Approval {
        let (resp_tx, resp_rx) = oneshot::channel();
        let req = TuiEvent::Approval(
            action.to_string(),
            detail.map(str::to_string),
            escalated,
            resp_tx,
        );
        if self.tx.send(req).is_err() {
            return kernel::Approval::Deny;
        }
        resp_rx.await.unwrap_or(kernel::Approval::Deny)
    }
}

/// Question-asker for the TUI: the `clarify` tool's questions are sent as a
/// `TuiEvent` with a oneshot responder, mirroring `TuiGate`. `None` back = the
/// user dismissed the form or the channel closed.
pub(crate) struct TuiAsker {
    pub(crate) tx: mpsc::UnboundedSender<TuiEvent>,
}

#[async_trait::async_trait]
impl kernel::Asker for TuiAsker {
    async fn ask(&self, questions: Vec<kernel::Question>) -> Option<Vec<kernel::Answer>> {
        let (resp_tx, resp_rx) = oneshot::channel();
        if self.tx.send(TuiEvent::Clarify(questions, resp_tx)).is_err() {
            return None;
        }
        resp_rx.await.ok().flatten()
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
    Approval(
        String,
        Option<String>,
        bool,
        oneshot::Sender<kernel::Approval>,
    ),
    /// `clarify` tool: ask the user structured questions, reply with their
    /// answers (or `None` if dismissed).
    Clarify(
        Vec<kernel::Question>,
        oneshot::Sender<Option<Vec<kernel::Answer>>>,
    ),
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
    /// `/skill install <src>` finished with a complete package report.
    SkillInstalled(Result<tools::InstallReport, String>),
    /// `/skill search <query>` finished querying the registered sources.
    SkillSearchResults(Result<tools::SearchResults, String>),
    /// `/skill update` finished checking (and possibly applying) updates; the
    /// lines are ready-to-show status/outcome rows.
    SkillUpdateReport(Vec<String>),
    /// Model setup queried the endpoint's `/v1/models`; `Err` carries the
    /// reason so the form can fall back to manual model-id entry honestly.
    /// Tagged with the queried base URL so a slow reply for an abandoned
    /// draft can never populate a newer draft for a different endpoint.
    ModelsDiscovered {
        base_url: String,
        result: Result<Vec<providers::openai_compat::ModelInfo>, String>,
    },
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
        matches!(
            self,
            RewindScope::Conversation | RewindScope::ConversationAndCode
        )
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
        let plural = if self.files == 1 {
            "tracked file"
        } else {
            "tracked files"
        };
        let mut opts = Vec::new();
        if self.files > 0 {
            opts.push((
                format!(
                    "⏪ restore code + conversation — roll back {} {plural}",
                    self.files
                ),
                Some(RewindScope::ConversationAndCode),
            ));
        }
        opts.push((
            "↩ restore conversation only — keep current files".to_string(),
            Some(RewindScope::Conversation),
        ));
        if self.files > 0 {
            opts.push((
                format!(
                    "⟲ restore code only — keep conversation, roll back {} {plural}",
                    self.files
                ),
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
    ToolCall {
        tool: String,
        args: serde_json::Value,
    },
    ToolResult {
        tool: String,
        ok: bool,
        payload: serde_json::Value,
    },
    Compaction {
        before: u32,
        after: u32,
        summarized: bool,
        summary: Option<String>,
    },
    Verify {
        ok: bool,
        summary: String,
    },
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
        Self {
            item,
            lines: None,
            height: 0,
            stream_cache: None,
        }
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
                self.stream_cache = Some(StreamCache {
                    prefix_bytes: split,
                    width,
                    rows,
                });
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

/// The user's in-progress answer to one clarify question.
#[derive(Default)]
struct ClarifyDraft {
    /// Chosen option indices (one for radio, any for checkbox).
    selected: Vec<usize>,
    /// Free-text entered via the "Other" row.
    other: Option<String>,
}

/// An in-flight `clarify` form: the questions, per-question drafts, and the
/// responder the `clarify` tool is awaiting. Owns all keyboard input while up.
struct ClarifyState {
    questions: Vec<kernel::Question>,
    /// Which question is on screen.
    idx: usize,
    /// One draft per question (same length/order as `questions`).
    drafts: Vec<ClarifyDraft>,
    /// Highlighted row in the current question (options, then Other).
    cursor: usize,
    /// True while the free-text "Other" input owns keys.
    entering_other: bool,
    /// Dedicated free-text editor buffer. Keeping this inside the form means a
    /// clarify request can never overwrite a message the user was already typing
    /// in the main composer while the agent was running.
    other_input: String,
    /// UTF-8 byte offset into `other_input`, always on a character boundary.
    other_cursor: usize,
    /// Inline validation feedback (for example, an unanswered radio question).
    validation: Option<String>,
    responder: oneshot::Sender<Option<Vec<kernel::Answer>>>,
}

impl ClarifyState {
    /// Row count for the current question: options + the "Other" row. Navigation
    /// between questions is ←→; Enter submits ALL answers, so there is no
    /// per-question "continue" row.
    fn row_count(&self) -> usize {
        self.questions[self.idx].options.len() + 1
    }
    fn other_row(&self) -> usize {
        self.questions[self.idx].options.len()
    }
    /// Build the final answers from the drafts (option indices → labels + other).
    fn answers(&self) -> Vec<kernel::Answer> {
        self.questions
            .iter()
            .zip(self.drafts.iter())
            .map(|(q, d)| kernel::Answer {
                selected: d
                    .selected
                    .iter()
                    .filter_map(|&i| q.options.get(i))
                    .map(|o| o.label.clone())
                    .collect(),
                other: d.other.clone(),
            })
            .collect()
    }
}

#[derive(Clone)]
struct ReasoningPanelState {
    enabled: Option<bool>,
    show: bool,
    effort: Option<kernel::ReasoningEffort>,
    last_turn_received: Option<bool>,
}

impl ReasoningPanelState {
    fn from_model(model: &Model) -> Self {
        Self {
            enabled: model.reasoning.enabled,
            show: model.show_thinking,
            effort: model.reasoning.effort,
            last_turn_received: model.last_turn_reasoning_received,
        }
    }

    fn mode_label(&self) -> &'static str {
        match self.enabled {
            Some(true) => "On",
            Some(false) => "Off",
            None => "Server default",
        }
    }

    fn visibility_label(&self) -> &'static str {
        if self.show { "Shown" } else { "Hidden" }
    }

    fn effort_label(&self) -> &'static str {
        match self.effort {
            Some(kernel::ReasoningEffort::Low) => "Low",
            Some(kernel::ReasoningEffort::Medium) => "Medium",
            Some(kernel::ReasoningEffort::High) => "High",
            None => "Auto",
        }
    }

    fn last_turn_label(&self) -> &'static str {
        match self.last_turn_received {
            Some(true) => "Reasoning received",
            Some(false) => "No reasoning received",
            None => "No completed turn yet",
        }
    }

    fn labels(&self) -> Vec<String> {
        vec![
            format!("Mode:       {}", self.mode_label()),
            format!("Visibility: {}", self.visibility_label()),
            format!("Effort:     {}", self.effort_label()),
            format!("Last turn:  {}", self.last_turn_label()),
        ]
    }

    fn status_block(&self) -> String {
        format!(
            "reasoning\n  Mode:       {}\n  Visibility: {}\n  Effort:     {}\n  Last turn:  {}",
            self.mode_label(),
            self.visibility_label(),
            self.effort_label(),
            self.last_turn_label()
        )
    }
}

/// Reasoning control / session picker kind. Not `Copy` — some variants own data.
#[derive(Clone)]
enum PickerKind {
    Reasoning(ReasoningPanelState),
    /// Browse past sessions to resume. Holds the list from `log.sessions()`.
    Session(Vec<kernel::SessionMeta>),
    /// Time-travel cut points in the current session. Holds the list from
    /// `log.events()`, one entry per past user turn.
    Rewind(Vec<RewindPoint>),
    /// Step 2 of `/rewind`: having chosen a cut point, pick the scope
    /// (conversation only · conversation + code · cancel).
    RewindMode(RewindPoint),
    /// `/skill` with no name: pick an installed skill to force-load. Holds
    /// (name, description) for each effective skill.
    Skill(Vec<(String, String)>),
    /// Destructive user-skill removal always gets explicit confirmation.
    RemoveSkill(String),
    /// Provider presets shared with first-run setup, followed by Custom.
    ProviderPreset,
    /// Models the endpoint reported during setup — pick one instead of typing
    /// an id blind. A trailing row keeps manual entry available.
    ModelDiscovery(Vec<providers::openai_compat::ModelInfo>),
    /// Saved provider/model profiles. Switching is available only while idle,
    /// so a stream can never be retargeted midway through a response.
    /// `active` is the profile driving THIS session (may differ from the
    /// startup default) — it gets the ✓ mark and the initial cursor.
    Model {
        profiles: Vec<config::ModelProfile>,
        active: String,
    },
    /// Pick a saved profile whose key should be added or replaced.
    ModelCredential(Vec<config::ModelProfile>),
    /// Pick which saved profile becomes the startup default.
    ModelDefault(Vec<config::ModelProfile>),
    /// Pick a saved profile to remove before entering confirmation.
    ModelRemove(Vec<config::ModelProfile>),
    /// Destructive profile removal always gets a second, explicit confirmation.
    RemoveModel(String),
    /// `/search` step 1: pick the web-search backend. Rows are the entries of
    /// [`SEARCH_PROVIDERS`]; Tavily/Brave continue to a key, SearXNG to a URL,
    /// DuckDuckGo finishes immediately.
    SearchProvider,
    /// `/mode`: pick the autonomy dial. Rows are [`AUTONOMY_MODES`]; choosing one
    /// sets it live for the session.
    AutonomyMode,
    /// `/skill search` results: pick one to install. Holds the ranked hits;
    /// Enter installs the selected one through the guard-gated installer.
    SkillSearch(Vec<tools::SkillHit>),
}

/// The autonomy levels offered by the `/mode` picker, with self-explanatory
/// descriptions (the picker shows these verbatim). Order = increasing autonomy.
const AUTONOMY_MODES: &[(kernel::AutonomyLevel, &str)] = &[
    (
        kernel::AutonomyLevel::Careful,
        "careful — ask before every edit and shell command (safest)",
    ),
    (
        kernel::AutonomyLevel::Normal,
        "normal — auto-apply edits; still ask before shell commands",
    ),
    (
        kernel::AutonomyLevel::Yolo,
        "yolo — auto-apply edits AND shell; only dangerous ops (rm -rf, personal files, deploys) still ask",
    ),
];

/// Providers offered by the `/search` picker, in display order. DuckDuckGo
/// first: it needs no key and is the safe, always-available default.
const SEARCH_PROVIDERS: &[(tools::SearchProvider, &str)] = &[
    (
        tools::SearchProvider::DuckDuckGo,
        "DuckDuckGo — free, no key (default fallback)",
    ),
    (
        tools::SearchProvider::Tavily,
        "Tavily — LLM-optimized API (needs API key)",
    ),
    (
        tools::SearchProvider::Brave,
        "Brave — Search API (needs API key)",
    ),
    (
        tools::SearchProvider::Searxng,
        "SearXNG — your self-hosted instance (needs URL)",
    ),
];

impl PickerKind {
    fn title(&self) -> String {
        match self {
            PickerKind::Reasoning(_) => " reasoning — ↑↓ select, Enter change, Esc done ".into(),
            PickerKind::Session(_) => {
                " resume a session — ↑↓ select, Enter open, Esc cancel ".into()
            }
            PickerKind::Rewind(_) => {
                " rewind to a turn — ↑↓ select, Enter choose, Esc cancel ".into()
            }
            PickerKind::RewindMode(p) => {
                format!(
                    " rewind → “{}” — ↑↓ select, Enter apply, Esc back ",
                    p.label
                )
            }
            PickerKind::Skill(_) => " load a skill — ↑↓ select, Enter load, Esc cancel ".into(),
            PickerKind::RemoveSkill(name) => {
                format!(" remove user skill '{name}'? — ↑↓ move · Enter confirm · Esc back ")
            }
            PickerKind::ProviderPreset => {
                " choose provider — ↑↓ move · Enter/→ continue · Esc/← back ".into()
            }
            PickerKind::ModelDiscovery(_) => {
                " choose a model — ↑↓ move · Enter/→ select · Esc/← back ".into()
            }
            PickerKind::Model { .. } => {
                " model — ↑↓ move · Enter/→ switch or open · Esc close ".into()
            }
            PickerKind::ModelCredential(_) => {
                " update API key — ↑↓ move · Enter/→ continue · Esc/← back ".into()
            }
            PickerKind::ModelDefault(_) => {
                " choose default model — ↑↓ move · Enter/→ save · Esc/← back ".into()
            }
            PickerKind::ModelRemove(_) => {
                " choose model to remove — ↑↓ move · Enter/→ continue · Esc/← back ".into()
            }
            PickerKind::RemoveModel(name) => {
                format!(" remove '{name}'? — ↑↓ move · Enter/→ confirm · Esc/← back ")
            }
            PickerKind::SearchProvider => {
                " web search — ↑↓ move · Enter/→ choose · Esc/← cancel ".into()
            }
            PickerKind::SkillSearch(_) => {
            " install a skill — ↑↓ select, Enter install, Esc cancel ".into()
        }
        PickerKind::AutonomyMode => {
                " autonomy — ↑↓ move · Enter/→ choose · Esc/← cancel ".into()
            }
        }
    }
    /// Dynamic labels for each row. For `Session`, each row is a one-line
    /// summary (date · events · title) matching the `--sessions` headless format.
    fn labels(&self) -> Vec<String> {
        match self {
            PickerKind::Reasoning(state) => state.labels(),
            PickerKind::Session(sessions) => sessions
                .iter()
                .map(|s| {
                    let when = chrono::DateTime::from_timestamp(s.last_ts as i64, 0)
                        .map(|d| {
                            d.with_timezone(&chrono::Local)
                                .format("%m-%d %H:%M")
                                .to_string()
                        })
                        .unwrap_or_else(|| "?".into());
                    let title = if s.title.is_empty() {
                        "(no messages)"
                    } else {
                        &s.title
                    };
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
            PickerKind::Skill(skills) => skills
                .iter()
                .map(|(n, d)| format!("{n} — {d}"))
                .chain(std::iter::once(
                    "⇩ Install a skill — local folder, GitHub folder, or raw SKILL.md".to_string(),
                ))
                .collect(),
            PickerKind::RemoveSkill(_) => {
                vec!["Keep skill".to_string(), "Remove user skill".to_string()]
            }
            PickerKind::ProviderPreset => config::provider_presets()
                .iter()
                .enumerate()
                .map(|(i, (name, url))| format!("{}. {name} — {url}", i + 1))
                .chain(std::iter::once(format!(
                    "{}. Custom — enter your own base URL",
                    config::provider_presets().len() + 1
                )))
                .collect(),
            PickerKind::ModelDiscovery(models) => models
                .iter()
                .map(|m| match m.context_length {
                    Some(c) => format!("{} · {c} ctx", m.id),
                    None => m.id.clone(),
                })
                .chain(std::iter::once("Type a model id manually…".to_string()))
                .collect(),
            PickerKind::Model { profiles, active } => {
                // Models first (Enter switches immediately — the common case,
                // as in other agent CLIs); management actions follow below.
                let remove = if profiles.iter().any(|p| p.name != *active) {
                    "− Remove a saved model"
                } else {
                    "− Remove a saved model · unavailable (only the active model is saved)"
                };
                profiles
                    .iter()
                    .map(|p| {
                        let mark = if p.name == *active { "✓ " } else { "  " };
                        let startup = if p.is_default {
                            " · startup default"
                        } else {
                            ""
                        };
                        let ctx = p
                            .provider
                            .max_ctx
                            .map(|n| format!(" · {n} ctx"))
                            .unwrap_or_default();
                        format!(
                            "{}{} — {} · {}{}{}",
                            mark, p.name, p.provider.model, p.provider.base_url, ctx, startup
                        )
                    })
                    .chain(
                        [
                            "＋ Add a model (presets or custom URL + API key)".to_string(),
                            "🔑 Add or update an API key".to_string(),
                            "★ Set the default model".to_string(),
                            remove.to_string(),
                        ]
                        .map(|s| format!("  {s}")),
                    )
                    .collect()
            }
            PickerKind::ModelCredential(profiles) => profiles
                .iter()
                .map(|p| {
                    format!(
                        "{} — {} · {}",
                        p.name, p.provider.model, p.provider.base_url
                    )
                })
                .collect(),
            PickerKind::ModelDefault(profiles) => profiles
                .iter()
                .map(|p| {
                    let current = if p.is_default {
                        " · current default"
                    } else {
                        ""
                    };
                    format!("{} — {}{}", p.name, p.provider.model, current)
                })
                .collect(),
            PickerKind::ModelRemove(profiles) => profiles
                .iter()
                .map(|p| format!("{} — {}", p.name, p.provider.model))
                .collect(),
            PickerKind::RemoveModel(name) => vec![
                "Cancel".to_string(),
                format!("Remove '{name}' from saved models"),
            ],
            PickerKind::SearchProvider => SEARCH_PROVIDERS
                .iter()
                .map(|(_, desc)| (*desc).to_string())
                .collect(),
            PickerKind::AutonomyMode => AUTONOMY_MODES
                .iter()
                .map(|(_, desc)| (*desc).to_string())
                .collect(),
            PickerKind::SkillSearch(hits) => hits
                .iter()
                .map(|h| {
                    let desc: String = h.description.chars().take(72).collect();
                    format!("{} v{} · {} — {desc}", h.name, h.version, h.repo)
                })
                .collect(),
        }
    }
}

/// Arrow-key picker state
struct Picker {
    kind: PickerKind,
    selected: usize,
}

/// Stages in the guided `/model add` flow. A form rather than a shell command
/// keeps secrets out of terminal history and makes all required connection
/// details discoverable for first-time users. There is deliberately no
/// "profile name" question — the name is derived from the chosen model id
/// (users kept pasting the model id into it).
#[derive(Clone, Copy)]
enum ModelSetupStep {
    BaseUrl,
    ApiKey,
    /// `/v1/models` is being queried in the background; Enter is inert until
    /// the result arrives (picker on success, manual ModelId on failure).
    Discovering,
    ModelId,
    ContextWindow,
}

struct ModelSetup {
    mode: ModelSetupMode,
    step: ModelSetupStep,
    base_url: String,
    api_key: String,
    model: String,
    /// Context window carried over from discovery, when the server reports it.
    max_ctx: Option<u32>,
}

enum ModelSetupMode {
    Add,
    UpdateKey { profile: String },
}

impl ModelSetup {
    fn new() -> Self {
        Self {
            mode: ModelSetupMode::Add,
            step: ModelSetupStep::BaseUrl,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            max_ctx: None,
        }
    }

    fn update_key(profile: String, base_url: String) -> Self {
        Self {
            mode: ModelSetupMode::UpdateKey { profile },
            step: ModelSetupStep::ApiKey,
            base_url,
            api_key: String::new(),
            model: String::new(),
            max_ctx: None,
        }
    }

    fn prompt(&self) -> &'static str {
        match self.step {
            ModelSetupStep::BaseUrl => {
                "OpenAI-compatible base URL (for example http://localhost:11434/v1):"
            }
            ModelSetupStep::ApiKey => match &self.mode {
                ModelSetupMode::Add => {
                    "API key (leave blank for a local server; stored securely, never in config.toml):"
                }
                ModelSetupMode::UpdateKey { .. } => {
                    "New API key (stored securely, never in config.toml):"
                }
            },
            ModelSetupStep::Discovering => "Querying the server for its models… (Esc cancels)",
            ModelSetupStep::ModelId => "Model ID (as the server names it):",
            ModelSetupStep::ContextWindow => {
                "Context window in tokens (optional; blank = unknown):"
            }
        }
    }

    fn is_secret(&self) -> bool {
        matches!(self.step, ModelSetupStep::ApiKey)
    }
}

/// Stages in the guided `/search` flow. Like `/model add`, a form (not a shell
/// command) keeps the API key out of terminal history and the transcript.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchSetupStep {
    /// The provider picker is open (owned by the generic picker handler).
    Provider,
    /// Entering the secret: an API key (Tavily/Brave, masked) or the instance
    /// URL (SearXNG, not masked).
    Secret,
}

/// In-flight `/search` draft. `provider` is set once the picker is confirmed.
struct SearchSetup {
    provider: tools::SearchProvider,
    step: SearchSetupStep,
}

impl SearchSetup {
    fn new() -> Self {
        // DuckDuckGo is a placeholder until the picker sets the real choice.
        Self {
            provider: tools::SearchProvider::DuckDuckGo,
            step: SearchSetupStep::Provider,
        }
    }

    fn prompt(&self) -> &'static str {
        match self.provider {
            tools::SearchProvider::Tavily | tools::SearchProvider::Brave => {
                "API key (stored securely, never in config.toml):"
            }
            tools::SearchProvider::Searxng => {
                "SearXNG instance base URL (for example https://searx.example.com):"
            }
            tools::SearchProvider::DuckDuckGo => "",
        }
    }

    /// True while entering a value that must be masked — an API key, not a URL.
    fn is_secret(&self) -> bool {
        self.step == SearchSetupStep::Secret
            && matches!(
                self.provider,
                tools::SearchProvider::Tavily | tools::SearchProvider::Brave
            )
    }
}

impl Picker {
    fn new(kind: PickerKind) -> Self {
        Self { kind, selected: 0 }
    }

    /// Open with the cursor on a specific row (e.g. the active model), so
    /// Enter with no navigation is a no-surprise confirm of the status quo.
    fn with_selected(kind: PickerKind, selected: usize) -> Self {
        Self { kind, selected }
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
    /// The saved profile currently active for this session. This is distinct
    /// from `model`, which is the provider's model id and may be duplicated
    /// across endpoints.
    active_profile: String,
    /// Persistent model profiles, shared with the main entrypoint so a TUI add
    /// is immediately available to this and future sessions.
    model_config: Arc<Mutex<config::Config>>,
    /// `/model add` is a guided flow. Its API-key field is masked and never
    /// enters history, the transcript, or config.toml.
    model_setup: Option<ModelSetup>,
    /// Live web-search settings shared with the `web.*` tools. `/search` writes
    /// it so a provider change takes effect on the next search without restart.
    search: tools::SearchHandle,
    /// `/search` is a guided flow like `/model add`; its key field is masked.
    search_setup: Option<SearchSetup>,
    /// Autonomy dial (`/mode`): how much runs without asking. Applied to the
    /// session at the start of each turn; the safety floor is level-independent.
    autonomy: kernel::AutonomyLevel,
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
    /// In-flight `clarify` question form (owns input while `Some`).
    clarify: Option<ClarifyState>,
    /// Selected approval option (0=Yes, 1=Yes-all, 2=No)
    approval_sel: usize,
    /// Auto-approved tool classes for session
    auto_approve: std::collections::HashSet<String>,
    /// Current reasoning config
    reasoning: kernel::ReasoningConfig,
    /// Whether any reasoning delta arrived during the active turn.
    reasoning_received_this_turn: bool,
    /// Delivery result for the most recently completed turn. `None` means this
    /// TUI has not completed a turn yet (resumed history does not retain it).
    last_turn_reasoning_received: Option<bool>,
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
    /// Skill store + the session's registered tool names, so `/skills` can
    /// re-scan live. `None` in tests / when skills aren't wired.
    skills: Option<Arc<tools::SkillStore>>,
    known_tools: Arc<std::collections::HashSet<String>>,
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
            active_profile: String::new(),
            model_config: Arc::new(Mutex::new(config::Config::default())),
            model_setup: None,
            search: Arc::new(Mutex::new(tools::SearchSettings::default())),
            search_setup: None,
            autonomy: kernel::AutonomyLevel::Careful,
            running: false,
            current_tool: None,
            turn_started: None,
            interrupt: None,
            cancelling: false,
            pending_approvals: VecDeque::new(),
            clarify: None,
            approval_sel: 0,
            auto_approve: std::collections::HashSet::new(),
            reasoning,
            reasoning_received_this_turn: false,
            last_turn_reasoning_received: None,
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
            skills: None,
            known_tools: Arc::new(std::collections::HashSet::new()),
        }
    }

    /// Wire the skill store (project + user dirs) and the session's tool names so
    /// `/skills` can list them. Set once in `run_tea`.
    fn with_skills(
        mut self,
        store: Arc<tools::SkillStore>,
        known_tools: std::collections::HashSet<String>,
    ) -> Self {
        self.skills = Some(store);
        self.known_tools = Arc::new(known_tools);
        self
    }

    fn with_model_profiles(
        mut self,
        profiles: Arc<Mutex<config::Config>>,
        active_profile: String,
    ) -> Self {
        self.model_config = profiles;
        self.active_profile = active_profile;
        self
    }

    /// Share the tools' live search-settings handle so `/search` updates the
    /// same settings the running `web.*` tools read.
    fn with_search(mut self, search: tools::SearchHandle) -> Self {
        self.search = search;
        self
    }

    /// Refresh the skills manifest inside the system message (transcript[0]) so a
    /// skill saved or edited mid-session shows up in the model's list on the very
    /// next turn — the manifest is otherwise built once at startup. Strips the
    /// old "## Skills available" section (stable marker) and re-appends a fresh
    /// one; no-op when skills aren't wired or transcript[0] isn't the system msg.
    fn refresh_skill_manifest(&self, transcript: &mut [Message]) {
        let Some(store) = &self.skills else { return };
        let Some(sys) = transcript.first_mut() else {
            return;
        };
        if sys.role != kernel::Role::System {
            return;
        }
        const MARKER: &str = "## Skills available";
        if let Some(idx) = sys.content.find(MARKER) {
            let head = sys.content[..idx].trim_end().to_string();
            sys.content = head;
        }
        let fresh = store.manifest(&self.known_tools, None);
        if !fresh.is_empty() {
            sys.content.push_str("\n\n");
            sys.content.push_str(&fresh);
        }
    }

    /// Render the `/skills` notice: effective skills (scope, availability),
    /// shadowed ones, and parse errors. Mirrors `/tasks`' upsert-in-place block.
    fn skills_notice(&self) -> String {
        let Some(store) = &self.skills else {
            return "skills: unavailable".to_string();
        };
        let disc = store.discover(&self.known_tools);
        if disc.listings.is_empty() && disc.errors.is_empty() {
            return "skills: none installed\n\n(add one under .medha/skills/<name>/SKILL.md, \
                    or ask me to save a procedure as a skill)"
                .to_string();
        }
        let mut out = String::from("skills:");
        for l in disc.effective() {
            let s = &l.skill;
            let avail = if l.available() {
                String::new()
            } else {
                format!("  (unavailable: needs {})", l.missing_tools.join(", "))
            };
            out.push_str(&format!(
                "\n  {} [{}]  {}{}",
                s.name,
                s.scope.as_str(),
                s.description,
                avail
            ));
        }
        for l in disc.listings.iter().filter(|l| l.shadowed) {
            out.push_str(&format!(
                "\n  {} [{}]  — shadowed by project",
                l.skill.name,
                l.skill.scope.as_str()
            ));
        }
        for (path, reason) in &disc.errors {
            out.push_str(&format!("\n  ⚠ {} — {reason}", path.display()));
        }
        out
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
        // Also drop any live clarify form (its tool future is gone once the turn
        // settles) so a stale question can't linger on screen owning input.
        if let Some(state) = self.clarify.take() {
            let _ = state.responder.send(None);
        }
    }

    /// The declared category of a tool (from the executor specs), or `Other` if
    /// the surface hasn't been told about it.
    fn category(&self, tool: &str) -> ToolCategory {
        self.tool_viz
            .get(tool)
            .map(|v| v.category)
            .unwrap_or(ToolCategory::Other)
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
        let next = (self.scroll_offset as i32)
            .saturating_add(delta)
            .clamp(0, max as i32) as usize;
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
        self.reasoning_received_this_turn = true;
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

    fn reasoning_status_block(&self) -> String {
        ReasoningPanelState::from_model(self).status_block()
    }

    fn reasoning_trace_label(&self) -> &'static str {
        if self.running {
            if self.reasoning_received_this_turn {
                "receiving"
            } else {
                "waiting"
            }
        } else {
            match self.last_turn_reasoning_received {
                Some(true) => "received",
                Some(false) => "no trace",
                None => "no turn",
            }
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
    model_profiles: Arc<Mutex<config::Config>>,
    active_profile: String,
    open_setup: bool,
    budget: Budget,
    ui: lockfile::UiConfig,
    resumed: Vec<Message>,
    restore: Arc<WorkspaceSandbox>,
    stray_log: std::path::PathBuf,
    skill_store: Arc<tools::SkillStore>,
    known_tools: std::collections::HashSet<String>,
    search_handle: tools::SearchHandle,
    tx: mpsc::UnboundedSender<TuiEvent>,
    mut rx: mpsc::UnboundedReceiver<TuiEvent>,
) -> anyhow::Result<()>
where
    P: ProfileProvider + 'static,
    L: EventLog + 'static,
{
    // Terminal setup with panic-safe restore hook (PART 0/2). Draws on a private
    // tty handle; a dependency's stray stdout is redirected to `stray_log` so it
    // can't corrupt the alternate screen. See tty.rs.
    let (mut terminal, mut redirect) = tty::init(&stray_log)?;

    // Presentation flows from the tools' declared metadata (glyph + category) —
    // the single source of truth — so adding a tool needs no TUI edit.
    let tool_viz: HashMap<String, ToolViz> = kernel
        .executor
        .specs()
        .into_iter()
        .map(|s| {
            (
                s.name,
                ToolViz {
                    icon: s.icon,
                    category: s.category,
                },
            )
        })
        .collect();
    let mut model = Model::new(
        model_name,
        max_ctx,
        kernel.provider.reasoning(),
        ui,
        tool_viz,
        restore,
    )
    .with_skills(skill_store, known_tools)
    .with_model_profiles(model_profiles, active_profile)
    .with_search(search_handle);
    // Reflect the session's starting autonomy (from lock/MEDHA_MODE) in the TUI.
    model.autonomy = session.autonomy;
    // First run (nothing configured) or explicit `medha --setup`: open the
    // model-setup form immediately — the same surface `/model add` uses. The
    // quiet variant keeps the welcome identity screen visible behind the form.
    if open_setup {
        update::open_model_setup_quiet(&mut model);
    }
    let mut transcript = vec![Message::system(system)];
    transcript.extend(resumed); // prior conversation when resuming (else empty)
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(REDRAW_INTERVAL);
    let mut redraw_needed = true;

    // Initial draw
    terminal.draw(|f| view(f, &mut model)).ok();

    // Async event loop driven directly on the current runtime (PART 0.4).
    loop {
        if model.should_quit {
            break;
        }

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

    // Restore terminal on exit (PART 2): leave the alternate screen on the
    // private handle, then undo the fd 1/2 redirection.
    tty::restore(&mut terminal, &mut redirect);
    Ok(())
}

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

    // ---- mid-session skill manifest refresh ----

    #[test]
    fn refresh_skill_manifest_injects_saved_skill_same_session() {
        let dir = std::env::temp_dir().join(format!("medha-skref-{}", ulid::Ulid::new()));
        let proj = dir.join(".medha").join("skills");
        std::fs::create_dir_all(proj.join("note-taker")).unwrap();
        std::fs::write(
            proj.join("note-taker").join("SKILL.md"),
            "---\nname = \"note-taker\"\ndescription = \"Capture a decision\"\n---\n\nsteps",
        )
        .unwrap();
        let store = Arc::new(tools::SkillStore::new(proj, None));
        let mut known = std::collections::HashSet::new();
        known.insert("fs.write".to_string());
        let model = Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            lockfile::UiConfig::default(),
            HashMap::new(),
            test_sbx(),
        )
        .with_skills(store, known);

        // A system message with no skills section yet (as at startup with none).
        let mut transcript = vec![Message::system("BASE PROMPT")];
        model.refresh_skill_manifest(&mut transcript);
        assert!(transcript[0].content.contains("## Skills available"));
        assert!(transcript[0].content.contains("note-taker"));
        // Idempotent: a second refresh must not stack a duplicate section.
        model.refresh_skill_manifest(&mut transcript);
        assert_eq!(
            transcript[0].content.matches("## Skills available").count(),
            1
        );
        assert!(transcript[0].content.starts_with("BASE PROMPT"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- /rewind scope menu (step 2) ----

    #[test]
    fn rewind_scope_menu_hides_code_options_when_nothing_to_undo() {
        // No file edits from this point on → only "restore conversation" + cancel;
        // offering a code rollback that reverts nothing would be misleading.
        let p = RewindPoint {
            at_event: ulid::Ulid::new(),
            label: "hi".into(),
            files: 0,
        };
        let opts = p.scope_options();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].1, Some(RewindScope::Conversation));
        assert_eq!(opts[1].1, None, "last row is cancel");
        assert!(!opts.iter().any(|(_, s)| matches!(
            s,
            Some(RewindScope::ConversationAndCode | RewindScope::Code)
        )));
    }

    #[test]
    fn rewind_scope_menu_offers_all_three_with_count_when_edits_exist() {
        // Order: code+conversation, conversation, code, then cancel.
        let p = RewindPoint {
            at_event: ulid::Ulid::new(),
            label: "hi".into(),
            files: 3,
        };
        let opts = p.scope_options();
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0].1, Some(RewindScope::ConversationAndCode));
        // "tracked" is deliberate (K18): only snapshot-tracked writes revert.
        assert!(
            opts[0].0.contains("3 tracked files"),
            "count shown: {}",
            opts[0].0
        );
        assert_eq!(opts[1].1, Some(RewindScope::Conversation));
        assert_eq!(opts[2].1, Some(RewindScope::Code));
        assert!(
            opts[2].0.contains("3 tracked files"),
            "count shown: {}",
            opts[2].0
        );
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
        assert!(
            !out.contains('┌') && !out.contains('│') && !out.contains('╭'),
            "approval must not draw a box"
        );
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
        let mut m = Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            ui,
            HashMap::new(),
            test_sbx(),
        );
        // Type "café" then a trailing char — the classic char-vs-byte panic case.
        for c in "café".chars() {
            m.insert_char(c);
        }
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
        assert!(
            rows.iter().any(|r| matches!(r, DiffRow::Gap(n) if *n > 0)),
            "expected a gap marker"
        );
        assert!(
            rows.iter()
                .any(|r| matches!(r, DiffRow::Ins(_, t) if t.contains("CHANGED")))
        );
        // Only context (3) around the change on each side survives, never all 40 lines.
        let ctx = rows
            .iter()
            .filter(|r| matches!(r, DiffRow::Ctx(..)))
            .count();
        assert!(ctx <= 8, "kept too much context: {ctx}");
    }

    #[test]
    fn diff_one_sided_uses_unified_layout() {
        // All-additions (new file) at wide width must NOT go side-by-side (which
        // would waste the whole left column) — single column instead.
        let out = block(&render_diff("", "line a\nline b\nline c", "f.rs", 200));
        assert!(
            !out.contains('│'),
            "one-sided diff should be single-column: {out}"
        );
        assert!(out.contains("line a"));
    }

    #[test]
    fn diff_modification_uses_side_by_side_when_wide() {
        // A real modification (deletion + insertion) at wide width uses side-by-side.
        let out = block(&render_diff(
            "old line\ncommon",
            "new line\ncommon",
            "f.rs",
            200,
        ));
        assert!(
            out.contains('│'),
            "modification should be side-by-side when wide: {out}"
        );
    }

    #[test]
    fn wrap_line_hard_wraps_long_run_and_preserves_text() {
        let line = Line::from(Span::styled("abcdefghij", Style::default().fg(theme::TEXT)));
        let rows = wrap_line(&line, 4);
        assert_eq!(rows.len(), 3, "10 chars / width 4 = 3 rows");
        let joined: String = rows
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(joined, "abcdefghij");
        assert!(rows.iter().all(|r| text(r).chars().count() <= 4));
    }

    #[test]
    fn wrap_line_breaks_at_spaces() {
        let rows = wrap_line(&Line::from("hello world foo"), 8);
        let texts: Vec<String> = rows.iter().map(text).collect();
        assert!(texts.iter().all(|t| t.chars().count() <= 8), "{texts:?}");
        assert!(
            texts.iter().all(|t| !t.starts_with(' ')),
            "breaking space should be dropped: {texts:?}"
        );
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
        assert!(is_slash_command("/reasoning"));
        assert!(is_slash_command("/reasoning effort high"));
        // Legacy names remain accepted even though autocomplete presents only
        // the unified command.
        assert!(is_slash_command("/think on"));
        assert!(is_slash_command("/effort high"));
        // A pasted absolute path is CHAT, not an unknown-command error.
        assert!(!is_slash_command(
            "/Users/x/medha/learn.html open this in browser"
        ));
        assert!(!is_slash_command("/tmp/foo.txt"));
        // Near-miss typo goes to the model too (autocomplete guides while typing).
        assert!(!is_slash_command("/claer"));
        assert!(!is_slash_command("/"));
    }

    #[test]
    fn steer_events_promote_queued_notice_and_return_unsent_text() {
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            ui,
            HashMap::new(),
            test_sbx(),
        );
        let mut session = kernel::Session::new();
        let mut transcript: Vec<Message> = Vec::new();

        // Steer queued in the TUI → notice; kernel applies it → Steered
        // promotes the notice to a real user line (exactly one).
        m.push_notice("↳ queued for this task: check the tests");
        update::handle_agent_event(
            &mut m,
            TuiEvent::Steered("check the tests".into()),
            &mut session,
            &mut transcript,
        );
        assert!(
            !m.items
                .iter()
                .any(|e| matches!(&e.item, Item::Notice(n) if n.starts_with("↳ queued")))
        );
        assert!(
            m.items
                .iter()
                .any(|e| matches!(&e.item, Item::User(s) if s == "check the tests"))
        );

        // Cancel raced a steer → the text lands back in the input box.
        m.push_notice("↳ queued for this task: do Y instead");
        update::handle_agent_event(
            &mut m,
            TuiEvent::SteersReturned(vec!["do Y instead".into()]),
            &mut session,
            &mut transcript,
        );
        assert_eq!(
            m.input, "do Y instead",
            "returned steer must be editable, not lost"
        );
        assert_eq!(m.cursor, m.input.len());
        assert!(
            !m.items
                .iter()
                .any(|e| matches!(&e.item, Item::Notice(n) if n.starts_with("↳ queued")))
        );
    }

    #[test]
    fn text_and_reasoning_stream_into_separate_transcript_items() {
        // The display side of the "answer hidden behind thinking" bug: text
        // deltas must build ONE visible Assistant item; only Reasoning deltas
        // may create a (collapsible) Thinking item.
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            ui,
            HashMap::new(),
            test_sbx(),
        );
        let mut session = kernel::Session::new();
        let mut transcript: Vec<Message> = Vec::new();

        // A pure-text answer that mentions think tags — streamed in deltas.
        for d in [
            "Shape 2: `<think>",
            "` tags) and the rest ",
            "of the answer",
        ] {
            update::handle_agent_event(
                &mut m,
                TuiEvent::Text(d.to_string()),
                &mut session,
                &mut transcript,
            );
        }
        let thinking = m
            .items
            .iter()
            .filter(|e| matches!(e.item, Item::Thinking(_)))
            .count();
        assert_eq!(thinking, 0, "no Thinking item for a text-only answer");
        assert!(
            m.items.iter().any(|e| matches!(&e.item, Item::Assistant(s) if s == "Shape 2: `<think>` tags) and the rest of the answer")),
            "the full answer is one visible Assistant item"
        );

        // Genuine reasoning deltas DO make a Thinking item, answer separate.
        update::handle_agent_event(
            &mut m,
            TuiEvent::Reasoning("planning".into()),
            &mut session,
            &mut transcript,
        );
        update::handle_agent_event(
            &mut m,
            TuiEvent::Text("done.".into()),
            &mut session,
            &mut transcript,
        );
        assert!(
            m.items
                .iter()
                .any(|e| matches!(&e.item, Item::Thinking(s) if s == "planning"))
        );
        assert!(
            m.items
                .iter()
                .any(|e| matches!(&e.item, Item::Assistant(s) if s == "done."))
        );
    }

    // Resume regression: tool RESULTS must replay into the transcript, not
    // just the calls — outputs silently vanished from resumed sessions once.
    #[test]
    fn resumed_history_replays_tool_results_with_their_tool_names() {
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            ui,
            HashMap::new(),
            test_sbx(),
        );
        let msgs = vec![
            Message::user("list files"),
            Message::assistant_calls(
                "checking",
                vec![kernel::ToolIntent {
                    id: "call-1".into(),
                    tool: "fs.list".into(),
                    args: serde_json::json!({"path": "."}),
                }],
            ),
            Message::tool_result("call-1", r#"{"entries": 3}"#),
            Message::tool_result("call-1", r#"{"error": "denied"}"#),
        ];
        update::repaint_history(&mut m, &msgs);
        assert!(
            m.items
                .iter()
                .any(|e| matches!(&e.item, Item::ToolCall { tool, .. } if tool == "fs.list"))
        );
        assert!(
            m.items.iter().any(
                |e| matches!(&e.item, Item::ToolResult { tool, ok: true, .. } if tool == "fs.list")
            ),
            "successful result replays with its tool name"
        );
        assert!(
            m.items
                .iter()
                .any(|e| matches!(&e.item, Item::ToolResult { ok: false, .. })),
            "error payloads replay as failures"
        );
    }

    #[test]
    fn upsert_notice_replaces_the_previous_matching_block() {
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            ui,
            HashMap::new(),
            test_sbx(),
        );
        m.upsert_notice(
            "background tasks",
            "background tasks:\n  t1 [running] find".into(),
        );
        m.upsert_notice(
            "background tasks",
            "background tasks:\n  t1 [done] find".into(),
        );
        let notices: Vec<&Item> = m.items.iter().map(|e| &e.item).collect();
        assert_eq!(
            notices.len(),
            1,
            "re-running /tasks must refresh, not stack"
        );
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
        let cx = RenderCtx {
            width: 12,
            full_transparency: false,
            show_thinking: true,
            show_summary: false,
            viz: &cats,
        };
        let mut streamed = Entry::new(Item::Assistant(String::new()));
        let mut acc = String::new();
        for delta in ["- [x] do", "ne\nnow a long", " line that wraps\n", "tail"] {
            acc.push_str(delta);
            if let Item::Assistant(buf) = &mut streamed.item {
                buf.push_str(delta);
            }
            streamed.invalidate();
            streamed.ensure(&cx, 12);
            let mut fresh = Entry::new(Item::Assistant(acc.clone()));
            fresh.ensure(&cx, 12);
            let flat =
                |e: &Entry| -> Vec<String> { e.lines.as_ref().unwrap().iter().map(text).collect() };
            assert_eq!(flat(&streamed), flat(&fresh), "diverged after {acc:?}");
            assert_eq!(streamed.height, fresh.height);
        }
        // A width change invalidates the cached prefix (no stale-width rows).
        streamed.invalidate();
        streamed.ensure(&cx, 7);
        let mut fresh = Entry::new(Item::Assistant(acc.clone()));
        fresh.ensure(&cx, 7);
        assert_eq!(
            streamed.height, fresh.height,
            "cache must not survive a width change"
        );
    }

    #[test]
    fn wrap_line_measures_terminal_cells_not_chars() {
        use unicode_width::UnicodeWidthStr;
        // K14: 4 CJK chars = 8 cells. At width 4 that's 2 rows of 2 chars each —
        // char-counting would cram all 4 into one row and clip the terminal.
        let rows = wrap_line(&Line::from("你好世界"), 4);
        let texts: Vec<String> = rows.iter().map(text).collect();
        assert_eq!(texts, vec!["你好", "世界"]);
        assert!(
            texts
                .iter()
                .all(|t| UnicodeWidthStr::width(t.as_str()) <= 4)
        );
        // Cursor/layout side: emoji before the cursor offsets it by 2 cells.
        let (rows, crow, ccol) = view::layout_input("🙂ab", 3, 80);
        assert_eq!(rows, vec!["🙂ab".to_string()]);
        assert_eq!(
            (crow, ccol),
            (0, 4),
            "cursor lands after 2-cell emoji + 2 chars"
        );
    }

    #[test]
    fn entry_height_equals_wrapped_rows() {
        // The virtualization invariant: an item's reported height is exactly the
        // number of physical rows it renders (so scroll math can't drift).
        let cats = HashMap::new();
        let cx = RenderCtx {
            width: 20,
            full_transparency: false,
            show_thinking: true,
            show_summary: false,
            viz: &cats,
        };
        let mut e = Entry::new(Item::Assistant(
            "a fairly long line that must wrap across several rows here".into(),
        ));
        e.ensure(&cx, 20);
        assert_eq!(e.height, e.lines.as_ref().unwrap().len());
        assert!(
            e.height > 1,
            "long line should wrap to multiple physical rows"
        );
    }

    #[test]
    fn activity_label_shows_streaming_tool_and_target() {
        let ui = lockfile::UiConfig::default();
        // The surface learns tool presentation from the executor specs; simulate it.
        let viz = |icon: &str, c: ToolCategory| ToolViz {
            icon: icon.into(),
            category: c,
        };
        let cats = HashMap::from([
            ("fs.write".to_string(), viz("✎", ToolCategory::Write)),
            ("fs.read".to_string(), viz("◇", ToolCategory::Read)),
            ("shell.exec".to_string(), viz("❯", ToolCategory::Shell)),
        ]);
        let mut m = Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            ui,
            cats,
            test_sbx(),
        );
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
        let cx = RenderCtx {
            width: 80,
            full_transparency: false,
            show_thinking: true,
            show_summary: false,
            viz: &cats,
        };
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
        let old = (0..30)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut v: Vec<String> = (0..30).map(|i| format!("l{i}")).collect();
        v[15] = "l15!".to_string();
        let new = v.join("\n");
        let out = block(&render_diff(&old, &new, "f.rs", 80));
        assert!(
            out.contains("unchanged line"),
            "should show a collapsed-context marker: {out}"
        );
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
        assert_eq!(
            expand_paste_tokens(&pastes, "before [paste #0: 12 chars] after"),
            "before FULL CONTENT after"
        );
        // A bare bracket that isn't a real token is left untouched.
        assert_eq!(expand_paste_tokens(&pastes, "arr[0] = 1"), "arr[0] = 1");
        // No pastes → identity.
        assert_eq!(
            expand_paste_tokens(&[], "[paste #0: 5 chars]"),
            "[paste #0: 5 chars]"
        );
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

    fn push_approval_ex(
        model: &mut Model,
        action: &str,
        escalated: bool,
    ) -> oneshot::Receiver<kernel::Approval> {
        let (tx, rx) = oneshot::channel();
        let ev = TuiEvent::Approval(action.to_string(), None, escalated, tx);
        // Drive the same path handle_agent_event uses, minus the generic plumbing.
        match ev {
            TuiEvent::Approval(action, detail, escalated, responder) => {
                if !escalated && model.auto_approve.contains(&action) {
                    let _ = responder.send(kernel::Approval::Once);
                } else {
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
        let mut m = Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            ui,
            HashMap::new(),
            test_sbx(),
        );
        // The user previously chose "always" for this exact action.
        m.auto_approve
            .insert("shell.exec: curl example.com".to_string());

        // A normal request for it auto-approves (no card queued).
        let mut rx = push_approval_ex(&mut m, "shell.exec: curl example.com", false);
        assert!(
            m.pending_approvals.is_empty(),
            "remembered action auto-approves"
        );
        assert_eq!(rx.try_recv(), Ok(kernel::Approval::Once));

        // The SAME action, but trust-flow ESCALATED (web-tainted), must NOT be
        // auto-approved — it queues a fresh card for review (K9).
        let _rx2 = push_approval_ex(&mut m, "shell.exec: curl example.com", true);
        assert_eq!(
            m.pending_approvals.len(),
            1,
            "escalated action is always re-asked, never waved through"
        );
    }

    #[test]
    fn concurrent_approvals_queue_without_dropping_responders() {
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            ui,
            HashMap::new(),
            test_sbx(),
        );

        // Two approvals arrive back-to-back (the concurrent-dispatch case).
        let mut rx1 = push_approval(&mut m, "Read access to /outside/a");
        let mut rx2 = push_approval(&mut m, "Read access to /outside/b");

        // Both are queued; the first is the one rendered/answerable.
        assert_eq!(m.pending_approvals.len(), 2);
        assert_eq!(
            m.pending_approval().map(|p| p.action.as_str()),
            Some("Read access to /outside/a")
        );

        // Answer the visible one (Yes, allow once). Its responder must fire.
        m.approval_ready = true;
        handle_approval_key(
            &mut m,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        );
        assert_eq!(rx1.try_recv(), Ok(kernel::Approval::Once));

        // The second approval is now front-and-center — NOT dropped.
        assert_eq!(m.pending_approvals.len(), 1);
        assert_eq!(
            m.pending_approval().map(|p| p.action.as_str()),
            Some("Read access to /outside/b")
        );

        // Answer it too; its responder fires as well (this was the dropped one).
        m.approval_ready = true;
        handle_approval_key(
            &mut m,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        );
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
        assert!(
            !help.contains("esc to reject"),
            "help text still ties Esc to deny: {help}"
        );

        // And at the queue level: routing Esc through handle_approval_key must not
        // pop or answer anything (Esc is handled by the caller, not here).
        let ui = lockfile::UiConfig::default();
        let mut m = Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            ui,
            HashMap::new(),
            test_sbx(),
        );
        let _rx = push_approval(&mut m, "Read access to /outside/a");
        m.approval_ready = true;
        handle_approval_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            m.pending_approvals.len(),
            1,
            "Esc must not consume a pending approval"
        );
    }
}
