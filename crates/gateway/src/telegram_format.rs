//! Markdown → the HTML Telegram reads: bold, italics, strikethrough, code, links and quotes.
//! Headings, lists and tables have no tag there, so they are drawn with text the way a phone
//! shows them well. Model text is untrusted: raw HTML stays text and only web and mail links work.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// The HTML, and its length as Telegram counts it: UTF-16 units of the text once the tags are read.
pub(crate) fn to_telegram(source: &str) -> (String, usize) {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut writer = Writer::default();
    for event in Parser::new_ext(source, options) {
        writer.event(event);
    }
    writer.finish()
}

#[derive(Default)]
struct Writer {
    out: String,
    visible: usize,
    /// Each open list's next number, or `None` for bullets.
    lists: Vec<Option<u64>>,
    /// A list item's marker, written before its first words so a task box can replace it.
    marker: Option<String>,
    quotes: usize,
    links: Vec<bool>,
    table: Option<Table>,
}

#[derive(Default)]
struct Table {
    head: Vec<String>,
    rows: Vec<Vec<String>>,
    in_head: bool,
    /// What was written before the cell being filled, and its count: cells are counted once laid out.
    outside: String,
    counted: usize,
}

impl Writer {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) | Event::InlineMath(code) | Event::DisplayMath(code) => {
                self.lead();
                self.tag("<code>");
                self.text(&code);
                self.tag("</code>");
            }
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text),
            Event::FootnoteReference(label) => self.text(&format!("[{label}]")),
            Event::SoftBreak | Event::HardBreak => self.newline(),
            Event::Rule => {
                self.block();
                self.text("──────────");
                self.block();
            }
            Event::TaskListMarker(done) => {
                self.marker = None;
                self.text(if done { "☑ " } else { "☐ " });
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.block_in_list(),
            Tag::Heading { .. } => {
                self.block();
                self.tag("<b>");
            }
            Tag::BlockQuote(_) => {
                self.block();
                // Telegram cannot nest quotes; an inner one reads as part of the outer.
                if self.quotes == 0 {
                    self.tag("<blockquote>");
                }
                self.quotes += 1;
            }
            Tag::CodeBlock(kind) => {
                self.block_in_list();
                self.lead();
                let language: String = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split(|c: char| c.is_whitespace() || c == ',')
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '#'))
                        .collect(),
                    CodeBlockKind::Indented => String::new(),
                };
                if language.is_empty() {
                    self.tag("<pre><code>");
                } else {
                    self.tag(&format!("<pre><code class=\"language-{language}\">"));
                }
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.newline();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                self.marker = Some(format!("{}{marker}", "   ".repeat(depth)));
            }
            Tag::Table(_) => {
                self.block();
                self.table = Some(Table::default());
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.outside = std::mem::take(&mut self.out);
                    table.counted = self.visible;
                }
            }
            Tag::Emphasis => self.inline_tag("<i>"),
            Tag::Strong => self.inline_tag("<b>"),
            Tag::Strikethrough => self.inline_tag("<s>"),
            Tag::Link { dest_url, .. } => {
                self.lead();
                let safe = safe_url(&dest_url);
                if safe {
                    self.tag(&format!("<a href=\"{}\">", attribute(&dest_url)));
                }
                self.links.push(safe);
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {}
            TagEnd::Heading(_) => {
                self.tag("</b>");
                self.block();
            }
            TagEnd::BlockQuote(_) => {
                self.quotes = self.quotes.saturating_sub(1);
                if self.quotes == 0 {
                    self.trim_end();
                    self.tag("</blockquote>");
                }
                self.block();
            }
            TagEnd::CodeBlock => {
                if self.out.ends_with('\n') {
                    self.out.pop();
                    self.visible -= 1;
                }
                self.tag("</code></pre>");
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.block();
                }
            }
            TagEnd::Item => self.marker = None,
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = false;
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    let cell = std::mem::replace(&mut self.out, std::mem::take(&mut table.outside));
                    self.visible = table.counted;
                    let cell = cell.trim().to_owned();
                    if table.in_head {
                        table.head.push(cell);
                    } else if let Some(row) = table.rows.last_mut() {
                        row.push(cell);
                    }
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.rows(table);
                }
                self.block();
            }
            TagEnd::Emphasis => self.tag("</i>"),
            TagEnd::Strong => self.tag("</b>"),
            TagEnd::Strikethrough => self.tag("</s>"),
            TagEnd::Link if self.links.pop().unwrap_or(false) => self.tag("</a>"),
            _ => {}
        }
    }

    /// A table is too wide for a phone: each row becomes its first cell in bold, then the
    /// others as `name: value` lines under it.
    fn rows(&mut self, table: Table) {
        for (number, row) in table.rows.into_iter().enumerate() {
            if number > 0 {
                self.push_visible("\n\n");
            }
            let mut cells = row.into_iter().enumerate();
            if let Some((_, first)) = cells.next() {
                self.out.push_str("<b>");
                self.push_cell(&first);
                self.out.push_str("</b>");
            }
            for (column, cell) in cells {
                self.push_visible("\n• ");
                if let Some(name) = table.head.get(column).filter(|name| !name.is_empty()) {
                    self.push_cell(name);
                    self.push_visible(": ");
                }
                self.push_cell(&cell);
            }
        }
    }

    /// A cell's HTML, counted by its text alone.
    fn push_cell(&mut self, cell: &str) {
        self.out.push_str(cell);
        self.visible += visible_len(cell);
    }

    fn push_visible(&mut self, text: &str) {
        self.out.push_str(text);
        self.visible += text.encode_utf16().count();
    }

    fn inline_tag(&mut self, tag: &str) {
        self.lead();
        self.tag(tag);
    }

    fn tag(&mut self, tag: &str) {
        self.out.push_str(tag);
    }

    /// A list item's marker goes before its first words.
    fn lead(&mut self) {
        if let Some(marker) = self.marker.take() {
            self.push_visible(&marker);
        }
    }

    fn text(&mut self, text: &str) {
        self.lead();
        for c in text.chars() {
            match c {
                '&' => self.out.push_str("&amp;"),
                '<' => self.out.push_str("&lt;"),
                '>' => self.out.push_str("&gt;"),
                _ => self.out.push(c),
            }
            self.visible += c.len_utf16();
        }
    }

    fn newline(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.push_visible("\n");
        }
    }

    /// Blocks are set apart by one blank line, never more.
    fn block(&mut self) {
        if self.out.is_empty() || self.out.ends_with("<blockquote>") {
            return;
        }
        self.trim_end();
        self.push_visible("\n\n");
    }

    /// In a list, an item's first paragraph stays on its marker's line and later ones start a line.
    fn block_in_list(&mut self) {
        if self.lists.is_empty() {
            self.block();
        } else if self.marker.is_none() {
            self.newline();
        }
    }

    fn trim_end(&mut self) {
        while self.out.ends_with([' ', '\n']) {
            self.out.pop();
            self.visible -= 1;
        }
    }

    fn finish(mut self) -> (String, usize) {
        self.trim_end();
        (self.out, self.visible)
    }
}

