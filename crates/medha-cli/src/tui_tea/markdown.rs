//! Markdown → ratatui rendering.
//!
//! `pulldown-cmark` (the rustdoc/mdBook parser) does the parsing; we own the
//! presentation so every block styles from the active [`super::theme`] palette
//! and adapts to light/dark. No off-the-shelf renderer fit our constraints:
//! `tui-markdown` drops tables and hardcodes colours, `termimad` owns its own
//! draw loop (fights our virtualized scroll buffer), and `ratatui-markdown`
//! ships a non-standard licence. Layering a battle-tested parser under a thin
//! renderer we control is the same pattern bat/delta/mdcat/glow use.
//!
//! Output contract: logical [`Line`]s. Paragraph/heading/list/quote lines are
//! emitted *unwrapped* — the caller's `wrap_line` folds them to the pane width.
//! Tables and horizontal rules are pre-laid-out to `width` (cells wrapped inside
//! their columns) so `wrap_line`'s fast-path passes them through untouched and
//! the grid never breaks. Code-fence lines are highlighted per language via
//! `syntect`; over-long code lines are left for `wrap_line` (safe, not clipped).

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;
use unicode_width::UnicodeWidthStr;

use super::theme;

/// Render a markdown document to logical lines at the given pane `width`.
pub(super) fn render(src: &str, width: u16) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    // Recognize `$…$` / `$$…$$` so LaTeX math becomes InlineMath/DisplayMath
    // events we can transliterate, instead of the raw `$$\text{…}$$` leaking
    // through as literal text (a terminal can't typeset real math).
    opts.insert(Options::ENABLE_MATH);
    let parser = Parser::new_ext(src, opts);
    let mut r = Renderer::new(width);
    for ev in parser {
        r.event(ev);
    }
    r.finish()
}

/// One inline styling layer on the stack (bold/italic/code/link…). Kept as
/// additive deltas so nested spans (bold-inside-link) compose correctly.
#[derive(Clone, Copy, Default)]
struct Inline {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link: bool,
}

struct ListFrame {
    /// `Some(n)` for an ordered list counting from n; `None` for a bullet list.
    ordered: Option<u64>,
}

struct Renderer {
    width: u16,
    out: Vec<Line<'static>>,
    /// Spans accumulated for the line currently being built.
    cur: Vec<Span<'static>>,
    inline: Inline,
    lists: Vec<ListFrame>,
    quote_depth: usize,
    /// Pending task-list checkbox marker for the next list item's first text.
    /// Code-fence capture: language + collected source, set between Start/End.
    code: Option<(String, String)>,
    /// Table capture.
    table: Option<TableBuilder>,
    /// True while inside a heading (so text is styled as a heading).
    heading: Option<HeadingLevel>,
    /// Deferred link destination, rendered dimmed after the link text.
    link_dest: Option<String>,
    /// Whether the current list item has emitted its bullet/number yet.
    need_marker: bool,
}

struct TableBuilder {
    aligns: Vec<Alignment>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    in_header: bool,
    cur_cell: String,
    cur_row: Vec<String>,
}

impl Renderer {
    fn new(width: u16) -> Self {
        Self {
            width,
            out: Vec::new(),
            cur: Vec::new(),
            inline: Inline::default(),
            lists: Vec::new(),
            quote_depth: 0,
            code: None,
            table: None,
            heading: None,
            link_dest: None,
            need_marker: false,
        }
    }

