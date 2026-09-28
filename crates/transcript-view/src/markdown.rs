//! Markdown → HTML for the desktop transcript, parsed with the same
//! `pulldown-cmark` options as the TUI. Model text is untrusted: raw HTML is
//! escaped as text, only web and mail links survive, and images are never
//! fetched — their alt text is shown instead.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

pub fn to_html(source: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut html = Writer::default();
    for event in Parser::new_ext(source, options) {
        html.event(event);
    }
    html.out
}

#[derive(Default)]
struct Writer {
    out: String,
    alignments: Vec<Alignment>,
    cell: usize,
    in_head: bool,
    links: Vec<bool>,
}

impl Writer {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => self.text(&text),
            Event::Code(code) | Event::InlineMath(code) => {
                self.out.push_str("<code>");
                self.text(&code);
                self.out.push_str("</code>");
            }
            Event::DisplayMath(math) => {
                self.out.push_str("<pre><code>");
                self.text(&math);
                self.out.push_str("</code></pre>");
            }
            Event::FootnoteReference(label) => {
                self.out.push_str("<sup>");
                self.text(&label);
                self.out.push_str("</sup>");
            }
            Event::SoftBreak => self.out.push('\n'),
            Event::HardBreak => self.out.push_str("<br>"),
            Event::Rule => self.out.push_str("<hr>"),
            Event::TaskListMarker(done) => self.out.push_str(if done {
                "<input type=\"checkbox\" disabled checked> "
            } else {
                "<input type=\"checkbox\" disabled> "
            }),
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.out.push_str("<p>"),
            Tag::Heading { level, .. } => self.out.push_str(&format!("<h{}>", level as usize)),
            Tag::BlockQuote(_) => self.out.push_str("<blockquote>"),
            Tag::CodeBlock(kind) => {
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split(|c: char| c.is_whitespace() || c == ',')
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '#'))
                        .collect::<String>(),
                    CodeBlockKind::Indented => String::new(),
                };
                if language.is_empty() {
                    self.out.push_str("<pre><code>");
                } else {
                    self.out
                        .push_str(&format!("<pre data-lang=\"{language}\"><code>"));
                }
            }
            Tag::List(Some(1)) => self.out.push_str("<ol>"),
            Tag::List(Some(start)) => self.out.push_str(&format!("<ol start=\"{start}\">")),
            Tag::List(None) => self.out.push_str("<ul>"),
            Tag::Item => self.out.push_str("<li>"),
            Tag::Table(alignments) => {
                self.alignments = alignments;
                self.out.push_str("<table>");
            }
            Tag::TableHead => {
                self.in_head = true;
                self.cell = 0;
                self.out.push_str("<thead><tr>");
            }
            Tag::TableRow => {
                self.cell = 0;
                self.out.push_str("<tr>");
            }
            Tag::TableCell => {
                let cell = if self.in_head { "th" } else { "td" };
                let align = match self.alignments.get(self.cell) {
                    Some(Alignment::Left) => " style=\"text-align:left\"",
                    Some(Alignment::Center) => " style=\"text-align:center\"",
                    Some(Alignment::Right) => " style=\"text-align:right\"",
                    _ => "",
                };
                self.out.push_str(&format!("<{cell}{align}>"));
            }
            Tag::Emphasis => self.out.push_str("<em>"),
            Tag::Strong => self.out.push_str("<strong>"),
            Tag::Strikethrough => self.out.push_str("<del>"),
            Tag::Link { dest_url, .. } => {
                let safe = safe_url(&dest_url);
                if safe {
                    self.out.push_str("<a href=\"");
                    self.text(&dest_url);
                    self.out.push_str("\">");
                }
                self.links.push(safe);
            }
            Tag::Image { .. } => self.out.push_str("<span class=\"md-image\">"),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.out.push_str("</p>"),
            TagEnd::Heading(level) => self.out.push_str(&format!("</h{}>", level as usize)),
            TagEnd::BlockQuote(_) => self.out.push_str("</blockquote>"),
            TagEnd::CodeBlock => self.out.push_str("</code></pre>"),
            TagEnd::List(true) => self.out.push_str("</ol>"),
            TagEnd::List(false) => self.out.push_str("</ul>"),
            TagEnd::Item => self.out.push_str("</li>"),
            TagEnd::Table => self.out.push_str("</tbody></table>"),
            TagEnd::TableHead => {
                self.in_head = false;
                self.out.push_str("</tr></thead><tbody>");
            }
            TagEnd::TableRow => self.out.push_str("</tr>"),
            TagEnd::TableCell => {
                self.out
                    .push_str(if self.in_head { "</th>" } else { "</td>" });
                self.cell += 1;
            }
            TagEnd::Emphasis => self.out.push_str("</em>"),
            TagEnd::Strong => self.out.push_str("</strong>"),
            TagEnd::Strikethrough => self.out.push_str("</del>"),
            TagEnd::Link => {
                if self.links.pop().unwrap_or(false) {
                    self.out.push_str("</a>");
                }
            }
            TagEnd::Image => self.out.push_str("</span>"),
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        for c in text.chars() {
            match c {
                '&' => self.out.push_str("&amp;"),
                '<' => self.out.push_str("&lt;"),
                '>' => self.out.push_str("&gt;"),
                '"' => self.out.push_str("&quot;"),
                '\'' => self.out.push_str("&#39;"),
                _ => self.out.push(c),
            }
        }
    }
}

fn safe_url(url: &str) -> bool {
    let url = url.trim();
    match url.find(':') {
        Some(colon) if !url[..colon].contains(['/', '?', '#']) => {
            matches!(
                url[..colon].to_ascii_lowercase().as_str(),
                "http" | "https" | "mailto"
            )
        }
        _ => true,
    }
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;
