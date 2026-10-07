//! TEA-based ratatui interface with message-driven state updates and pure views.

use crate::config;
use crossterm::event::{
    Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use kernel::ToolCategory;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
#[cfg(test)]
use sandbox::WorkspaceSandbox;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod backend_features;
mod backend_plugins;
pub(crate) mod backend_ui;
mod input;
mod markdown;
mod spin;
mod staged_images;
#[cfg(test)]
mod tree_tests;
mod tty;
mod view;
pub(crate) use protocol::AgentStep;
use view::*;

mod termbg;
pub(crate) mod theme;

/// Maximum lines retained in scrollback.
const MAX_SCROLLBACK_LINES: usize = 5000;
const MAX_ITEM_BYTES: usize = 256 * 1024;
const MAX_PANE_BYTES: usize = 4 * 1024 * 1024;
const MAX_AGENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMPOSER_BYTES: usize = 2 * 1024 * 1024;
const MAX_SECRET_BYTES: usize = 64 * 1024;
const PREVIEW_NOTICE: &str = "\n… preview limited; the full content remains in saved history.";
/// Maximum diff lines rendered inline.
const MAX_DIFF_LINES: usize = 60;
/// Maximum raw tool I/O lines rendered per call.
const MAX_TOOL_OUTPUT_LINES: usize = 500;
/// Pastes longer than this collapse to an input placeholder.
const PASTE_COLLAPSE_THRESHOLD: usize = 1000;
/// Redraw interval for 60 fps.
const REDRAW_INTERVAL: Duration = Duration::from_millis(16);

pub(crate) use crate::application_catalog::{COMMANDS, MODEL_PROTOCOLS};

/// Compatibility commands intentionally omitted from autocomplete and help.
const HIDDEN_COMMANDS: &[&str] = &[
    "/reconnect",
    "/paste",
    "/detach",
    "/plan",
    "/steer",
    "/followup",
    "/tree",
    "/think",
    "/thinking",
    "/effort",
    "/skills",
];

/// `(label, action id)` rows shown before installed skills in the skill hub.
pub(super) const SKILL_HUB_ACTIONS: &[(&str, &str)] = &[
    (
        "➕ Add a skill…      search the catalog, or paste a GitHub link",
        "add",
    ),
    (
        "⚙  Manage skills…    updates · sources · lock / sync",
        "manage",
    ),
];

/// `(label, action id)` rows in the skill-management submenu.
pub(super) const SKILL_MANAGE_ACTIONS: &[(&str, &str)] = &[
    ("▸ Check for updates", "update"),
    ("▸ Sources — add or remove repositories", "sources"),
    ("▸ Lock — save this set (for your team)", "lock"),
    ("▸ Sync — restore skills from the lockfile", "sync"),
    ("← Back", "back"),
];

fn command_matches(model: &Model) -> Vec<(String, String)> {
    let input = model.input.as_str();
    let extra: Vec<(String, String)> = model
        .remote
        .as_ref()
        .map(|peer| {
            peer.plugins
                .commands
                .iter()
                .map(|command| (command.name.clone(), command.description.clone()))
                .collect()
        })
        .unwrap_or_default();
    COMMANDS
        .iter()
        .map(|(name, about)| (name.to_string(), about.to_string()))
        .chain(extra)
        .filter(|(name, _)| name.starts_with(input))
        .collect()
}

/// Recognizes only known first-token commands, leaving absolute paths as chat.
fn is_slash_command(line: &str) -> bool {
    match line.split_whitespace().next() {
        Some(tok) => COMMANDS.iter().any(|(c, _)| *c == tok) || HIDDEN_COMMANDS.contains(&tok),
        None => false,
    }
}

/// A user-message boundary offered by `/rewind`; the cut occurs before `at_event`.
/// `files` controls whether code rollback choices are available.
#[derive(Clone, Debug)]
pub(crate) struct RewindPoint {
    pub at_event: ulid::Ulid,
    pub label: String,
    pub files: usize,
}

/// Rewind scope. Conversation scopes fork and prefill; code-only preserves chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RewindScope {
    /// Rewind the conversation only; leave the working files as they are now.
    Conversation,
    /// Rewind the conversation *and* roll the files back to before the turn.
    ConversationAndCode,
    /// Roll the files back only; keep the conversation intact (no fork/prefill).
    Code,
}

impl RewindPoint {
    /// Builds scope choices, omitting code rollback when no files are tracked.
    fn scope_options(&self) -> Vec<(String, Option<RewindScope>)> {
        // Only snapshot-tracked writes can be rolled back.
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

/// One rendered transcript item.
#[derive(Debug)]
enum Item {
    User(String),
    Assistant(String),
    ToolCall {
        id: Option<String>,
        tool: String,
        args: serde_json::Value,
    },
    ToolResult {
        id: Option<String>,
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
    /// What a fan-out did, written once when the last child settles.
    ///
    /// The live tree is pinned above the composer and vanishes with the agents,
    /// so without this the delegation leaves no trace in the conversation it
    /// served. Collapsed to one line by default and expanded with the same key
    /// as any other collapsed card.
    AgentsDone(Vec<AgentDoneRow>),
}

/// One finished child, as its record reads afterwards.
#[derive(Debug, Clone)]
struct AgentDoneRow {
    name: String,
    status: orchestrator::AgentStatus,
    tool_calls: u32,
    tokens: u64,
    seconds: u64,
}

/// A transcript item with a memoized physical-row render.
struct Entry {
    item: Item,
    bytes: usize,
    lines: Option<Vec<Line<'static>>>,
    height: usize,
}

/// A position in the virtualized transcript. Rows remain stable while the user
/// scrolls; columns are terminal cells rather than bytes/chars so wide glyphs
/// remain selectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TextPoint {
    row: usize,
    column: usize,
}

#[derive(Debug, Clone, Copy)]
struct TextSelection {
    anchor: TextPoint,
    focus: TextPoint,
    non_empty: bool,
}

#[derive(Debug, Clone, Copy)]
struct LastClick {
    point: TextPoint,
    at: Instant,
    count: u8,
}

impl Entry {
    fn new(mut item: Item) -> Self {
        limit_item(&mut item);
        let bytes = item_bytes(&item);
        Self {
            item,
            bytes,
            lines: None,
            height: 0,
        }
    }
    fn invalidate(&mut self) {
        limit_item(&mut self.item);
        self.lines = None;
        self.bytes = item_bytes(&self.item);
    }
    /// Renders and caches physical rows so height and scroll agree. Markdown is
    /// rendered whole, since tables and fences make earlier lines depend on later
    /// content, and throttled so a growing message re-renders once per frame.
    fn ensure(&mut self, cx: &RenderCtx<'_>, width: u16) {
        if self.lines.is_some() {
            return;
        }
        let mut rows: Vec<Line<'static>> = Vec::new();
        for logical in render_item(&self.item, cx) {
            rows.extend(wrap_line(&logical, width as usize));
            if rows.len() >= 1024 {
                rows.truncate(1024);
                rows.push(Line::from(PREVIEW_NOTICE.trim()));
                break;
            }
        }
        self.height = rows.len();
        self.lines = Some(rows);
    }
}

/// Wraps styled text by terminal cells; long words hard-break.
fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    // Terminal-cell widths keep CJK and emoji layout aligned.
    let cell_w = |c: char| c.width().unwrap_or(0);
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        let st = span.style;
        for c in span.content.chars() {
            chars.push((c, st));
        }
    }
    if chars.iter().map(|&(c, _)| cell_w(c)).sum::<usize>() <= width {
        return vec![line.clone()];
    }

    // Prefer the last fitting space before hard-breaking.
    let n = chars.len();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        // Always consume one glyph so a wide glyph cannot stall a narrow row.
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
        if hard_end < n
            && let Some(sp) = (i..hard_end).rev().find(|&k| chars[k].0 == ' ')
            && sp > i
        {
            ranges.push((i, sp)); // drop the breaking space
            i = sp + 1;
            continue;
        }
        ranges.push((i, hard_end));
        i = hard_end;
    }

    // Coalesce adjacent glyphs with the same style.
    let omitted = ranges.len() > 1024;
    let mut rows: Vec<_> = ranges
        .into_iter()
        .take(1024)
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
        .collect();
    if omitted {
        rows.push(Line::from(PREVIEW_NOTICE.trim()));
    }
    rows
}

fn item_bytes(item: &Item) -> usize {
    match item {
        Item::User(s) | Item::Assistant(s) | Item::Thinking(s) | Item::Notice(s) => s.len(),
        Item::ToolCall { id, tool, args } => {
            id.as_ref().map_or(0, String::len) + tool.len() + crate::chat_presentation::size(args)
        }
        Item::ToolResult {
            id, tool, payload, ..
        } => {
            id.as_ref().map_or(0, String::len)
                + tool.len()
                + crate::chat_presentation::size(payload)
        }
        Item::Compaction { summary, .. } => summary.as_ref().map_or(0, String::len),
        Item::Verify { summary, .. } => summary.len(),
        Item::AgentsDone(rows) => rows.iter().map(|row| row.name.len() + 64).sum(),
    }
}

fn append_preview(buffer: &mut String, delta: &str) {
    if buffer.ends_with(PREVIEW_NOTICE) {
        return;
    }
    let room = MAX_ITEM_BYTES.saturating_sub(buffer.len());
    if delta.len() <= room {
        buffer.push_str(delta);
        return;
    }
    let content_limit = MAX_ITEM_BYTES - PREVIEW_NOTICE.len();
    if buffer.len() > content_limit {
        buffer.truncate(floor_char_boundary(buffer, content_limit));
    }
    let take = floor_char_boundary(delta, content_limit.saturating_sub(buffer.len()));
    buffer.push_str(&delta[..take]);
    buffer.push_str(PREVIEW_NOTICE);
}