/// The length of HTML this writer made, without its tags and with entities read.
fn visible_len(html: &str) -> usize {
    let mut count = 0;
    let mut chars = html.chars();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                for c in chars.by_ref() {
                    if c == '>' {
                        break;
                    }
                }
            }
            '&' => {
                for c in chars.by_ref() {
                    if c == ';' {
                        break;
                    }
                }
                count += 1;
            }
            _ => count += c.len_utf16(),
        }
    }
    count
}

fn attribute(url: &str) -> String {
    url.trim()
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Web and mail links only; a link with no scheme is left as text, since a phone has no folder to open it in.
fn safe_url(url: &str) -> bool {
    let url = url.trim();
    url.find(':').is_some_and(|colon| {
        matches!(
            url[..colon].to_ascii_lowercase().as_str(),
            "http" | "https" | "mailto"
        )
    })
}

/// Markdown that shows `text` exactly as written.
pub(crate) fn literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `text` as inline code, with a fence longer than any run of backticks inside it.
pub(crate) fn code(text: &str) -> String {
    let fence = "`".repeat(longest_run(text, '`') + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// `text` as a code block that no line inside it can close.
pub(crate) fn code_block(text: &str) -> String {
    let fence = "`".repeat(longest_run(text, '`').max(2) + 1);
    format!("{fence}\n{text}\n{fence}")
}

fn longest_run(text: &str, c: char) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for next in text.chars() {
        run = if next == c { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    longest
}

#[cfg(test)]
#[path = "telegram_format_tests.rs"]
mod tests;