    /// The base text style for the current context (heading vs quote vs body).
    fn base_style(&self) -> Style {
        if self.heading.is_some() {
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD)
        } else if self.quote_depth > 0 {
            Style::default()
                .fg(theme::quote())
                .add_modifier(Modifier::ITALIC)
        } else {
            Style::default().fg(theme::text())
        }
    }

    /// Apply the active inline layer to the base style.
    fn styled(&self) -> Style {
        let mut s = self.base_style();
        let i = self.inline;
        if i.code {
            s = Style::default().fg(theme::code_fg()).bg(theme::code_bg());
        }
        if i.link {
            s = s.fg(theme::link()).add_modifier(Modifier::UNDERLINED);
        }
        if i.bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        if i.italic {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if i.strike {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        s
    }

    /// The left-margin spans (quote bars + list indentation) for a fresh line.
    fn indent_spans(&self) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for _ in 0..self.quote_depth {
            spans.push(Span::styled("▏ ", Style::default().fg(theme::border())));
        }
        // Two columns of indent per nested list level (the marker for the
        // deepest level is added separately).
        let depth = self.lists.len().saturating_sub(1);
        if depth > 0 {
            spans.push(Span::raw("  ".repeat(depth)));
        }
        spans
    }

    fn push_text(&mut self, t: &str) {
        if self.cur.is_empty() {
            let indent = self.indent_spans();
            self.cur.extend(indent);
            self.emit_marker_if_needed();
        }
        self.cur.push(Span::styled(t.to_string(), self.styled()));
    }

    /// Emit the list bullet/number once, at the first text of an item.
    fn emit_marker_if_needed(&mut self) {
        if !self.need_marker {
            return;
        }
        self.need_marker = false;
        if let Some(frame) = self.lists.last_mut() {
            match &mut frame.ordered {
                Some(n) => {
                    let label = format!("{n}. ");
                    *n += 1;
                    self.cur
                        .push(Span::styled(label, Style::default().fg(theme::accent())));
                }
                None => {
                    // Alternate bullet glyph by depth for visual nesting.
                    let glyph = if self.lists.len() % 2 == 0 {
                        "◦ "
                    } else {
                        "• "
                    };
                    self.cur.push(Span::styled(
                        glyph.to_string(),
                        Style::default().fg(theme::accent()),
                    ));
                }
            }
        }
    }

    /// Flush the in-progress line to output (even if empty, to preserve blanks).
    fn flush_line(&mut self) {
        let spans = std::mem::take(&mut self.cur);
        self.out.push(Line::from(spans));
    }

    /// End the current line only if it has content.
    fn break_line(&mut self) {
        if !self.cur.is_empty() {
            self.flush_line();
        }
    }

    /// Ensure a blank separator before a new block, but never two in a row and
    /// never as the very first line.
    fn blank(&mut self) {
        if self.out.last().map(|l| line_is_blank(l)).unwrap_or(true) {
            return;
        }
        self.out.push(Line::default());
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.write_inline(&t, false),
            Event::Code(t) => self.write_inline(&t, true),
            Event::SoftBreak | Event::HardBreak => {
                if self.code.is_some() {
                    // handled in code accumulation
                } else if self.table.is_some() {
                    // ignore inside cells
                } else {
                    self.break_line();
                }
            }
            Event::Rule => {
                self.blank();
                let w = self.width.max(1) as usize;
                self.out.push(Line::from(Span::styled(
                    "─".repeat(w),
                    Style::default().fg(theme::border()),
                )));
                self.blank();
            }
            Event::TaskListMarker(done) => {
                // Replace the pending bullet with a checkbox glyph.
                self.need_marker = false;
                if self.cur.is_empty() {
                    let indent = self.indent_spans();
                    self.cur.extend(indent);
                }
                let (glyph, color) = if done {
                    ("✔ ", theme::ok())
                } else {
                    ("○ ", theme::dim())
                };
                self.cur.push(Span::styled(
                    glyph.to_string(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                // Show raw HTML rather than dropping it silently.
                self.write_inline(&h, false);
            }
            Event::FootnoteReference(name) => {
                self.write_inline(&format!("[^{name}]"), false);
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                // Terminals can't typeset math; transliterate LaTeX to readable
                // Unicode so a flow like `$$\text{A}\longrightarrow\text{B}$$`
                // reads as `A ⟶ B` instead of raw markup.
                self.write_inline(&math_to_unicode(&t), false);
            }
        }
    }

    /// The single sink for every inline text event. Routing lives HERE and
    /// nowhere else, so inline code / html / math inside a table cell or code
    /// fence is captured into that buffer instead of leaking onto the main
    /// transcript line (the bug where `` `file.rs` `` in a cell printed itself
    /// below the table). Precedence: code fence → table cell → current line.
    fn write_inline(&mut self, text: &str, code: bool) {
        if let Some((_, buf)) = self.code.as_mut() {
            buf.push_str(text);
            return;
        }
        if let Some(tb) = self.table.as_mut() {
            // Cells are plain text; capture the content (code, emphasis, etc.
            // collapse to text) so it stays inside the grid.
            tb.cur_cell.push_str(text);
            return;
        }
        if code {
            let prev = self.inline.code;
            self.inline.code = true;
            self.push_text(text);
            self.inline.code = prev;
        } else {
            self.push_text(text);
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.table.is_none() {
                    self.blank();
                }
            }
            Tag::Heading { level, .. } => {
                self.blank();
                self.heading = Some(level);
                // A leading bar makes H1/H2 unmistakable when scrolling.
                if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                    self.cur
                        .push(Span::styled("▌ ", Style::default().fg(theme::accent())));
                }
            }
            Tag::BlockQuote(_) => {
                self.break_line();
                self.blank();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.break_line();
                self.blank();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.break_line();
                if self.lists.is_empty() {
                    self.blank();
                }
                self.lists.push(ListFrame { ordered: start });
            }
            Tag::Item => {
                self.break_line();
                self.need_marker = true;
            }
            Tag::Emphasis => self.inline.italic = true,
            Tag::Strong => self.inline.bold = true,
            Tag::Strikethrough => self.inline.strike = true,
            Tag::Link { dest_url, .. } => {
                self.inline.link = true;
                self.link_dest = Some(dest_url.to_string());
            }
            Tag::Image { dest_url, .. } => {
                // No inline images in a TUI — show a labelled placeholder.
                // Routed through write_inline so it lands in the active sink
                // (e.g. a table cell) instead of leaking onto the main line.
                self.write_inline("🖼 ", false);
                self.link_dest = Some(dest_url.to_string());
                self.inline.link = true;
            }
            Tag::Table(aligns) => {
                self.break_line();
                self.blank();
                self.table = Some(TableBuilder {
                    aligns,
                    header: Vec::new(),
                    rows: Vec::new(),
                    in_header: false,
                    cur_cell: String::new(),
                    cur_row: Vec::new(),
                });
            }
            Tag::TableHead => {
                if let Some(tb) = self.table.as_mut() {
                    tb.in_header = true;
                }
            }
            Tag::TableRow => {
                if let Some(tb) = self.table.as_mut() {
                    tb.cur_row = Vec::new();
                }
            }
            Tag::TableCell => {
                if let Some(tb) = self.table.as_mut() {
                    tb.cur_cell = String::new();
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.break_line(),
            TagEnd::Heading(_) => {
                self.break_line();
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.break_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some((lang, buf)) = self.code.take() {
                    self.emit_code_block(&lang, &buf);
                }
            }
            TagEnd::List(_) => {
                self.break_line();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => self.break_line(),
            TagEnd::Emphasis => self.inline.italic = false,
            TagEnd::Strong => self.inline.bold = false,
            TagEnd::Strikethrough => self.inline.strike = false,
            TagEnd::Link | TagEnd::Image => {
                // Append the destination so the target is visible. In a table
                // cell it goes into the cell as text (no leak); elsewhere it's a
                // dimmed span after the link text.
                if let Some(dest) = self.link_dest.take() {
                    if !dest.is_empty() && !dest.starts_with('#') {
                        if let Some(tb) = self.table.as_mut() {
                            tb.cur_cell.push_str(&format!(" ({dest})"));
                        } else {
                            self.cur.push(Span::styled(
                                format!(" ({dest})"),
                                Style::default().fg(theme::faint()),
                            ));
                        }
                    }
                }
                self.inline.link = false;
            }
            TagEnd::Table => {
                if let Some(tb) = self.table.take() {
                    self.emit_table(tb);
                }
            }
            TagEnd::TableHead => {
                if let Some(tb) = self.table.as_mut() {
                    let row = std::mem::take(&mut tb.cur_row);
                    tb.header = row;
                    tb.in_header = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(tb) = self.table.as_mut() {
                    let row = std::mem::take(&mut tb.cur_row);
                    if !tb.in_header {
                        tb.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(tb) = self.table.as_mut() {
                    let cell = std::mem::take(&mut tb.cur_cell);
                    tb.cur_row.push(cell.trim().to_string());
                }
            }
            _ => {}
        }
    }

    fn emit_code_block(&mut self, lang: &str, src: &str) {
        let border = Style::default().fg(theme::border());
        // Language tag header line.
        let label = if lang.is_empty() { "code" } else { lang };
        self.out.push(Line::from(vec![
            Span::styled("╭─ ", border),
            Span::styled(
                label.to_string(),
                Style::default()
                    .fg(theme::dim())
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        let highlighted = highlight(lang, src);
        let src_no_trailing = src.strip_suffix('\n').unwrap_or(src);
        match highlighted {
            Some(rows) => {
                for spans in rows {
                    let mut line = vec![Span::styled("│ ", border)];
                    line.extend(spans);
                    self.out.push(Line::from(line));
                }
            }
            None => {
                for raw in src_no_trailing.split('\n') {
                    self.out.push(Line::from(vec![
                        Span::styled("│ ", border),
                        Span::styled(raw.to_string(), Style::default().fg(theme::code_fg())),
                    ]));
                }
            }
        }
        self.out.push(Line::from(Span::styled("╰─", border)));
        self.blank();
    }

    fn emit_table(&mut self, tb: TableBuilder) {
        let ncols = tb
            .header
            .len()
            .max(tb.rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }
        let get = |row: &[String], c: usize| row.get(c).cloned().unwrap_or_default();

        // Natural column widths = widest cell (header + body), display cells.
        let mut widths = vec![0usize; ncols];
        for (c, w) in widths.iter_mut().enumerate() {
            *w = (*w).max(display_width(&get(&tb.header, c)));
            for row in &tb.rows {
                *w = (*w).max(display_width(&get(row, c)));
            }
            *w = (*w).max(3); // room for the alignment dashes
        }

        // Fit to the pane: borders take `3*ncols + 1` columns (│ pad … │).
        let avail = self.width as usize;
        let overhead = 3 * ncols + 1;
        let budget = avail.saturating_sub(overhead).max(ncols);
        let total: usize = widths.iter().sum();
        if total > budget {
            // Shrink proportionally, keeping each column ≥ 3 cells.
            let mut remaining = budget;
            let n = ncols;
            for w in widths.iter_mut() {
                let share = if total == 0 {
                    budget / n
                } else {
                    *w * budget / total
                };
                *w = share.max(3);
                remaining = remaining.saturating_sub(*w);
            }
            // Hand any rounding leftover to the first column.
            if remaining > 0 {
                widths[0] += remaining;
            }
        }

        let border = Style::default().fg(theme::border());
        let rule = |left: &str, mid: &str, right: &str, widths: &[usize]| -> Line<'static> {
            let mut s = String::from(left);
            for (c, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(w + 2));
                s.push_str(if c + 1 == widths.len() { right } else { mid });
            }
            Line::from(Span::styled(s, border))
        };

        self.out.push(rule("┌", "┬", "┐", &widths));
        self.emit_table_row(&tb.header, &widths, &tb.aligns, true, border);
        self.out.push(rule("├", "┼", "┤", &widths));
        for row in &tb.rows {
            self.emit_table_row(row, &widths, &tb.aligns, false, border);
        }
        self.out.push(rule("└", "┴", "┘", &widths));
        self.blank();
    }

    fn emit_table_row(
        &mut self,
        row: &[String],
        widths: &[usize],
        aligns: &[Alignment],
        header: bool,
        border: Style,
    ) {
        // Wrap each cell to its column width; a row is as tall as its tallest cell.
        let cells: Vec<Vec<String>> = widths
            .iter()
            .enumerate()
            .map(|(c, &w)| {
                let text = row.get(c).cloned().unwrap_or_default();
                wrap_cell(&text, w)
            })
            .collect();
        let height = cells.iter().map(|c| c.len()).max().unwrap_or(1).max(1);
        let cell_style = if header {
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::text())
        };
        for line_idx in 0..height {
            let mut spans = vec![Span::styled("│", border)];
            for (c, &w) in widths.iter().enumerate() {
                let content = cells[c].get(line_idx).cloned().unwrap_or_default();
                let align = aligns.get(c).copied().unwrap_or(Alignment::None);
                let padded = pad_cell(&content, w, align);
                spans.push(Span::raw(" "));
                spans.push(Span::styled(padded, cell_style));
                spans.push(Span::styled(" │", border));
            }
            self.out.push(Line::from(spans));
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.break_line();
        // Trim leading/trailing blank lines so a message doesn't float.
        while self.out.first().map(line_is_blank).unwrap_or(false) {
            self.out.remove(0);
        }
        while self.out.last().map(line_is_blank).unwrap_or(false) {
            self.out.pop();
        }
        self.out
    }
}

fn line_is_blank(l: &Line<'_>) -> bool {
    l.spans.iter().all(|s| s.content.trim().is_empty())
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Greedy word-wrap a table cell to `width` display cells.
fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if display_width(text) <= width {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    let mut line_w = 0usize;
    for word in text.split_whitespace() {
        let ww = display_width(word);
        if line_w == 0 {
            // First word on the line; hard-split if it alone exceeds width.
            if ww <= width {
                line.push_str(word);
                line_w = ww;
            } else {
                for ch in word.chars() {
                    let cw = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]) as &str);
                    if line_w + cw > width {
                        out.push(std::mem::take(&mut line));
                        line_w = 0;
                    }
                    line.push(ch);
                    line_w += cw;
                }
            }
        } else if line_w + 1 + ww <= width {
            line.push(' ');
            line.push_str(word);
            line_w += 1 + ww;
        } else {
            out.push(std::mem::take(&mut line));
            line.clear();
            line_w = 0;
            if ww <= width {
                line.push_str(word);
                line_w = ww;
            } else {
                for ch in word.chars() {
                    let cw = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]) as &str);
                    if line_w + cw > width {
                        out.push(std::mem::take(&mut line));
                        line_w = 0;
                    }
                    line.push(ch);
                    line_w += cw;
                }
            }
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

/// Pad (and align) a cell's text to exactly `width` display cells.
fn pad_cell(s: &str, width: usize, align: Alignment) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    let pad = width - w;
    match align {
        Alignment::Right => format!("{}{}", " ".repeat(pad), s),
        Alignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
        }
        _ => format!("{}{}", s, " ".repeat(pad)),
    }
}

// ---- syntect code-fence highlighting -------------------------------------

struct Highlighter {
    syntaxes: syntect::parsing::SyntaxSet,
    themes: syntect::highlighting::ThemeSet,
}

fn highlighter() -> &'static Highlighter {
    static H: OnceLock<Highlighter> = OnceLock::new();
    H.get_or_init(|| Highlighter {
        syntaxes: syntect::parsing::SyntaxSet::load_defaults_newlines(),
        themes: syntect::highlighting::ThemeSet::load_defaults(),
    })
}

/// Highlight `src` as `lang` into per-line spans, falling back to plain
/// code-coloured text when the language is unknown. Public so the tool
/// input/output panels render with the same code styling as markdown fences.
pub(super) fn highlight_lines(lang: &str, src: &str) -> Vec<Vec<Span<'static>>> {
    if let Some(rows) = highlight(lang, src) {
        rows
    } else {
        src.split('\n')
            .map(|l| {
                vec![Span::styled(
                    l.to_string(),
                    Style::default().fg(theme::code_fg()),
                )]
            })
            .collect()
    }
}

/// Highlight `src` as `lang`, returning per-line ratatui spans. `None` when the
/// language is unknown or highlighting is unavailable (caller falls back to
/// plain code styling).
fn highlight(lang: &str, src: &str) -> Option<Vec<Vec<Span<'static>>>> {
    use syntect::easy::HighlightLines;
    use syntect::util::LinesWithEndings;

    // Syntax definitions may backtrack heavily on adversarial/generated input.
    // Large fences still render as plain code; they simply skip highlighting.
    const MAX_HIGHLIGHT_BYTES: usize = 256 * 1024;
    if lang.trim().is_empty() || src.len() > MAX_HIGHLIGHT_BYTES {
        return None;
    }
    let h = highlighter();
    let syntax = h
        .syntaxes
        .find_syntax_by_token(lang)
        .or_else(|| h.syntaxes.find_syntax_by_extension(lang))?;
    let theme_name = theme::current().syntect_theme;
    let theme = h.themes.themes.get(theme_name)?;
    let mut hl = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(src) {
        let ranges = hl.highlight_line(line, &h.syntaxes).ok()?;
        let mut spans = Vec::new();
        for (style, piece) in ranges {
            let text = piece.trim_end_matches(['\n', '\r']);
            if text.is_empty() {
                continue;
            }
            let fg = ratatui::style::Color::Rgb(
                style.foreground.r,
                style.foreground.g,
                style.foreground.b,
            );
            spans.push(Span::styled(text.to_string(), Style::default().fg(fg)));
        }
        out.push(spans);
    }
    Some(out)
}

// ── math transliteration ─────────────────────────────────────────────────────

/// LaTeX-ish math → readable Unicode for a terminal (which has no math
/// typesetting). Not a real renderer: it strips formatting wrappers (`\text{}`,
/// grouping braces), maps common commands to Unicode symbols, and turns simple
/// super/subscripts into Unicode, so `\text{A}\longrightarrow\text{B}` reads as
/// `A ⟶ B` instead of raw markup. Unknown commands degrade to their bare name.
fn math_to_unicode(src: &str) -> String {
    const MAX_MATH_CHARS: usize = 32 * 1024;
    let chars: Vec<char> = src.chars().take(MAX_MATH_CHARS).collect();
    let mut i = 0;
    let raw = convert_math(&chars, &mut i, false, 0);
    // Collapse the whitespace that stripped commands/braces leave behind.
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Formatting-only wrappers whose braces are dropped but content kept.
const MATH_WRAPPERS: &[&str] = &[
    "text",
    "textrm",
    "textbf",
    "textit",
    "texttt",
    "mathrm",
    "mathbf",
    "mathit",
    "mathsf",
    "mathtt",
    "mathcal",
    "mathbb",
    "mathfrak",
    "boldsymbol",
    "operatorname",
];

/// `\command` (without the backslash) → Unicode symbol.
fn math_symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // arrows
        "longrightarrow" | "implies" => "⟶",
        "rightarrow" | "to" => "→",
        "Rightarrow" => "⇒",
        "longleftarrow" => "⟵",
        "leftarrow" | "gets" => "←",
        "Leftarrow" => "⇐",
        "leftrightarrow" => "↔",
        "Leftrightarrow" | "iff" => "⇔",
        "longleftrightarrow" => "⟷",
        "mapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        // operators
        "times" => "×",
        "cdot" => "·",
        "div" => "÷",
        "pm" => "±",
        "mp" => "∓",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "•",
        "oplus" => "⊕",
        "otimes" => "⊗",
        // relations
        "leq" | "le" => "≤",
        "geq" | "ge" => "≥",
        "neq" | "ne" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "cong" => "≅",
        "sim" => "∼",
        "simeq" => "≃",
        "propto" => "∝",
        "ll" => "≪",
        "gg" => "≫",
        "ratio" => "∶",
        // sets & logic
        "in" => "∈",
        "notin" => "∉",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "supset" => "⊃",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "setminus" => "∖",
        "emptyset" | "varnothing" => "∅",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "neg" | "lnot" => "¬",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        // misc
        "infty" => "∞",
        "partial" => "∂",
        "nabla" => "∇",
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "oint" => "∮",
        "cdots" => "⋯",
        "ldots" | "dots" => "…",
        "vdots" => "⋮",
        "angle" => "∠",
        "perp" => "⊥",
        "parallel" => "∥",
        "hbar" => "ℏ",
        "ell" => "ℓ",
        "aleph" => "ℵ",
        "degree" => "°",
        "prime" => "′",
        "quad" | "qquad" | "," | ";" | ":" | "!" => " ",
        // lowercase Greek
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" | "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" | "vartheta" => "θ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" | "varpi" => "π",
        "rho" | "varrho" => "ρ",
        "sigma" | "varsigma" => "σ",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" | "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        // uppercase Greek
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        _ => return None,
    })
}