fn limit_item(item: &mut Item) {
    match item {
        Item::User(s)
        | Item::Assistant(s)
        | Item::Thinking(s)
        | Item::Notice(s)
        | Item::Verify { summary: s, .. } => {
            if s.len() > MAX_ITEM_BYTES {
                s.truncate(floor_char_boundary(
                    s,
                    MAX_ITEM_BYTES - PREVIEW_NOTICE.len(),
                ));
                s.push_str(PREVIEW_NOTICE);
            }
        }
        Item::Compaction {
            summary: Some(s), ..
        } => {
            if s.len() > MAX_ITEM_BYTES {
                s.truncate(floor_char_boundary(
                    s,
                    MAX_ITEM_BYTES - PREVIEW_NOTICE.len(),
                ));
                s.push_str(PREVIEW_NOTICE);
            }
        }
        Item::ToolCall { args, .. } | Item::ToolResult { payload: args, .. } => {
            let bytes = crate::chat_presentation::size(args);
            if bytes > MAX_ITEM_BYTES {
                *args = serde_json::json!({"preview_omitted_bytes": bytes, "notice": PREVIEW_NOTICE.trim()});
            }
        }
        _ => {}
    }
}

fn ordered_selection(selection: TextSelection) -> Option<(TextPoint, TextPoint)> {
    if !selection.non_empty {
        return None;
    }
    Some(if selection.anchor <= selection.focus {
        (selection.anchor, selection.focus)
    } else {
        (selection.focus, selection.anchor)
    })
}

fn selection_cell_range(selection: TextSelection, row: usize) -> Option<(usize, usize)> {
    let (start, end) = ordered_selection(selection)?;
    if row < start.row || row > end.row {
        return None;
    }
    let from = if row == start.row { start.column } else { 0 };
    let to = if row == end.row {
        end.column.saturating_add(1)
    } else {
        usize::MAX
    };
    Some((from, to))
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn text_cell_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    text.chars()
        .map(|character| character.width().unwrap_or(0))
        .sum()
}

/// Return the characters whose terminal cells intersect `[start, end)`.
fn slice_cells(text: &str, start: usize, end: usize) -> String {
    use unicode_width::UnicodeWidthChar;

    if start >= end {
        return String::new();
    }
    let mut result = String::new();
    let mut column = 0usize;
    let mut previous_selected = false;
    for character in text.chars() {
        let width = character.width().unwrap_or(0);
        let selected = if width == 0 {
            previous_selected
        } else {
            column < end && column.saturating_add(width) > start
        };
        if selected {
            result.push(character);
        }
        previous_selected = selected;
        column = column.saturating_add(width);
    }
    result
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Double-click word semantics favor source-code identifiers: letters, digits,
/// and `_` form a word. Punctuation remains independently selectable.
fn word_cell_range(text: &str, target: usize) -> Option<(usize, usize)> {
    use unicode_width::UnicodeWidthChar;

    let mut column = 0usize;
    let mut chars = Vec::new();
    for character in text.chars() {
        let width = character.width().unwrap_or(0);
        let start = column;
        column = column.saturating_add(width);
        chars.push((character, start, column));
    }
    let index = chars.iter().position(|(_, start, end)| {
        (*start <= target && target < *end) || (*start == target && *start == *end)
    })?;
    let is_word = |character: char| character.is_alphanumeric() || character == '_';
    if !is_word(chars[index].0) {
        return Some((chars[index].1, chars[index].2.max(chars[index].1 + 1)));
    }
    let mut first = index;
    while first > 0 && is_word(chars[first - 1].0) {
        first -= 1;
    }
    let mut last = index + 1;
    while last < chars.len() && is_word(chars[last].0) {
        last += 1;
    }
    Some((chars[first].1, chars[last - 1].2))
}

/// Approval choices come from the backend's authoritative prompt.
enum ApprovalResponder {
    Remote(protocol::ApprovalPrompt),
}
impl ApprovalResponder {
    fn options_for(&self, _escalated: bool) -> Vec<&'static str> {
        let Self::Remote(prompt) = self;
        let about_a_path = matches!(prompt.kind, protocol::ApprovalKind::Path);
        prompt
            .choices
            .iter()
            .map(|choice| match choice {
                protocol::ApprovalDecision::Approve | protocol::ApprovalDecision::Once => {
                    "Yes, allow once"
                }
                // For a path, which of the two is remembered is said in the choice itself.
                protocol::ApprovalDecision::Always if about_a_path => {
                    match prompt.path.as_ref().map(|path| path.kind) {
                        Some(protocol::PathKind::File) => "Yes, always allow this file",
                        Some(protocol::PathKind::Directory) => {
                            "Yes, always allow this folder and its contents"
                        }
                        _ => "Yes, always allow this path",
                    }
                }
                protocol::ApprovalDecision::Always => "Yes, always allow",
                protocol::ApprovalDecision::Folder => "Yes, always allow this whole folder",
                protocol::ApprovalDecision::Session => "Allow for this session",
                protocol::ApprovalDecision::Persistent => "Always allow for this project",
                protocol::ApprovalDecision::Deny => "No, deny",
            })
            .collect()
    }
}

/// Pending inline approval.
struct PendingApproval {
    action: String,
    detail: Option<String>,
    /// Trust-escalated actions cannot be remembered or auto-approved.
    escalated: bool,
    responder: ApprovalResponder,
}

/// The user's in-progress answer to one clarify question.
#[derive(Default)]
struct ClarifyDraft {
    /// Chosen option indices (one for radio, any for checkbox).
    selected: Vec<usize>,
    /// Free-text entered via the "Other" row.
    other: Option<String>,
}

/// In-flight `clarify` form that owns keyboard input while visible.
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
    responder: QuestionResponder,
}

enum QuestionResponder {
    Remote(u64),
}

impl ClarifyState {
    /// Current options plus the free-text row.
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
    support: kernel::ReasoningSupport,
    last_turn_received: Option<bool>,
    choosing_effort: bool,
    levels: Vec<kernel::ReasoningEffort>,
}

impl ReasoningPanelState {
    fn from_model(model: &Model) -> Self {
        Self {
            enabled: model.reasoning.enabled,
            show: model.show_thinking,
            effort: model.reasoning.effort,
            support: model.reasoning_support,
            last_turn_received: model.last_turn_reasoning_received,
            choosing_effort: false,
            levels: kernel::ReasoningEffort::ALL.to_vec(),
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
        self.effort
            .map(kernel::ReasoningEffort::as_str)
            .unwrap_or("Auto")
    }

    fn last_turn_label(&self) -> &'static str {
        match self.last_turn_received {
            Some(true) => "Reasoning received",
            Some(false) => "No reasoning received",
            None => "No completed turn yet",
        }
    }

    fn labels(&self) -> Vec<String> {
        if self.choosing_effort {
            let selected = if self.enabled == Some(false) {
                Some(kernel::ReasoningEffort::None)
            } else {
                self.effort
            };
            return std::iter::once(None)
                .chain(self.levels.iter().copied().map(Some))
                .enumerate()
                .map(|(i, effort)| {
                    let description = match effort {
                        None => "auto — let the server decide",
                        Some(kernel::ReasoningEffort::None) => "none — disable reasoning",
                        Some(kernel::ReasoningEffort::Minimal) => "minimal — least reasoning",
                        Some(kernel::ReasoningEffort::Low) => "low — faster responses",
                        Some(kernel::ReasoningEffort::Medium) => {
                            "medium — balanced depth and speed"
                        }
                        Some(kernel::ReasoningEffort::High) => "high — more depth",
                        Some(kernel::ReasoningEffort::XHigh) => "xhigh — deeper, slower reasoning",
                        Some(kernel::ReasoningEffort::Max) => {
                            "max — maximum depth on supporting models"
                        }
                        Some(kernel::ReasoningEffort::Ultra) => {
                            "ultra — only where explicitly supported"
                        }
                    };
                    format!(
                        "{}. {} {}",
                        i + 1,
                        if selected == effort { "✓" } else { " " },
                        description
                    )
                })
                .collect();
        }
        vec![
            format!("Mode:       {}", self.mode_label()),
            format!("Visibility: {}", self.visibility_label()),
            format!("Effort:     {}", self.effort_label()),
            format!("Support:    {}", self.support.as_str()),
            format!("Last turn:  {}", self.last_turn_label()),
        ]
    }

    fn status_block(&self) -> String {
        format!(
            "reasoning\n  Mode:       {}\n  Visibility: {}\n  Effort:     {}\n  Support:    {}\n  Last turn:  {}",
            self.mode_label(),
            self.visibility_label(),
            self.effort_label(),
            self.support.as_str(),
            self.last_turn_label()
        )
    }
}