/// Convert until end of input, or (when `in_group`) until the matching `}`,
/// which is consumed. Grouping braces are dropped; their content is kept.
const MAX_MATH_DEPTH: usize = 128;

fn convert_math(chars: &[char], i: &mut usize, in_group: bool, depth: usize) -> String {
    if depth >= MAX_MATH_DEPTH {
        let remainder = chars[*i..].iter().collect();
        *i = chars.len();
        return remainder;
    }
    let mut out = String::new();
    while *i < chars.len() {
        match chars[*i] {
            '}' if in_group => {
                *i += 1;
                break;
            }
            '{' => {
                *i += 1;
                out.push_str(&convert_math(chars, i, true, depth + 1));
            }
            '\\' => {
                *i += 1;
                out.push_str(&math_command(chars, i, depth));
            }
            '^' => {
                *i += 1;
                out.push_str(&math_script(chars, i, true, depth));
            }
            '_' => {
                *i += 1;
                out.push_str(&math_script(chars, i, false, depth));
            }
            '$' => *i += 1, // stray delimiter
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
    out
}

/// Handle one `\…` control sequence. `i` points just past the backslash.
fn math_command(chars: &[char], i: &mut usize, depth: usize) -> String {
    // Control symbol (non-letter): `\\` line break, escaped `{`/`}`/`$`/`%`/`#`,
    // or thin-space punctuation like `\,`.
    if *i < chars.len() && !chars[*i].is_ascii_alphabetic() {
        let c = chars[*i];
        *i += 1;
        return match c {
            '\\' => " ".into(),
            '{' | '}' | '$' | '%' | '#' | '&' | '_' => c.to_string(),
            ',' | ';' | ':' | '!' | ' ' => " ".into(),
            _ => c.to_string(),
        };
    }
    // Control word: a run of letters.
    let start = *i;
    while *i < chars.len() && chars[*i].is_ascii_alphabetic() {
        *i += 1;
    }
    let name: String = chars[start..*i].iter().collect();

    if MATH_WRAPPERS.contains(&name.as_str()) {
        skip_spaces(chars, i);
        if *i < chars.len() && chars[*i] == '{' {
            *i += 1;
            return convert_math(chars, i, true, depth + 1);
        }
        return String::new();
    }
    if matches!(name.as_str(), "frac" | "dfrac" | "tfrac") {
        let a = take_group(chars, i, depth);
        let b = take_group(chars, i, depth);
        return format!("({a})/({b})");
    }
    if name == "sqrt" {
        return format!("√({})", take_group(chars, i, depth));
    }
    if let Some(sym) = math_symbol(&name) {
        return sym.to_string();
    }
    // Unknown command → its bare name. Keep the following space (readability
    // beats LaTeX's space-eating rule here); doubles collapse at the end.
    name
}

/// Read a `{…}` group's converted content; if the next token isn't a brace,
/// take a single following character (LaTeX's single-token argument rule).
fn take_group(chars: &[char], i: &mut usize, depth: usize) -> String {
    skip_spaces(chars, i);
    if *i < chars.len() && chars[*i] == '{' {
        *i += 1;
        return convert_math(chars, i, true, depth + 1);
    }
    if *i < chars.len() {
        let c = chars[*i];
        *i += 1;
        return c.to_string();
    }
    String::new()
}

/// Convert a `^`/`_` argument to Unicode super/subscript when every character
/// maps; otherwise fall back to `^(…)` / `_(…)` so nothing is lost.
fn math_script(chars: &[char], i: &mut usize, sup: bool, depth: usize) -> String {
    let arg = take_group(chars, i, depth);
    let mapped: Option<String> = arg
        .chars()
        .map(|c| if sup { super_char(c) } else { sub_char(c) })
        .collect();
    match mapped {
        Some(s) if !s.is_empty() => s,
        _ => {
            let mark = if sup { '^' } else { '_' };
            if arg.chars().count() <= 1 {
                format!("{mark}{arg}")
            } else {
                format!("{mark}({arg})")
            }
        }
    }
}

fn super_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        _ => return None,
    })
}