/// One MCP Registry setup: its row, the `/mcp add` line it fills in, and
/// where the cursor goes for the value still needed.
#[derive(Debug, Clone)]
pub(crate) struct CatalogPick {
    pub label: String,
    pub line: String,
    pub cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConnectorPick {
    pub id: String,
    pub label: String,
}

/// Reasoning control / session picker kind. Not `Copy` — some variants own data.
#[derive(Clone)]
enum PickerKind {
    /// `/mcp catalog`: registry servers Medha can set up.
    McpCatalog(Vec<CatalogPick>),
    /// `/connect`: the reviewed connectors.
    Connectors(Vec<ConnectorPick>),
    Reasoning(ReasoningPanelState),
    /// Browse past sessions to resume. Holds the list from `log.sessions()`.
    Session(Vec<kernel::SessionMeta>),
    /// Time-travel cut points in the current session. Holds the list from
    /// `log.events()`, one entry per past user turn.
    Rewind(Vec<RewindPoint>),
    /// Step 2 of `/rewind`: having chosen a cut point, pick the scope
    /// (conversation only · conversation + code · cancel).
    RewindMode(RewindPoint),
    Memory(Vec<memory::MemoryEntry>),
    /// `/skill` with no name: pick an installed skill to force-load. Holds
    /// (name, description) for each effective skill.
    Skill(Vec<(String, String)>),
    /// Destructive user-skill removal always gets explicit confirmation.
    RemoveSkill(String),
    /// A skill installed switched off, for its code to be read first, is
    /// switched on only by saying so.
    EnableSkill(String),
    /// Provider presets shared with first-run setup, followed by Custom.
    ModelProtocol,
    /// OpenAI-compatible deployment presets shared with first-run setup,
    /// followed by Custom.
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
    SkillSearch(Vec<protocol::SkillHit>),
    /// The skill hub's "Manage skills…" sub-menu (updates / sources / lock /
    /// sync). Fixed rows from [`SKILL_MANAGE_ACTIONS`]; no data to carry.
    SkillManage,
    /// The sources sub-picker (reached from Manage → Sources). Each entry is
    /// `(repo, path, removable)`; built-ins are shown but not removable. Rows are
    /// an "Add a source…" row, one per source, then "Back".
    SkillSources(Vec<(String, String, bool)>),
    /// `/plugins`: every plugin screen; its rows and keys live in `plugins`.
    BackendPlugins(Box<backend_plugins::Screen>),
    /// `/theme`: pick the colour theme. Rows are [`theme::modes`]; choosing one
    /// re-colours the UI live for the session.
    Theme,
    /// `/mcp`: manage MCP servers. Row 0 is "＋ Add a server"; the rest are the
    /// configured servers. Enter connects (or opens add); `d` removes.
    Mcp(Vec<McpRow>),
    /// One server's catalogue, each tool switched on or off. Space toggles;
    /// the choice is saved to that server's `deny_tools`.
    McpTools {
        id: String,
        tools: Vec<(String, bool)>,
    },
    /// `/agents`: what has been delegated. `d` stops a running child; `a`
    /// applies a finished writer's patch.
    Agents(Vec<AgentRow>),
    /// How to authenticate a remote MCP server the probe could not classify.
    /// Only shown when the server is ambiguous — a server advertising OAuth
    /// signs in without asking.
    McpAuth {
        id: String,
        url: String,
    },
}

/// One running, settled, or patch-ready row in the `/agents` panel.
#[derive(Debug, Clone)]
pub(super) enum AgentRow {
    /// A child, running or settled — its `state` says which.
    Agent {
        agent: orchestrator::Agent,
        /// What this child is doing, from the live plane. `None` only before the
        /// first sample; it is never used to mean "quiet", because the phase says
        /// that far better than a timestamp difference ever could.
        progress: Option<kernel::Progress>,
        /// Drawn tree branch for this row's depth, empty at the top level.
        /// Precomputed because it depends on what *follows* the row, which a
        /// per-row render cannot see.
        branch: String,
    },
    /// A writer's patch, waiting for the user to accept or ignore it.
    Patch {
        agent: String,
        /// Exact durable handout, and the only way a patch is addressed: a
        /// follow-up reuses the child's session, so a session id names a moving
        /// target while this names the diff the user is looking at.
        dispatch: String,
        files: usize,
        /// `None` when the project has no verify command — not the same as a
        /// patch that failed, and must not be shown as if it were.
        verified: Option<bool>,
    },
}

/// Rows of [`PickerKind::McpAuth`], in order.
pub(super) const MCP_AUTH_CHOICES: &[&str] = &[
    "Sign in with OAuth (opens a browser)",
    "Paste an API token",
    "Connect without authentication",
];

/// One configured MCP server row in the `/mcp` picker.
#[derive(Clone)]
pub(super) struct McpRow {
    pub id: String,
    pub command: String,
    pub disabled: bool,
}

const AUTONOMY_MODES: &[(kernel::AutonomyLevel, &str)] = &[
    (
        kernel::AutonomyLevel::Plan,
        "plan — read-only investigation; no edits, shell, or delegation",
    ),
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
            PickerKind::Reasoning(state) if state.choosing_effort => {
                " effort · ↑↓ choose · Enter apply · Esc back ".into()
            }
            PickerKind::Reasoning(_) => " reasoning · ↑↓ select · Enter change · Esc done ".into(),
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
            PickerKind::Memory(_) => {
                " memory — ↑↓ select · Enter provenance · p pin/unpin · f forget · Esc cancel "
                    .into()
            }
            PickerKind::Skill(_) => {
                " skill hub — ↑↓ select · Enter use · Space on/off · Esc cancel ".into()
            }
            PickerKind::McpCatalog(_) => {
                " MCP catalog — ↑↓ select · Enter fills in /mcp add · Esc close ".into()
            }
            PickerKind::Connectors(_) => {
                " connect an app — ↑↓ select · Enter connects · Esc close ".into()
            }
            PickerKind::RemoveSkill(name) => {
                format!(" remove user skill '{name}'? — ↑↓ move · Enter confirm · Esc back ")
            }
            PickerKind::EnableSkill(name) => {
                format!(" switch on '{name}'? · ↑↓ move · Enter confirm · Esc back ")
            }
            PickerKind::ProviderPreset => {
                " choose provider — ↑↓ move · Enter/→ continue · Esc/← back ".into()
            }
            PickerKind::ModelProtocol => {
                " choose model protocol — ↑↓ move · Enter/→ continue · Esc/← back ".into()
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
            PickerKind::SkillSearch(_) => " add a skill — ↑↓ select · Enter · Esc back ".into(),
            PickerKind::SkillManage => " manage skills — ↑↓ select · Enter · Esc back ".into(),
            PickerKind::SkillSources(_) => " skill sources — ↑↓ · Enter · Esc back ".into(),
            PickerKind::BackendPlugins(screen) => screen.title(),
            PickerKind::Theme => " theme — ↑↓ select · Enter apply · Esc done ".into(),
            PickerKind::Mcp(_) => {
                " MCP — Enter connect · space on/off · t tools · d remove · Esc close ".into()
            }
            PickerKind::McpTools { id, tools } => {
                let on = tools.iter().filter(|(_, on)| *on).count();
                format!(
                    " {id} — {on}/{} tools in context · space toggle · Esc back ",
                    tools.len()
                )
            }
            PickerKind::Agents(rows) => {
                let running = rows
                    .iter()
                    .filter(
                        |row| matches!(row, AgentRow::Agent { agent, .. } if agent.is_running()),
                    )
                    .count();
                let patches = rows
                    .iter()
                    .filter(|row| matches!(row, AgentRow::Patch { .. }))
                    .count();
                let done = rows.len() - running - patches;
                if rows.is_empty() {
                    return " agents — nothing delegated · Esc close ".into();
                }
                let mut parts = Vec::new();
                if running > 0 {
                    parts.push(format!("{running} running"));
                }
                if patches > 0 {
                    parts.push(format!("{patches} patch(es) waiting"));
                }
                if done > 0 {
                    parts.push(format!("{done} finished"));
                }
                // Enter reads any row — a diff for a patch, the live transcript
                // for anything else — so it is always offered.
                let keys = match (running > 0, patches > 0) {
                    (true, true) => "Enter watch/view · d stop · a apply",
                    (true, false) => "Enter watch · d stop",
                    (false, true) => "Enter view · a apply",
                    (false, false) => "Enter view",
                };
                format!(" {} — {keys} · Esc close ", parts.join(" · "))
            }
            PickerKind::McpAuth { id, .. } => {
                format!(" {id} needs credentials — ↑↓ select · Enter · Esc cancel ")
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
            PickerKind::Memory(entries) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs_f64())
                    .unwrap_or(0.0);
                entries
                    .iter()
                    .map(|entry| {
                        let age = ((now - entry.updated).max(0.0) / 86_400.0).floor() as u64;
                        format!(
                            "[{} · {}d{}] {} — {}",
                            entry.trust.as_str(),
                            age,
                            if entry.pinned { " · pinned" } else { "" },
                            entry.name,
                            entry.description
                        )
                    })
                    .collect()
            }
            PickerKind::McpCatalog(picks) => picks.iter().map(|pick| pick.label.clone()).collect(),
            PickerKind::Connectors(picks) => picks.iter().map(|pick| pick.label.clone()).collect(),
            PickerKind::Skill(skills) => SKILL_HUB_ACTIONS
                .iter()
                .map(|(label, _)| (*label).to_string())
                .chain(skills.iter().map(|(n, d)| format!("{n} — {d}")))
                .collect(),
            PickerKind::RemoveSkill(_) => {
                vec!["Keep skill".to_string(), "Remove user skill".to_string()]
            }
            PickerKind::EnableSkill(_) => vec![
                "Keep it off".to_string(),
                "Switch it on. I have read what it runs".to_string(),
            ],
            PickerKind::ModelProtocol => MODEL_PROTOCOLS
                .iter()
                .map(|(label, available, _)| {
                    format!(
                        "{label} — {}",
                        if *available {
                            "available"
                        } else {
                            "coming soon"
                        }
                    )
                })
                .collect(),
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
                            "{}{} — {} · {} · {}{}{}",
                            mark,
                            p.name,
                            p.provider.model,
                            p.provider.protocol.as_str(),
                            p.provider.base_url,
                            ctx,
                            startup
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
            PickerKind::Theme => theme::modes()
                .iter()
                .map(|(_, desc)| (*desc).to_string())
                .collect(),
            PickerKind::McpAuth { .. } => {
                MCP_AUTH_CHOICES.iter().map(|c| (*c).to_string()).collect()
            }
            PickerKind::Mcp(rows) => std::iter::once("＋ Add a server".to_string())
                .chain(rows.iter().map(|row| {
                    format!(
                        "{} {}   {}",
                        if row.disabled { "○" } else { "●" },
                        row.id,
                        row.command
                    )
                }))
                .collect(),
            PickerKind::Agents(rows) if rows.is_empty() => {
                vec!["nothing waiting — running agents are on tab".to_string()]
            }
            PickerKind::Agents(rows) => rows
                .iter()
                .map(|row| match row {
                    AgentRow::Agent {
                        agent,
                        progress,
                        branch,
                    } => {
                        let objective: String = agent.objective.chars().take(52).collect();
                        // The phase, not a timestamp difference. "starting" used
                        // to appear for every running agent on the first paint
                        // and for anything the log had not heard from, which made
                        // a wedged child and a thinking one identical.
                        let note = match (&agent.state, progress) {
                            (orchestrator::State::Running, Some(progress)) => {
                                let counters = match progress.tool_calls {
                                    0 => String::new(),
                                    n => format!("{n} tools · "),
                                };
                                format!("  {counters}{}", progress.phase.label())
                            }
                            (orchestrator::State::Running, None) => String::new(),
                            (orchestrator::State::Settled(_), _) => String::new(),
                        };
                        let mark = match agent.state {
                            orchestrator::State::Running => "⚇",
                            orchestrator::State::Settled(orchestrator::AgentStatus::Completed) => {
                                "✓"
                            }
                            orchestrator::State::Settled(orchestrator::AgentStatus::Exhausted) => {
                                "◐"
                            }
                            orchestrator::State::Settled(orchestrator::AgentStatus::Cancelled) => {
                                "⊘"
                            }
                            orchestrator::State::Settled(orchestrator::AgentStatus::Failed) => "✗",
                        };
                        format!("{branch}{mark} {}   {objective}{note}", agent.path.name())
                    }
                    // Verification is on the row, not one level down: a patch
                    // that failed its build is a draft, and that is the thing a
                    // user should not have to go looking for before applying.
                    AgentRow::Patch {
                        agent,
                        files,
                        verified,
                        ..
                    } => format!(
                        "⎇ {agent}   {files} file(s) · {}",
                        match verified {
                            Some(true) => "verified",
                            Some(false) => "BUILD FAILED",
                            None => "not verified",
                        }
                    ),
                })
                .collect(),
            PickerKind::McpTools { tools, .. } => tools
                .iter()
                .map(|(name, on)| format!("{} {name}", if *on { "●" } else { "○" }))
                .chain(std::iter::once("← Back".to_string()))
                .collect(),
            // Row 0 is always "install from a link" (never a dead end); the rest
            // are the browsable catalog, one row per skill.
            PickerKind::SkillSearch(hits) => {
                std::iter::once("🔗 Install from a link or local folder…".to_string())
                    .chain(hits.iter().map(|h| {
                        let desc: String = h.description.chars().take(66).collect();
                        format!("{} · {} — {desc}", h.name, h.repo)
                    }))
                    .collect()
            }
            PickerKind::SkillManage => SKILL_MANAGE_ACTIONS
                .iter()
                .map(|(label, _)| (*label).to_string())
                .collect(),
            PickerKind::BackendPlugins(screen) => screen.labels(),
            PickerKind::SkillSources(sources) => {
                let mut rows = vec!["➕ Add a source…".to_string()];
                for (repo, path, removable) in sources {
                    rows.push(if *removable {
                        format!("✕  {repo}  ·  {path}/     (remove)")
                    } else {
                        format!("·  {repo}  ·  {path}/     (built-in)")
                    });
                }
                rows.push("← Back".to_string());
                rows
            }
        }
    }
}