fn sub_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        _ => return None,
    })
}

fn skip_spaces(chars: &[char], i: &mut usize) {
    while *i < chars.len() && chars[*i] == ' ' {
        *i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn headings_and_emphasis() {
        let out = render("# Title\n\nsome **bold** and *italic* text", 80);
        let text = plain(&out);
        assert!(text.contains("Title"));
        assert!(text.contains("bold"));
        assert!(text.contains("italic"));
    }

    #[test]
    fn bold_span_is_bold_not_literal_stars() {
        let out = render("a **b** c", 80);
        // No literal ** survives.
        assert!(!plain(&out).contains("**"));
        let bolded = out
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains('b') && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(bolded);
    }

    #[test]
    fn inline_underscores_not_treated_as_italic() {
        // pulldown-cmark follows CommonMark: intraword underscores are literal.
        let out = render("call foo_bar_baz please", 80);
        assert!(plain(&out).contains("foo_bar_baz"));
    }

    #[test]
    fn math_transliterates_to_unicode() {
        // The screenshot case: a flow diagram in display math.
        assert_eq!(
            math_to_unicode(
                r"\text{Task Trajectory} \longrightarrow \text{Reflection/Diagnosis} \longrightarrow \text{Eval Gate}"
            ),
            "Task Trajectory ⟶ Reflection/Diagnosis ⟶ Eval Gate"
        );
        // Wrappers stripped, symbols mapped, super/subscripts, fractions.
        assert_eq!(
            math_to_unicode(r"\alpha \times \beta \leq \gamma"),
            "α × β ≤ γ"
        );
        assert_eq!(math_to_unicode(r"x^2 + y_1"), "x² + y₁");
        assert_eq!(math_to_unicode(r"\frac{a}{b}"), "(a)/(b)");
        assert_eq!(math_to_unicode(r"\sqrt{x+1}"), "√(x+1)");
        // Unknown command degrades to its bare name — never a raw backslash.
        assert_eq!(math_to_unicode(r"\weirdcmd x"), "weirdcmd x");
        assert!(!math_to_unicode(r"\foo^{bar}").contains('\\'));
    }

    #[test]
    fn math_renders_through_the_markdown_pipeline_no_dollars() {
        // `$$...$$` must be parsed as math (ENABLE_MATH) and transliterated, not
        // leaked as literal `$$` / raw LaTeX.
        let out = render(r"$$\text{A} \longrightarrow \text{B}$$", 80);
        let text = plain(&out);
        assert!(text.contains("A ⟶ B"), "got: {text:?}");
        assert!(!text.contains('$'), "dollar delimiters must be consumed");
        assert!(!text.contains("\\text"), "raw LaTeX must not leak");
    }

    #[test]
    fn adversarial_math_depth_and_large_highlight_are_bounded() {
        let nested = format!("{}x{}", "{".repeat(1_000), "}".repeat(1_000));
        assert!(!math_to_unicode(&nested).is_empty());
        assert!(highlight("rust", &"x".repeat(256 * 1024 + 1)).is_none());
    }

    #[test]
    fn table_renders_grid_no_raw_pipes_content() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let out = render(md, 40);
        let text = plain(&out);
        assert!(text.contains('┌') && text.contains('┐'));
        assert!(text.contains('│'));
        assert!(text.contains("A") && text.contains("B"));
        assert!(text.contains("1") && text.contains("2"));
        // The markdown separator row (---) must not leak as content.
        assert!(!text.contains("---"));
    }

    #[test]
    fn nested_list_indents() {
        let md = "- a\n  - b\n    - c\n";
        let out = render(md, 80);
        let text = plain(&out);
        assert!(text.contains('a') && text.contains('b') && text.contains('c'));
    }

    #[test]
    fn code_fence_has_border_and_content() {
        let md = "```rust\nfn main() {}\n```\n";
        let out = render(md, 80);
        let text = plain(&out);
        assert!(text.contains("rust"));
        assert!(text.contains("fn main"));
        assert!(text.contains('│'));
    }

    #[test]
    fn inline_code_inside_table_cell_stays_in_the_cell() {
        // Regression: inline code in a cell used to leak onto the transcript
        // below the table (filenames printed outside the grid).
        let md = "| Crate | Key Files |\n|---|---|\n| kernel | `loop_.rs`, `events.rs` |\n";
        let out = render(md, 70);
        let text = plain(&out);
        assert!(text.contains("loop_.rs") && text.contains("events.rs"));
        // The last non-blank rendered line must be the table's bottom border —
        // nothing leaked after it.
        let last = out
            .iter()
            .rev()
            .find(|l| !l.spans.iter().all(|s| s.content.trim().is_empty()))
            .unwrap();
        let last_text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            last_text.contains('└'),
            "table must end with its bottom border, got: {last_text:?}"
        );
    }

    #[test]
    fn task_list_markers() {
        let md = "- [x] done\n- [ ] todo\n";
        let out = render(md, 80);
        let text = plain(&out);
        assert!(text.contains('✔'));
        assert!(text.contains('○'));
    }
}