/// Arrow-key picker state
struct Picker {
    kind: PickerKind,
    selected: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionOp {
    Resume {
        source: ulid::Ulid,
        target: ulid::Ulid,
    },
    Rewind {
        source: ulid::Ulid,
    },
    Opening,
}

/// Guided model setup keeps credentials out of history and the transcript.
#[derive(Clone, Copy)]
enum ModelSetupStep {
    BaseUrl,
    ApiKey,
    /// `/v1/models` is being queried in the background; Enter is inert until
    /// the result arrives (picker on success, manual ModelId on failure).
    Discovering,
    ModelId,
    ContextWindow,
    Saving,
    Activating,
}

#[derive(Default)]
struct Editor {
    text: String,
    cursor: usize,
}

struct ModelSetup {
    editor: Editor,
    mode: ModelSetupMode,
    step: ModelSetupStep,
    protocol: kernel::Protocol,
    base_url: String,
    api_key: String,
    model: String,
    /// Context window carried over from discovery, when the server reports it.
    max_ctx: Option<u32>,
}

struct McpCredential {
    id: String,
    saving: bool,
    editor: Editor,
}

enum ModelSetupMode {
    Add,
    UpdateKey { profile: String },
}

impl ModelSetup {
    fn new() -> Self {
        Self {
            editor: Editor::default(),
            mode: ModelSetupMode::Add,
            step: ModelSetupStep::BaseUrl,
            protocol: kernel::Protocol::OpenAiChat,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            max_ctx: None,
        }
    }

    fn update_key(profile: String, protocol: kernel::Protocol, base_url: String) -> Self {
        Self {
            editor: Editor::default(),
            mode: ModelSetupMode::UpdateKey { profile },
            step: ModelSetupStep::ApiKey,
            protocol,
            base_url,
            api_key: String::new(),
            model: String::new(),
            max_ctx: None,
        }
    }

    fn prompt(&self) -> &'static str {
        match self.step {
            ModelSetupStep::BaseUrl => match self.protocol {
                kernel::Protocol::OpenAiChat => {
                    "OpenAI-compatible API root or full /chat/completions URL (for example http://localhost:11434/v1):"
                }
                kernel::Protocol::GeminiInteractions => {
                    "Gemini Interactions v1 base URL (normally https://generativelanguage.googleapis.com/v1):"
                }
                _ => "Provider base URL:",
            },
            ModelSetupStep::ApiKey => match &self.mode {
                ModelSetupMode::Add if self.protocol == kernel::Protocol::GeminiInteractions => {
                    "Gemini API key (required; stored securely, never in config.toml):"
                }
                ModelSetupMode::Add => {
                    "API key (leave blank for a local server; stored securely, never in config.toml):"
                }
                ModelSetupMode::UpdateKey { .. } => {
                    "New API key (stored securely, never in config.toml):"
                }
            },
            ModelSetupStep::Discovering => "Querying the server for its models… (Esc cancels)",
            ModelSetupStep::Saving => {
                "Saving the model… (Esc leaves the form; an accepted save can still finish)"
            }
            ModelSetupStep::Activating => "The model is saved; activating it…",
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

/// Guided search setup keeps API keys out of history and the transcript.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchSetupStep {
    /// The provider picker is open (owned by the generic picker handler).
    Provider,
    /// Entering the secret: an API key (Tavily/Brave, masked) or the instance
    /// URL (SearXNG, not masked).
    Secret,
    Saving,
}

/// In-flight `/search` draft. `provider` is set once the picker is confirmed.
struct SearchSetup {
    editor: Editor,
    provider: tools::SearchProvider,
    step: SearchSetupStep,
}

impl SearchSetup {
    fn new() -> Self {
        // DuckDuckGo is a placeholder until the picker sets the real choice.
        Self {
            editor: Editor::default(),
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

/// Data needed to render and stage local files. The chat sandbox stays in the backend.
#[derive(Clone)]
struct WorkspaceView {
    root: std::path::PathBuf,
    execution: String,
}

impl WorkspaceView {
    fn root(&self) -> &std::path::Path {
        &self.root
    }
    fn exec_backend_label(&self) -> &str {
        &self.execution
    }
}
#[cfg(test)]
impl From<Arc<WorkspaceSandbox>> for WorkspaceView {
    fn from(local: Arc<WorkspaceSandbox>) -> Self {
        Self {
            root: local.root().into(),
            execution: local.exec_backend_label().into(),
        }
    }
}

/// Complete TUI state.
struct Model {
    /// Capped transcript items; durable history remains in the event log.
    items: VecDeque<Entry>,
    /// Tool name → its declared presentation (glyph + category), from the
    /// executor's specs. The surface renders each tool's own glyph — no
    /// name→glyph table here.
    tool_viz: HashMap<String, ToolViz>,
    input: String,
    images: staged_images::Pending,
    /// A message held back until its attachments finish being admitted.
    backend_deferred: Option<backend_ui::DeferredSend>,
    /// UTF-8 byte offset, always on a character boundary.
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    scroll_offset: usize,
    auto_scroll: bool,
    viewport_height: usize,
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
    /// Last rendered transcript rectangle, used to translate mouse coordinates
    /// into virtual transcript rows.
    transcript_area: Rect,
    /// Application-owned transcript selection. Mouse capture stays enabled so
    /// wheel scrolling works consistently across terminal emulators.
    text_selection: Option<TextSelection>,
    mouse_selecting: bool,
    last_click: Option<LastClick>,
    /// Text waiting for the event loop to send to the terminal clipboard.
    pending_clipboard: Option<String>,
    /// Brief copy result shown without mutating the transcript.
    clipboard_status: Option<(String, Instant)>,
    /// The pending approval card has been rendered at least once, so its options
    /// are on screen and selection input is safe to accept (blocks blind-Enter).
    approval_ready: bool,
    context_pressure: Option<kernel::ContextPressure>,
    /// Live prompt tokens observed since opening this session (not restored
    /// from history), and how many of them the provider served
    /// from its prefix cache. Accumulated rather than per-turn: one request's
    /// ratio swings with what the turn happened to add, and the question worth
    /// answering is what the session as a whole is re-paying for. Stays `None`
    /// until a route reports the bucket at least once, so "not measured" never
    /// renders as "no cache".
    cache: Option<(u64, u64)>,
    cache_last_usage: Option<kernel::Usage>,
    cache_unreported_attempts: u64,
    /// Session cost so far (USD, `true` = indicative "est." figure), when known.
    cost_usd: Option<(f64, bool)>,
    model: String,
    /// Active wire contract. Kept distinct from the provider/model label so a
    /// saved profile never hides which API shape is actually in use.
    protocol: kernel::Protocol,
    max_ctx: Option<u32>,
    /// The saved profile currently active for this session. This is distinct
    /// from `model`, which is the provider's model id and may be duplicated
    /// across endpoints.
    active_profile: String,
    /// Persistent model profiles, shared with the main entrypoint so a TUI add
    /// is immediately available to this and future sessions.
    /// `/model add` is a guided flow. Its API-key field is masked and never
    /// enters history, the transcript, or config.toml.
    model_setup: Option<ModelSetup>,
    /// Reject receipts from a cancelled or replaced credential form.
    form_generation: u64,
    command_draft: Option<String>,
    command_refused: bool,
    /// Live web-search settings shared with the `web.*` tools. `/search` writes
    /// it so a provider change takes effect on the next search without restart.
    /// `/search` is a guided flow like `/model add`; its key field is masked.
    search_setup: Option<SearchSetup>,
    mcp_credential: Option<McpCredential>,
    /// Autonomy dial (`/mode`): how much runs without asking. Applied to the
    /// session at the start of each turn; the safety floor is level-independent.
    autonomy: kernel::AutonomyLevel,
    running: bool,
    /// An asynchronous resume/rewind owns the session boundary until its
    /// terminal event. Composer submissions and other boundaries wait.
    session_op: Option<SessionOp>,
    /// Detached follow-up admissions not yet visible in the active roster.
    pending_agent_launches: usize,
    /// User messages accepted by a child but not yet reported as applied or
    /// returned. Boundaries wait so typed text cannot vanish in an event tail.
    pending_agent_steers: usize,
    /// A background report is collectable but a turn is already in flight. It
    /// cannot be injected mid-response, so the signal is held until this settles.
    /// Tool whose call is currently streaming: (name, optional target file/command).
    /// Drives the "writing medha.html…" activity label instead of a vague "thinking".
    current_tool: Option<(String, Option<String>)>,
    /// When the current turn started — for the elapsed-time counter in the status.
    turn_started: Option<Instant>,
    /// Interrupt handle for the running turn: Esc → graceful cancel_turn,
    /// Enter mid-turn → steer (applied at the next turn boundary).
    /// Owned foreground turn. The surface retains this handle until the turn
    /// has settled, and shutdown cancels then joins it before restoring the
    /// terminal or tearing down shared LSP/MCP/agent services.
    /// An Esc was sent and the kernel is settling in-flight tools — used to
    /// show one "stopping…" notice instead of one per Esc press.
    cancelling: bool,
    /// A second Esc aborted the foreground task and its cancellation is being
    /// joined. Visible turn state is already clear, but admitting another turn
    /// before this barrier settles could overlap owned tool/process cleanup.
    force_aborting: bool,
    /// Concurrent prompts retain each responder until answered in order.
    pending_approvals: VecDeque<PendingApproval>,
    /// In-flight `clarify` question form (owns input while `Some`).
    clarify: Option<ClarifyState>,
    /// Owner tag for `clarify`, kept beside the state so renderer test fixtures
    /// can construct the presentation-only form without cancellation plumbing.
    /// Selected approval option: once, always, or deny.
    approval_sel: usize,
    approval_expanded: bool,
    /// Session-scoped remembered approvals.
    reasoning: kernel::ReasoningConfig,
    /// Model/profile-level control support; unknown stays visibly unverified.
    reasoning_support: kernel::ReasoningSupport,
    /// SSE streaming on/off (mirrors the provider; shown in the status bar).
    streaming: bool,
    /// Whether any reasoning delta arrived during the active turn.
    reasoning_received_this_turn: bool,
    /// Assistant/thinking items this turn has streamed into the transcript.
    /// A retried turn re-streams its reply, so this is how much of the tail
    /// belongs to the attempt being abandoned — counted rather than searched
    /// for, because an earlier turn's answer can also end in an assistant item.
    streamed_this_turn: usize,
    /// Delivery result for the most recently completed turn. `None` means this
    /// TUI has not completed a turn yet (resumed history does not retain it).
    last_turn_reasoning_received: Option<bool>,
    picker: Option<Picker>,
    ac_sel: usize,
    welcome: bool,
    show_thinking: bool,
    /// Whether tool I/O is expanded.
    full_transparency: bool,
    anim_frame: u64,
    intro_frame: Option<u64>,
    should_quit: bool,
    /// One returned draft exceeding the composer budget; saved before exit.
    recovery_draft: Option<String>,
    last_redraw: Instant,
    /// Full large-paste content indexed by compact input placeholders.
    pastes: Vec<String>,
    /// Workspace identity and execution label returned by the backend.
    restore: WorkspaceView,
    /// Live owned shell tasks, polled from the executor so the *user* sees a
    /// foreground command that is still running in a concurrent session.
    bg_tasks: Vec<kernel::BackgroundTask>,
    /// A compaction (summarize pass) is currently running — shows a live
    /// "compacting…" indicator.
    compacting: bool,
    /// Expand compaction cards to show their full summary text (toggled by ^E).
    show_summary: bool,
    /// Whether this chat exposes the backend's memory commands.
    memory_enabled: bool,
    /// A plugin command is being expanded by the backend.
    plugin_command_busy: bool,
    /// Children running right now, refreshed on the animation tick.
    agent_runs: Vec<orchestrator::Agent>,
    /// What each child is doing, from the live plane rather than the event log,
    /// so the tree can name the tool a child is in instead of only that it runs.
    /// Refreshed on the same tick as `agent_runs`.
    agent_progress: HashMap<orchestrator::AgentPath, kernel::Progress>,
    /// Children that have settled since the last record was written. Held until
    /// the fleet empties so one fan-out leaves one record, not a line per child
    /// finishing at its own pace.
    agents_done: Vec<AgentDoneRow>,
    /// Each live child's own transcript, so one can be opened and watched.
    ///
    /// A bounded ring: a chatty child must not be able to grow this without
    /// limit, and the event log holds the complete record for anything that has
    /// scrolled off or settled.
    agent_panes: HashMap<orchestrator::AgentPath, VecDeque<Entry>>,
    /// Which pane the transcript area shows. `None` is the conversation itself.
    /// Kept apart from the switcher's cursor so moving through the list does not
    /// yank the view out from under what is being read.
    focus: Option<orchestrator::AgentPath>,
    /// The conversation, parked here while an agent's pane is on screen.
    ///
    /// One collection is displayed and every renderer reads it, so switching
    /// panes moves content in and out of `items` rather than teaching the
    /// renderer to choose — which it cannot do while holding a borrow of the
    /// model for its render context.
    parked_main: VecDeque<Entry>,
    /// Scroll position and follow mode of each pane that is not on screen, so
    /// returning to one behaves exactly as it did when the reader left it.
    parked_scroll: HashMap<Option<orchestrator::AgentPath>, (usize, bool)>,
    /// Where the switcher's keyboard cursor sits, as an index into its rows.
    /// Only meaningful while [`Model::switching`] is set.
    switch_cursor: usize,
    /// Whether the switcher owns the arrow keys. Off by default, because the
    /// input already claims them for history and the transcript for scrolling.
    switching: bool,
    /// Whether the switcher has ever been opened. The way in is advertised on the
    /// tree until it has been taken once, then stops competing for attention: a
    /// hint that never quiets down is one that stops being read.
    switched_before: bool,
    remote: Option<backend_ui::Peer>,
}

/// How much of one child's stream is kept for viewing.
const MAX_AGENT_PANE_ITEMS: usize = 200;

/// Append one rendered item while keeping the pane's physical history bounded.
/// Returns whether an old entry was evicted.
fn append_pane_item(pane: &mut VecDeque<Entry>, item: Item, limit: usize) -> bool {
    // Completion order can differ from dispatch order. Attach a result to its
    // exact call, including in parked child panes and reconstructed history.
    let call_index = match &item {
        Item::ToolResult {
            id: Some(id), tool, ..
        } => pane.iter().rposition(|entry| {
            matches!(&entry.item, Item::ToolCall { id: Some(call_id), tool: call_tool, .. }
                if call_id == id && call_tool == tool)
        }),
        _ => None,
    };
    if let Some(index) = call_index {
        pane.insert(index + 1, Entry::new(item));
    } else {
        pane.push_back(Entry::new(item));
    }
    trim_pane(pane, limit)
}

fn trim_pane(pane: &mut VecDeque<Entry>, limit: usize) -> bool {
    let mut bytes: usize = pane.iter().map(|entry| entry.bytes).sum();
    let mut dropped = false;
    while pane.len() > limit || bytes > MAX_PANE_BYTES {
        if let Some(entry) = pane.pop_front() {
            bytes = bytes.saturating_sub(entry.bytes);
        }
        dropped = true;
    }
    dropped
}

/// Notices sit before a live assistant block so later stream deltas continue
/// extending that block instead of creating a second answer.
fn append_pane_notice(pane: &mut VecDeque<Entry>, notice: String, limit: usize) -> bool {
    let live = matches!(
        pane.back().map(|entry| &entry.item),
        Some(Item::Assistant(_))
    )
    .then(|| pane.pop_back())
    .flatten();
    pane.push_back(Entry::new(Item::Notice(notice)));
    if let Some(live) = live {
        pane.push_back(live);
    }
    trim_pane(pane, limit)
}

/// Add one step to a pane, coalescing streamed deltas onto the item they extend
/// so a reply reads as a block rather than a line per token.
fn append_agent_step(pane: &mut VecDeque<Entry>, step: AgentStep) {
    let extend = |pane: &mut VecDeque<Entry>, delta: &str, thinking: bool| -> bool {
        let Some(entry) = pane.back_mut() else {
            return false;
        };
        let buffer = match (&mut entry.item, thinking) {
            (Item::Assistant(buffer), false) => buffer,
            (Item::Thinking(buffer), true) => buffer,
            _ => return false,
        };
        append_preview(buffer, delta);
        entry.invalidate();
        trim_pane(pane, MAX_AGENT_PANE_ITEMS);
        true
    };
    let queued_notice = |text: &str| format!("↳ queued for this agent: {text}");
    let item = match step {
        // Rendered as a user turn, because that is what it is: the message this
        // session was started with.
        AgentStep::Task {
            objective,
            contract,
        } => Item::User(match contract {
            Some(contract) => format!("{objective}\n\nAnswer must be: {contract}"),
            None => objective,
        }),
        AgentStep::Text(delta) => match extend(pane, &delta, false) {
            true => return,
            false => Item::Assistant(delta),
        },
        AgentStep::Reasoning(delta) => match extend(pane, &delta, true) {
            true => return,
            false => Item::Thinking(delta),
        },
        AgentStep::ToolCall { id, tool, args } => Item::ToolCall { id, tool, args },
        AgentStep::ToolResult {
            id,
            tool,
            ok,
            payload,
        } => Item::ToolResult {
            id,
            tool,
            ok,
            payload,
        },
        AgentStep::Restarted => {
            // A queued steer notice can arrive after the partial text. Work
            // backwards to the last durable task/user/tool boundary, removing
            // abandoned stream blocks while preserving that transient notice.
            let attempt_start = pane
                .iter()
                .rposition(|entry| {
                    matches!(
                        entry.item,
                        Item::User(_)
                            | Item::ToolCall { .. }
                            | Item::ToolResult { .. }
                            | Item::Compaction { .. }
                            | Item::Verify { .. }
                            | Item::AgentsDone(_)
                    )
                })
                .map(|index| index + 1)
                .unwrap_or(0);
            let mut tail = pane.split_off(attempt_start);
            tail.retain(|entry| {
                !matches!(entry.item, Item::Assistant(_) | Item::Thinking(_))
                    && !matches!(&entry.item, Item::Notice(text) if text == "the model's connection dropped — retrying")
            });
            pane.append(&mut tail);
            Item::Notice("the model's connection dropped — retrying".into())
        }
        AgentStep::SteerQueued(text) => Item::Notice(queued_notice(&text)),
        AgentStep::Steered(text) => {
            let notice = queued_notice(&text);
            if let Some(index) = pane
                .iter()
                .rposition(|entry| matches!(&entry.item, Item::Notice(value) if value == &notice))
            {
                pane.remove(index);
            }
            Item::User(text)
        }
        AgentStep::SteersReturned(texts) => {
            for text in &texts {
                let notice = queued_notice(text);
                if let Some(index) = pane.iter().rposition(
                    |entry| matches!(&entry.item, Item::Notice(value) if value == &notice),
                ) {
                    pane.remove(index);
                }
            }
            Item::Notice("queued message returned to the input box — not sent".into())
        }
    };
    append_pane_item(pane, item, MAX_AGENT_PANE_ITEMS);
}

impl Model {
    fn new(
        model: String,
        max_ctx: Option<u32>,
        reasoning: kernel::ReasoningConfig,
        ui: lockfile::UiConfig,
        tool_viz: HashMap<String, ToolViz>,
        restore: impl Into<WorkspaceView>,
    ) -> Self {
        Self {
            items: VecDeque::with_capacity(MAX_SCROLLBACK_LINES),
            tool_viz,
            input: String::new(),
            images: staged_images::Pending::default(),
            backend_deferred: None,
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
            transcript_area: Rect::default(),
            text_selection: None,
            mouse_selecting: false,
            last_click: None,
            pending_clipboard: None,
            clipboard_status: None,
            approval_ready: false,
            context_pressure: None,
            cache: None,
            cache_last_usage: None,
            cache_unreported_attempts: 0,
            cost_usd: None,
            model,
            protocol: kernel::Protocol::OpenAiChat,
            max_ctx,
            active_profile: String::new(),
            model_setup: None,
            form_generation: 0,
            command_draft: None,
            command_refused: false,
            search_setup: None,
            mcp_credential: None,
            autonomy: kernel::AutonomyLevel::Careful,
            running: false,
            session_op: None,
            pending_agent_launches: 0,
            pending_agent_steers: 0,
            current_tool: None,
            turn_started: None,
            cancelling: false,
            force_aborting: false,
            pending_approvals: VecDeque::new(),
            clarify: None,
            approval_sel: 0,
            approval_expanded: false,
            reasoning,
            reasoning_support: kernel::ReasoningSupport::Unknown,
            streaming: true,
            reasoning_received_this_turn: false,
            streamed_this_turn: 0,
            last_turn_reasoning_received: None,
            picker: None,
            ac_sel: 0,
            welcome: true,
            show_thinking: ui.show_thinking,
            full_transparency: ui.full_transparency,
            should_quit: false,
            recovery_draft: None,
            anim_frame: 0,
            intro_frame: Some(0),
            last_redraw: Instant::now(),
            pastes: Vec::new(),
            restore: restore.into(),
            bg_tasks: Vec::new(),
            compacting: false,
            show_summary: false,
            memory_enabled: true,
            plugin_command_busy: false,
            agent_runs: Vec::new(),
            agent_progress: HashMap::new(),
            agents_done: Vec::new(),
            agent_panes: HashMap::new(),
            focus: None,
            parked_main: VecDeque::new(),
            parked_scroll: HashMap::new(),
            switch_cursor: 0,
            switching: false,
            switched_before: false,
            remote: None,
        }
    }

    /// How many owned shell tasks are still running.
    fn bg_running(&self) -> usize {
        self.bg_tasks.iter().filter(|t| t.running).count()
    }

    fn attachment_chips(&self) -> Vec<String> {
        self.images.chips()
    }

    /// The approval currently rendered and answerable.
    fn pending_approval(&self) -> Option<&PendingApproval> {
        self.pending_approvals.front()
    }

    /// The declared category of a tool (from the executor specs), or `Other` if
    /// the surface hasn't been told about it.
    fn category(&self, tool: &str) -> ToolCategory {
        self.tool_viz
            .get(tool)
            .map(|v| v.category)
            .unwrap_or(ToolCategory::Other)
    }

    /// Expands paste placeholders before submission.
    fn resolve_pastes(&self, s: &str) -> Result<String, String> {
        input::expand_paste_tokens(&self.pastes, s)
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

    fn transcript_point(&self, column: u16, row: u16) -> Option<TextPoint> {
        let area = self.transcript_area;
        if column < area.x || column >= area.right() || row < area.y || row >= area.bottom() {
            return None;
        }
        let point = TextPoint {
            row: self.scroll_offset + usize::from(row - area.y),
            column: usize::from(column - area.x),
        };
        (point.row < self.total_rows).then_some(point)
    }

    fn transcript_line(&self, row: usize) -> Option<&Line<'static>> {
        let mut offset = 0usize;
        for entry in &self.items {
            let lines = entry.lines.as_ref()?;
            if row < offset + lines.len() {
                return lines.get(row - offset);
            }
            offset += lines.len();
        }
        self.approval_rows.get(row.checked_sub(offset)?)
    }

    fn selected_text(&self) -> Option<String> {
        const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

        let (start, end) = ordered_selection(self.text_selection?)?;
        let mut output = String::new();
        for row in start.row..=end.row {
            let line = self.transcript_line(row)?;
            let plain = line_text(line);
            let width = text_cell_width(&plain);
            let from = if row == start.row {
                start.column.min(width)
            } else {
                0
            };
            let to = if row == end.row {
                end.column.saturating_add(1).min(width)
            } else {
                width
            };
            if row > start.row {
                output.push('\n');
            }
            output.push_str(&slice_cells(&plain, from, to));
            if output.len() > MAX_CLIPBOARD_BYTES {
                output.truncate(floor_char_boundary(&output, MAX_CLIPBOARD_BYTES));
                break;
            }
        }
        (!output.is_empty()).then_some(output)
    }

    fn select_word(&mut self, point: TextPoint) {
        let Some(line) = self.transcript_line(point.row) else {
            return;
        };
        let Some((start, end_exclusive)) = word_cell_range(&line_text(line), point.column) else {
            return;
        };
        self.text_selection = Some(TextSelection {
            anchor: TextPoint {
                row: point.row,
                column: start,
            },
            focus: TextPoint {
                row: point.row,
                column: end_exclusive.saturating_sub(1),
            },
            non_empty: true,
        });
        self.queue_selection_copy();
    }

    fn select_line(&mut self, point: TextPoint) {
        let Some(line) = self.transcript_line(point.row) else {
            return;
        };
        let width = text_cell_width(&line_text(line));
        if width == 0 {
            return;
        }
        self.text_selection = Some(TextSelection {
            anchor: TextPoint {
                row: point.row,
                column: 0,
            },
            focus: TextPoint {
                row: point.row,
                column: width - 1,
            },
            non_empty: true,
        });
        self.queue_selection_copy();
    }

    fn queue_selection_copy(&mut self) {
        self.pending_clipboard = self.selected_text();
    }

    /// Whether the identity splash is currently visible.
    fn on_welcome_splash(&self) -> bool {
        self.welcome && self.items.is_empty() && self.pending_approvals.is_empty()
    }

    fn push_notice(&mut self, s: impl Into<String>) {
        let dropped = append_pane_notice(&mut self.items, s.into(), MAX_SCROLLBACK_LINES);
        if dropped {
            self.text_selection = None;
        }
        self.dirty = true;
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// File a conversation-owned notice into main even while a child pane is
    /// displayed. Off-screen content is rendered and scrolled only when main is
    /// opened again.
    fn push_main_notice(&mut self, s: impl Into<String>) {
        if self.focus.is_none() {
            self.push_notice(s);
            return;
        }
        append_pane_notice(&mut self.parked_main, s.into(), MAX_SCROLLBACK_LINES);
    }

    /// Remove the most recent notice starting with `prefix` — e.g. a "queued"
    /// steer marker being promoted to a real user line once it applies.
    fn remove_last_notice(&mut self, prefix: &str) {
        let showing = self.focus.is_none();
        let pane = if showing {
            &mut self.items
        } else {
            &mut self.parked_main
        };
        if let Some(idx) = pane
            .iter()
            .rposition(|e| matches!(&e.item, Item::Notice(n) if n.starts_with(prefix)))
        {
            pane.remove(idx);
            if showing {
                self.dirty = true;
            }
        }
    }

    /// Live-status notices (/tasks): when the most recent item is already a
    /// notice with this `prefix`, update it in place — re-running the command
    /// must refresh one block, not stack identical copies in the scrollback.
    fn upsert_notice(&mut self, prefix: &str, text: String) {
        if let Some(e) = self.items.back_mut()
            && matches!(&e.item, Item::Notice(n) if n.starts_with(prefix))
        {
            e.item = Item::Notice(text);
            e.invalidate();
            self.dirty = true;
            if self.auto_scroll {
                self.scroll_to_bottom();
            }
            return;
        }
        self.push_item(Item::Notice(text));
    }

    fn push_item(&mut self, item: Item) {
        // Correlated results can insert above a selection, changing its rows.
        if matches!(&item, Item::ToolResult { id: Some(_), .. }) {
            self.text_selection = None;
        }
        let dropped = append_pane_item(&mut self.items, item, MAX_SCROLLBACK_LINES);
        if dropped {
            self.text_selection = None;
        }
        self.dirty = true;
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Append a conversation-owned item without leaking it into a child pane
    /// that happens to be on screen.
    fn push_main_item(&mut self, item: Item) {
        if self.focus.is_none() {
            self.push_item(item);
        } else {
            append_pane_item(&mut self.parked_main, item, MAX_SCROLLBACK_LINES);
        }
    }

    fn push_text_delta(&mut self, delta: &str) {
        let showing = self.focus.is_none();
        let pane = if showing {
            &mut self.items
        } else {
            &mut self.parked_main
        };
        let appended = self.streamed_this_turn > 0
            && matches!(pane.back().map(|e| &e.item), Some(Item::Assistant(_)));
        if appended {
            let e = pane.back_mut().unwrap();
            if let Item::Assistant(buf) = &mut e.item {
                append_preview(buf, delta);
            }
            e.invalidate(); // only the streaming item re-renders next frame
            trim_pane(pane, MAX_SCROLLBACK_LINES);
            if showing {
                self.dirty = true;
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }
        } else {
            self.streamed_this_turn += 1;
            if showing {
                self.push_item(Item::Assistant(delta.to_string()));
            } else {
                append_pane_item(
                    &mut self.parked_main,
                    Item::Assistant(delta.to_string()),
                    MAX_SCROLLBACK_LINES,
                );
            }
        }
    }

    /// File one step into that agent's pane — the displayed collection when the
    /// agent is on screen, its parked one otherwise.
    fn push_agent_step(&mut self, path: orchestrator::AgentPath, step: AgentStep) {
        let selected = self.switch_selection();
        let settled_steers = match &step {
            AgentStep::Steered(_) => 1,
            AgentStep::SteersReturned(texts) => texts.len(),
            _ => 0,
        };
        let returned = match &step {
            AgentStep::SteersReturned(texts) => Some(texts.join("\n")),
            _ => None,
        };
        let showing = self.focus.as_ref() == Some(&path);
        if showing && matches!(&step, AgentStep::ToolResult { id: Some(_), .. }) {
            self.text_selection = None;
        }
        let pane = match showing {
            true => &mut self.items,
            false => self.agent_panes.entry(path).or_default(),
        };
        append_agent_step(pane, step);
        if showing {
            self.dirty = true;
            if self.auto_scroll {
                self.scroll_to_bottom();
            }
        }
        if let Some(returned) = returned {
            backend_ui::restore_draft(self, returned);
            self.dirty = true;
        }
        self.pending_agent_steers = self.pending_agent_steers.saturating_sub(settled_steers);
        self.trim_agent_views();
        self.reconcile_switch_cursor(selected);
    }

    /// Show `target`, parking whatever is leaving the screen where it belongs.
    ///
    /// `None` is the conversation. Scroll position travels with the pane, so
    /// looking at an agent and coming back does not lose the reader's place.
    fn focus_pane(&mut self, target: Option<orchestrator::AgentPath>) {
        if self.focus == target {
            return;
        }
        self.parked_scroll
            .insert(self.focus.clone(), (self.scroll_offset, self.auto_scroll));
        let mut leaving = std::mem::take(&mut self.items);
        // Only the displayed pane needs physical-row caches.
        for entry in &mut leaving {
            entry.invalidate();
        }
        match self.focus.take() {
            Some(path) => {
                self.agent_panes.insert(path, leaving);
            }
            None => self.parked_main = leaving,
        }
        self.items = match &target {
            Some(path) => self.agent_panes.remove(path).unwrap_or_default(),
            None => std::mem::take(&mut self.parked_main),
        };
        let restored = self.parked_scroll.get(&target).copied();
        self.focus = target;
        self.invalidate_all_renders();
        self.dirty = true;
        match restored {
            Some((offset, follow)) => {
                self.auto_scroll = follow;
                self.scroll_offset = offset;
            }
            None => {
                self.auto_scroll = true;
                self.scroll_to_bottom();
            }
        }
    }

    /// A session boundary owns a fresh set of live panes. Return to main before
    /// discarding them so the previous conversation cannot be resurrected from
    /// `parked_main` under the new session id.
    fn clear_session_panes(&mut self) {
        self.cache = None;
        self.cache_last_usage = None;
        self.cache_unreported_attempts = 0;
        self.context_pressure = None;
        self.plugin_command_busy = false;
        if self.focus.is_some() {
            self.focus_pane(None);
        }
        self.agent_panes.clear();
        self.parked_scroll.clear();
        self.agent_runs.clear();
        self.agent_progress.clear();
        self.agents_done.clear();
        self.pending_agent_launches = 0;
        self.pending_agent_steers = 0;
        self.switching = false;
        self.switch_cursor = 0;
        self.scroll_offset = 0;
        self.auto_scroll = true;
    }

    /// Keep live panes only while their agent remains in the registry's bounded
    /// running/settled window. The durable transcript remains addressable by
    /// session id, so old panes need not accumulate for the life of the TUI.
    fn retain_known_agent_panes(
        &mut self,
        known: &std::collections::HashSet<orchestrator::AgentPath>,
    ) {
        let selected = self.switch_selection();
        let focus = self.focus.clone();
        self.agent_panes
            .retain(|path, _| known.contains(path) || focus.as_ref() == Some(path));
        self.parked_scroll.retain(|path, _| match path {
            None => true,
            Some(path) => known.contains(path) || focus.as_ref() == Some(path),
        });
        self.reconcile_switch_cursor(selected);
    }

    fn trim_agent_views(&mut self) {
        while self.agent_panes.len() > 128 {
            let Some(path) = self.agent_panes.keys().min().cloned() else {
                break;
            };
            self.agent_panes.remove(&path);
            self.parked_scroll.remove(&Some(path));
        }
        while self
            .agent_panes
            .values()
            .flat_map(|pane| pane.iter())
            .map(|entry| entry.bytes)
            .sum::<usize>()
            > MAX_AGENT_BYTES
        {
            let Some(path) = self
                .agent_panes
                .iter()
                .filter(|(_, pane)| !pane.is_empty())
                .min_by_key(|(path, _)| {
                    self.agent_runs
                        .iter()
                        .find(|run| &run.path == *path)
                        .map_or(0, |run| run.started_ms)
                })
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.agent_panes.get_mut(&path).expect("pane").pop_front();
        }
    }

    fn has_active_agents(&self) -> bool {
        if self.pending_agent_launches > 0 || self.pending_agent_steers > 0 {
            return true;
        }
        if let Some(peer) = &self.remote {
            return peer.agent_requests > 0
                || self.pending_agent_launches > 0
                || self.pending_agent_steers > 0
                || peer.live.agents.iter().any(|agent| agent.status.is_none());
        }
        false
    }

    fn foreground_owned(&self) -> bool {
        self.running || self.force_aborting || self.remote.as_ref().is_some_and(|peer| peer.sending)
    }

    fn unmerged_count(&self) -> usize {
        if let Some(peer) = &self.remote {
            return peer.live.pending_patches;
        }
        0
    }

    /// The switcher's rows: the conversation first, then every agent this session
    /// knows about, so `main` is a destination like any other.
    fn switch_rows(&self) -> Vec<Option<orchestrator::AgentPath>> {
        let mut rows = vec![None];
        rows.extend(self.agent_runs.iter().map(|run| Some(run.path.clone())));
        // A settled live pane remains useful until the session ends. Previously
        // it stayed allocated but disappeared from this list as soon as the user
        // returned to main, making its contents permanently unreachable.
        let mut parked: Vec<_> = self.agent_panes.keys().cloned().collect();
        parked.sort();
        for path in parked {
            let row = Some(path);
            if !rows.contains(&row) {
                rows.push(row);
            }
        }
        // Whatever is on screen is always a row, including the instant between
        // settlement and parking it back into `agent_panes`.
        if self.focus.is_some() && !rows.contains(&self.focus) {
            rows.push(self.focus.clone());
        }
        rows
    }

    fn switch_selection(&self) -> Option<Option<orchestrator::AgentPath>> {
        self.switching
            .then(|| self.switch_rows().get(self.switch_cursor).cloned())
            .flatten()
    }

    /// Keep the switcher cursor attached to a path while live/parked rows are
    /// inserted or removed underneath it.
    fn reconcile_switch_cursor(&mut self, selected: Option<Option<orchestrator::AgentPath>>) {
        if !self.switching {
            return;
        }
        let rows = self.switch_rows();
        self.switch_cursor = selected
            .and_then(|target| rows.iter().position(|row| row == &target))
            .or_else(|| rows.iter().position(|row| row == &self.focus))
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
    }

    fn push_thinking_delta(&mut self, delta: &str) {
        self.reasoning_received_this_turn = true;
        let showing = self.focus.is_none();
        let pane = if showing {
            &mut self.items
        } else {
            &mut self.parked_main
        };
        let appended = self.streamed_this_turn > 0
            && matches!(pane.back().map(|e| &e.item), Some(Item::Thinking(_)));
        if appended {
            let e = pane.back_mut().unwrap();
            if let Item::Thinking(buf) = &mut e.item {
                append_preview(buf, delta);
            }
            e.invalidate();
            trim_pane(pane, MAX_SCROLLBACK_LINES);
            if showing {
                self.dirty = true;
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }
        } else {
            self.streamed_this_turn += 1;
            if showing {
                self.push_item(Item::Thinking(delta.to_string()));
            } else {
                append_pane_item(
                    &mut self.parked_main,
                    Item::Thinking(delta.to_string()),
                    MAX_SCROLLBACK_LINES,
                );
            }
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
        self.text_selection = None;
        self.dirty = true;
    }

    // Editing maintains `cursor` on a UTF-8 character boundary.

    fn edited(&self) -> (&str, usize) {
        let editor = self
            .mcp_credential
            .as_ref()
            .map(|form| &form.editor)
            .or_else(|| self.model_setup.as_ref().map(|form| &form.editor))
            .or_else(|| self.search_setup.as_ref().map(|form| &form.editor));
        editor.map_or((&self.input, self.cursor), |editor| {
            (&editor.text, editor.cursor)
        })
    }

    fn edited_mut(&mut self) -> (&mut String, &mut usize) {
        if let Some(form) = &mut self.mcp_credential {
            return (&mut form.editor.text, &mut form.editor.cursor);
        }
        if let Some(form) = &mut self.model_setup {
            return (&mut form.editor.text, &mut form.editor.cursor);
        }
        if let Some(form) = &mut self.search_setup {
            return (&mut form.editor.text, &mut form.editor.cursor);
        }
        (&mut self.input, &mut self.cursor)
    }

    fn clear_edited(&mut self) {
        let (text, cursor) = self.edited_mut();
        text.clear();
        *cursor = 0;
    }

    fn insert_char(&mut self, c: char) {
        if !self.can_insert(c.len_utf8()) {
            return;
        }
        let (text, cursor) = self.edited_mut();
        text.insert(*cursor, c);
        *cursor += c.len_utf8();
    }

    fn insert_text(&mut self, s: &str) {
        if !self.can_insert(s.len()) {
            return;
        }
        let (text, cursor) = self.edited_mut();
        text.insert_str(*cursor, s);
        *cursor += s.len();
    }

    fn secret_input(&self) -> bool {
        self.model_setup.as_ref().is_some_and(ModelSetup::is_secret)
            || self
                .search_setup
                .as_ref()
                .is_some_and(SearchSetup::is_secret)
            || self.mcp_credential.is_some()
    }

    fn can_insert(&mut self, bytes: usize) -> bool {
        let form = self.model_setup.is_some()
            || self.search_setup.is_some()
            || self.mcp_credential.is_some();
        let used = self.edited().0.len()
            + if form {
                0
            } else {
                self.pastes.iter().map(String::len).sum::<usize>()
            };
        let limit = if self.secret_input() {
            MAX_SECRET_BYTES
        } else {
            MAX_COMPOSER_BYTES
        };
        if bytes > limit.saturating_sub(used) {
            self.push_notice("Input is too large. Split it into smaller messages; the existing draft is preserved.");
            false
        } else {
            true
        }
    }

    fn backspace(&mut self) {
        let (text, cursor) = self.edited_mut();
        if let Some(c) = text[..*cursor].chars().next_back() {
            *cursor -= c.len_utf8();
            text.remove(*cursor);
        }
    }

    fn move_left(&mut self) {
        let (text, cursor) = self.edited_mut();
        if let Some(c) = text[..*cursor].chars().next_back() {
            *cursor -= c.len_utf8();
        }
    }

    fn move_right(&mut self) {
        let (text, cursor) = self.edited_mut();
        if let Some(c) = text[*cursor..].chars().next() {
            *cursor += c.len_utf8();
        }
    }
}

#[cfg(test)]
mod pane_tests;

#[cfg(test)]
mod tests {
    use super::input::{expand_paste_tokens, strip_paste_markers};
    use super::*;
    #[test]
    fn agent_commands_are_recognised_so_they_survive_a_running_turn() {
        assert!(super::is_slash_command("/steer tokio-research narrow it"));
        assert!(super::is_slash_command("/agents"));
        // Not a command: real prose must still reach the model as a steer.
        assert!(!super::is_slash_command("why is it stuck here"));
        assert!(!super::is_slash_command("/Users/me/notes.txt read this"));
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn block(lines: &[Line]) -> String {
        lines.iter().map(text).collect::<Vec<_>>().join("\n")
    }

    fn test_sbx() -> Arc<WorkspaceSandbox> {
        Arc::new(WorkspaceSandbox::new_jailed(std::env::temp_dir()).unwrap())
    }

    #[test]
    fn transcript_selection_uses_terminal_cells_for_wide_text() {
        assert_eq!(text_cell_width("a界b"), 4);
        assert_eq!(slice_cells("a界b", 1, 3), "界");
        assert_eq!(slice_cells("a界b", 2, 3), "界");
    }

    #[test]
    fn double_click_word_range_includes_source_identifiers() {
        assert_eq!(word_cell_range("call memory_search now", 8), Some((5, 18)));
        assert_eq!(word_cell_range("ok!", 2), Some((2, 3)));
    }

    #[test]
    fn an_unmoved_click_is_not_a_copy_selection() {
        let point = TextPoint { row: 4, column: 2 };
        assert!(
            ordered_selection(TextSelection {
                anchor: point,
                focus: point,
                non_empty: false,
            })
            .is_none()
        );
        assert!(
            ordered_selection(TextSelection {
                anchor: point,
                focus: point,
                non_empty: true,
            })
            .is_some(),
            "double-clicking a one-cell word is still a real selection"
        );
    }

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
        // Shell-side mutations without snapshots cannot be rolled back.
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

    #[test]
    fn approval_renders_heading_and_three_plain_options() {
        let lines = render_approval(
            "fs_write",
            Some("+ added line\n- removed line"),
            0,
            &["Yes, allow once", "Yes, always allow", "No, deny"],
            false,
        );
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
        let opts = &["Yes, allow once", "Yes, always allow", "No, deny"];
        let sel0 = block(&render_approval("fs_write", None, 0, opts, false));
        let sel2 = block(&render_approval("fs_write", None, 2, opts, false));
        // The accent marker sits on the selected option's line.
        assert!(sel0.contains("▌ 1. Yes, allow once"));
        assert!(sel2.contains("▌ 3. No, deny"));
    }

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

    #[test]
    fn diff_collapses_unchanged_runs_into_gaps() {
        let old: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        let mut new = old.clone();
        new[20] = "line 20 CHANGED".to_string();
        let rows = hunk_rows(&old.join("\n"), &new.join("\n"));
        assert!(
            rows.iter().any(|r| matches!(r, DiffRow::Gap(n) if *n > 0)),
            "expected a gap marker"
        );
        assert!(
            rows.iter()
                .any(|r| matches!(r, DiffRow::Ins(_, t) if t.contains("CHANGED")))
        );
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
        let line = Line::from(Span::styled(
            "abcdefghij",
            Style::default().fg(theme::text()),
        ));
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
        assert!(is_slash_command("/lsp"));
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
        m.upsert_notice("shell tasks", "shell tasks:\n  t1 [running] find".into());
        m.upsert_notice("shell tasks", "shell tasks:\n  t1 [done] find".into());
        let notices: Vec<&Item> = m.items.iter().map(|e| &e.item).collect();
        assert_eq!(
            notices.len(),
            1,
            "re-running /tasks must refresh, not stack"
        );
        assert!(matches!(notices[0], Item::Notice(n) if n.contains("[done]")));
        // A different item in between → a fresh block is appended, not merged.
        m.push_item(Item::User("hi".into()));
        m.upsert_notice("shell tasks", "shell tasks: none".into());
        assert_eq!(m.items.len(), 3);
    }

    #[test]
    fn streaming_incremental_render_matches_full_render() {
        // Cached streaming rows must match a full render at every step.
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
        // Width changes invalidate cached layout.
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
        // Four CJK glyphs occupy eight terminal cells.
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
            ("edit".to_string(), viz("✎", ToolCategory::Write)),
            ("read".to_string(), viz("◇", ToolCategory::Read)),
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
        // A streamed argument is being prepared; the file has not been written yet.
        m.current_tool = Some(("edit".into(), Some("/Users/x/medha/medha.html".into())));
        assert_eq!(activity_label(&m), "preparing edit medha.html");
        // Name-only (target not sniffed yet) still shows the verb.
        m.current_tool = Some(("read".into(), None));
        assert_eq!(activity_label(&m), "preparing read");
        m.current_tool = Some(("shell.exec".into(), Some("cargo build".into())));
        assert_eq!(activity_label(&m), "preparing shell.exec cargo build");
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

    #[test]
    fn strip_markers_removes_guards_but_keeps_content() {
        // Digits resembling guard markers remain content.
        let raw = "\u{1b}[200~code[200] = 0~ok\u{1b}[201~";
        assert_eq!(strip_paste_markers(raw), "code[200] = 0~ok");
    }

    #[test]
    fn expand_tokens_round_trips_and_ignores_plain_text() {
        let pastes = vec!["FULL CONTENT".to_string()];
        assert_eq!(
            expand_paste_tokens(&pastes, "before [paste #0: 12 chars] after").unwrap(),
            "before FULL CONTENT after"
        );
        // A bare bracket that isn't a real token is left untouched.
        assert_eq!(
            expand_paste_tokens(&pastes, "arr[0] = 1").unwrap(),
            "arr[0] = 1"
        );
        // No pastes → identity.
        assert_eq!(
            expand_paste_tokens(&[], "[paste #0: 5 chars]").unwrap(),
            "[paste #0: 5 chars]"
        );
        assert_eq!(
            expand_paste_tokens(&pastes, "[paste #99: 12 chars] literal").unwrap(),
            "[paste #99: 12 chars] literal"
        );
    }

    #[test]
    fn repeated_paste_references_cannot_amplify_a_small_composer_without_bound() {
        let pastes = vec!["x".repeat(MAX_COMPOSER_BYTES / 2)];
        assert!(
            expand_paste_tokens(
                &pastes,
                "[paste #0: many chars][paste #0: many chars][paste #0: many chars]"
            )
            .is_err()
        );
    }
}
