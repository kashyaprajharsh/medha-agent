//! View layer: all rendering (transcript, input, status, welcome, diffs,
//! pickers, autocomplete). Pure functions of Model. Split out of tui_tea.rs.
#![allow(clippy::too_many_arguments)]
use super::*;
use unicode_width::UnicodeWidthStr;

pub(super) const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub(super) fn spinner_frame(frame: u64) -> &'static str {
    SPINNER[(frame as usize) % SPINNER.len()]
}

/// Human-readable verb for a tool name (used both for the live activity label and
/// the in-progress tool-call line).
/// Live-activity verb for a tool's *category*.
pub(super) fn cat_verb(cat: ToolCategory) -> &'static str {
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
pub(super) fn elapsed_str(model: &Model) -> String {
    match model.turn_started {
        Some(t) => {
            let s = t.elapsed().as_secs();
            if s >= 60 {
                format!("{}m{:02}s", s / 60, s % 60)
            } else {
                format!("{s}s")
            }
        }
        None => String::new(),
    }
}

/// Compact display of a tool target: a file's basename, or a clipped command.
pub(super) fn short_target(t: &str) -> String {
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
pub(super) fn activity_label(model: &Model) -> String {
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
    let between = if model.reasoning.enabled == Some(false) {
        "working"
    } else {
        "thinking"
    };
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
pub(super) fn veena_line(frame: u64) -> Line<'static> {
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

    let white = Style::default()
        .fg(Color::Rgb(255, 246, 214))
        .add_modifier(Modifier::BOLD);
    let bright_gold = Style::default()
        .fg(Color::Rgb(247, 208, 120))
        .add_modifier(Modifier::BOLD);
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
            "◉" | "○" => white,            // gourds always glow (purity)
            "◖" | "◗" => gold,             // gourd rim
            "╮" => dim,                    // pegbox
            "┿" => comet.unwrap_or(faint), // frets: faint, lift as resonance passes
            _ => comet.unwrap_or(dim),     // neck: warm gold, flares with the pluck
        };
        spans.push(Span::styled(*g, style));
    }
    Line::from(spans)
}

pub(super) const LOGO: &str = r#"███╗   ███╗ ███████╗ ██████╗  ██╗  ██╗  █████╗
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
pub(super) const LOGO_GRADIENT: [(u8, u8, u8); 6] = [
    (255, 248, 224),
    (247, 208, 120),
    (230, 176, 84),
    (206, 150, 78),
    (176, 126, 66),
    (150, 108, 56),
];

/// Darken an rgb toward its shadow (num/den of full brightness). Used to bevel
/// the logo's box-drawing outline beneath the bright block fill.
pub(super) fn shade(rgb: (u8, u8, u8), num: u16, den: u16) -> Color {
    let m = |c: u8| ((c as u16 * num) / den.max(1)) as u8;
    Color::Rgb(m(rgb.0), m(rgb.1), m(rgb.2))
}

/// Build one logo row: the solid `█` fill in the row's gold, the box-drawing
/// outline (╔╗╚╝║═ …) a few shades darker so each letter looks raised/engraved
/// rather than flat — a lightweight bevel using only per-glyph color.
pub(super) fn logo_row(line: &str, rgb: (u8, u8, u8)) -> Vec<Span<'static>> {
    let fill = Style::default()
        .fg(Color::Rgb(rgb.0, rgb.1, rgb.2))
        .add_modifier(Modifier::BOLD);
    let edge = Style::default()
        .fg(shade(rgb, 52, 100))
        .add_modifier(Modifier::BOLD);
    // A glyph is either solid fill (█, or a space — no visible ink) or an
    // outline edge; coalesce consecutive same-class glyphs into one span.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut buf_fill = true;
    for ch in line.chars() {
        let is_fill = ch == '█' || ch == ' ';
        if is_fill != buf_fill && !buf.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut buf),
                if buf_fill { fill } else { edge },
            ));
        }
        buf_fill = is_fill;
        buf.push(ch);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, if buf_fill { fill } else { edge }));
    }
    spans
}

pub(super) fn center_line(spans: Vec<Span<'static>>, width: u16) -> Line<'static> {
    let content: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = (width as usize).saturating_sub(content) / 2;
    let mut out = Vec::with_capacity(spans.len() + 1);
    out.push(Span::raw(" ".repeat(pad)));
    out.extend(spans);
    Line::from(out)
}

pub(super) fn lerp_color(a: (u8, u8, u8), b: (u8, u8, u8), num: i32, den: i32) -> Color {
    let mix = |x: u8, y: u8| (x as i32 + (y as i32 - x as i32) * num / den.max(1)) as u8;
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

pub(super) fn draw_welcome(f: &mut Frame, model: &Model, area: Rect) {
    let w = area.width;
    let mut body: Vec<Line> = Vec::new();
    let t = (model.anim_frame % 60) as i32;
    let level = if t < 30 { t } else { 60 - t };
    // Devanagari wordmark breathes between deep gold and Saraswati's white —
    // knowledge-light pulsing over the intellect-gold (no cool/blue tones).
    let word = lerp_color((214, 158, 74), (255, 248, 224), level, 30);
    body.push(center_line(
        vec![Span::styled(
            "◆  मेधा  ◆",
            Style::default().fg(word).add_modifier(Modifier::BOLD),
        )],
        w,
    ));
    body.push(Line::from(""));
    for (i, line) in LOGO.lines().enumerate() {
        let rgb = LOGO_GRADIENT[i.min(LOGO_GRADIENT.len() - 1)];
        body.push(center_line(logo_row(line, rgb), w));
    }
    body.push(Line::from(""));
    body.push(center_line(
        vec![Span::styled(
            "verification-first · open-first agent harness",
            Style::default()
                .fg(theme::DIM)
                .add_modifier(Modifier::ITALIC),
        )],
        w,
    ));
    body.push(Line::from(""));
    let veena = veena_line(model.anim_frame);
    body.push(center_line(veena.spans, w));
    body.push(Line::from(""));
    body.push(center_line(
        vec![Span::styled(
            "type below to begin · /help for commands · Ctrl-D to quit",
            Style::default().fg(theme::FAINT),
        )],
        w,
    ));
    let top = (area.height as usize).saturating_sub(body.len()) / 2;
    let mut lines: Vec<Line> = (0..top).map(|_| Line::from("")).collect();
    lines.extend(body);
    f.render_widget(Paragraph::new(lines), area);
}

/// A tool's presentation, resolved once from its declared spec: its own glyph
/// plus the category that drives colour/verb. The surface holds no name→glyph
/// table — the glyph is the tool's, so each stays distinct.
#[derive(Clone)]
pub(super) struct ToolViz {
    pub(super) icon: String,
    pub(super) category: ToolCategory,
}

/// Colour for a tool's *category* (glyph is the tool's own, from `ToolViz`).
pub(super) fn cat_color(cat: ToolCategory) -> Color {
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
pub(super) fn tool_label(tool: &str) -> String {
    let seg = tool.rsplit('.').next().unwrap_or(tool).replace('_', " ");
    let mut chars = seg.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => seg,
    }
}

pub(super) fn render_plan(payload: &serde_json::Value) -> Vec<Line<'static>> {
    let steps = payload.get("steps").and_then(|v| v.as_array());
    let Some(steps) = steps else {
        return vec![Line::from(Span::styled(
            "  ☰ plan updated",
            Style::default().fg(theme::DIM),
        ))];
    };
    let total = steps.len();
    let is_done = |s: &&serde_json::Value| {
        matches!(
            s.get("status").and_then(|v| v.as_str()),
            Some("completed" | "done")
        )
    };
    let done = steps.iter().filter(is_done).count();
    // A tiny progress bar so completion is readable at a glance.
    let bar_w = 10usize;
    let filled = (done * bar_w).checked_div(total).unwrap_or(0);
    let bar: String = "█"
        .repeat(filled)
        .chars()
        .chain("░".repeat(bar_w - filled).chars())
        .collect();
    let mut lines = vec![Line::from(vec![
        Span::styled("☰ ", Style::default().fg(theme::ACCENT)),
        Span::styled(
            "Plan",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {bar} {done}/{total}"),
            Style::default().fg(theme::DIM),
        ),
    ])];
    // Optional one-line note about this update (Codex-style "explanation", inline).
    if let Some(exp) = payload.get("explanation").and_then(|v| v.as_str()) {
        if !exp.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  {}", exp.trim()),
                Style::default()
                    .fg(theme::FAINT)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
    }
    for s in steps {
        let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let (mark, style) = match s.get("status").and_then(|v| v.as_str()) {
            Some("completed" | "done") => ("✔", Style::default().fg(theme::OK)),
            // Active step: accent bar + bold, and an arrow so "what's happening now"
            // is unmistakable even when the list scrolls by.
            Some("in_progress") => (
                "▶",
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            _ => ("○", Style::default().fg(theme::TEXT)),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {mark} "), style),
            Span::styled(title.to_string(), style),
        ]));
    }
    lines
}

pub(super) struct RenderCtx<'a> {
    pub(super) width: u16,
    pub(super) full_transparency: bool,
    pub(super) show_thinking: bool,
    /// Expand compaction cards to show the full summary (toggled by ^E).
    pub(super) show_summary: bool,
    /// Tool name → its declared presentation, so rendering uses each tool's own
    /// glyph + category colour (static per session; borrowed).
    pub(super) viz: &'a HashMap<String, ToolViz>,
}

pub(super) fn render_item(item: &Item, cx: &RenderCtx<'_>) -> Vec<Line<'static>> {
    match item {
        Item::User(s) => {
            let mut lines = vec![Line::from("")];
            for (i, l) in s.lines().enumerate() {
                let bar = if i == 0 { "▌ " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(bar, Style::default().fg(theme::ACCENT)),
                    Span::styled(
                        l.to_string(),
                        Style::default()
                            .fg(theme::TEXT)
                            .add_modifier(Modifier::BOLD),
                    ),
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
                Span::styled(
                    tool_label(tool),
                    Style::default()
                        .fg(theme::TEXT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(arg, Style::default().fg(theme::DIM)),
            ])];
            if cx.full_transparency {
                lines.extend(json_block(args, "in"));
            }
            lines
        }
        Item::ToolResult { tool, ok, payload } => {
            if tool == "update_plan" && *ok {
                return render_plan(payload);
            }
            if let Some(card) = payload.get("reconciliation") {
                return render_reconciliation(card);
            }
            if let (Some(old), Some(new)) = (
                payload.get("old").and_then(|v| v.as_str()),
                payload.get("new").and_then(|v| v.as_str()),
            ) {
                let path = payload.get("path").and_then(|v| v.as_str()).unwrap_or("");
                return render_diff(old, new, path, cx.width);
            }
            let (mark, color, summary) = if !*ok {
                // Failures carry {"error": …}; policy denials carry {"reason": …}. Show
                // whichever is present so the user sees WHY, not a bare "error".
                let msg = payload
                    .get("error")
                    .or_else(|| payload.get("reason"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("error")
                    .to_string();
                ("╰ ✗", theme::ERR, msg)
            } else {
                ("╰", theme::DIM, crate::result_summary(tool, payload))
            };
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("  {mark} "), Style::default().fg(theme::FAINT)),
                Span::styled(summary, Style::default().fg(color)),
            ])];
            if cx.full_transparency {
                lines.extend(json_block(payload, "out"));
            }
            lines
        }
        Item::Compaction {
            before,
            after,
            summarized,
            summary,
        } => {
            let how = if *summarized { "summarized" } else { "pruned" };
            let hint = match summary {
                Some(_) if cx.show_summary => "  (^E to collapse)",
                Some(_) => "  (^E to expand summary)",
                None => "",
            };
            let mut lines = vec![Line::from(Span::styled(
                format!("  ↯ {how} context · {before} → {after} tokens{hint}"),
                Style::default().fg(theme::WARN),
            ))];
            if cx.show_summary {
                if let Some(s) = summary {
                    for l in s.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("    {l}"),
                            Style::default().fg(theme::DIM),
                        )));
                    }
                }
            }
            lines
        }
        Item::Verify { ok, summary } => {
            let (mark, color) = if *ok {
                ("✔", theme::OK)
            } else {
                ("✗", theme::ERR)
            };
            vec![Line::from(vec![
                Span::styled(
                    format!("{mark} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("verify · {summary}"), Style::default().fg(color)),
            ])]
        }
        Item::Notice(s) => s
            .lines()
            .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(theme::DIM))))
            .collect(),
        Item::Thinking(s) => {
            let style = Style::default()
                .fg(theme::DIM)
                .add_modifier(Modifier::ITALIC);
            if !cx.show_thinking {
                return vec![Line::from(Span::styled(
                    "  · reasoning (hidden — /reasoning show)",
                    Style::default()
                        .fg(theme::FAINT)
                        .add_modifier(Modifier::ITALIC),
                ))];
            }
            let mut lines = vec![Line::from(Span::styled("  · reasoning", style))];
            lines.extend(
                s.lines()
                    .map(|l| Line::from(Span::styled(format!("  {l}"), style))),
            );
            lines
        }
    }
}

fn render_reconciliation(card: &serde_json::Value) -> Vec<Line<'static>> {
    let name = card.get("name").and_then(|value| value.as_str()).unwrap_or("memory");
    let previous = card
        .get("previous")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let proposed = card
        .get("proposed")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    vec![
        Line::from(Span::styled(
            format!("╭─ memory contradiction · {name}"),
            Style::default().fg(theme::WARN).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("│ previous  {previous}")),
        Line::from(format!("│ proposed  {proposed}")),
        Line::from(Span::styled(
            "╰─ keep previous · replace with proposed · merge as a new claim",
            Style::default().fg(theme::DIM),
        )),
    ]
}

/// Inline approval rendering (PART 3: appended to the transcript stream, not a modal).
/// Rendered as a plain block in the same scrollable region — heading, diff hunk, then
/// options as plain numbered lines. Never a floating overlay, so it can never be clipped.
pub(super) fn render_approval(
    action: &str,
    detail: Option<&str>,
    sel: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Allow ", Style::default().fg(theme::TEXT)),
            Span::styled(
                tool_label(action).to_string(),
                Style::default()
                    .fg(theme::WARN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("?", Style::default().fg(theme::TEXT)),
        ]),
    ];
    if let Some(detail) = detail {
        lines.push(Line::from(""));
        for l in detail.lines().take(18) {
            let style = if l.starts_with('+') && !l.starts_with("+++") {
                Style::default().fg(theme::ADD_FG)
            } else if l.starts_with('-') && !l.starts_with("---") {
                Style::default().fg(theme::DEL_FG)
            } else {
                Style::default().fg(theme::DIM)
            };
            lines.push(Line::from(Span::styled(l.to_string(), style)));
        }
        let extra = detail.lines().count().saturating_sub(18);
        if extra > 0 {
            lines.push(Line::from(Span::styled(
                format!("… {extra} more lines"),
                Style::default().fg(theme::FAINT),
            )));
        }
    }
    lines.push(Line::from(""));
    let opts = ["Yes, allow once", "Yes, always allow", "No, deny"];
    for (i, label) in opts.iter().enumerate() {
        if i == sel {
            lines.push(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(theme::ACCENT)),
                Span::styled(
                    format!("{}. ", i + 1),
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label.to_string(),
                    Style::default()
                        .fg(theme::TEXT)
                        .add_modifier(Modifier::BOLD),
                ),
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
        Span::styled(
            "› ",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("waiting for your input", Style::default().fg(theme::ACCENT)),
    ]));
    lines.push(Line::from(Span::styled(
        "↑↓ + enter · or press 1/2/3 · n to deny",
        Style::default().fg(theme::FAINT),
    )));
    lines
}

pub(super) fn render_assistant(s: &str) -> Vec<Line<'static>> {
    s.lines()
        .map(|l| checklist_line(l).unwrap_or_else(|| Line::from(l.to_string())))
        .collect()
}

pub(super) fn checklist_line(line: &str) -> Option<Line<'static>> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let rest = &line[indent_len..];
    let body = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))?;
    let (mark, color, text, done) = if let Some(t) = body
        .strip_prefix("[x]")
        .or_else(|| body.strip_prefix("[X]"))
    {
        ("✔", theme::OK, t.trim_start(), true)
    } else if let Some(t) = body.strip_prefix("[ ]") {
        ("○", theme::DIM, t.trim_start(), false)
    } else {
        return None;
    };
    let text_style = if done {
        Style::default().fg(theme::DIM)
    } else {
        Style::default().fg(theme::TEXT)
    };
    Some(Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(
            format!("{mark} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(text.to_string(), text_style),
    ]))
}

pub(super) fn json_block(v: &serde_json::Value, label: &str) -> Vec<Line<'static>> {
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
        lines.push(Line::from(Span::styled(
            format!("    └ [+{hidden} more lines — toggle /detail]"),
            Style::default().fg(theme::FAINT),
        )));
    }
    lines
}

pub(super) const MIN_SIDE_BY_SIDE: u16 = 96;

/// Lines of unchanged context kept above/below each change (PART 5: hunk-based).
pub(super) const DIFF_CONTEXT: usize = 3;

/// One display row of a hunk-filtered diff.
pub(super) enum DiffRow {
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
pub(super) fn hunk_rows(old: &str, new: &str) -> Vec<DiffRow> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let raw: Vec<(ChangeTag, Option<usize>, Option<usize>, String)> = diff
        .iter_all_changes()
        .map(|c| {
            (
                c.tag(),
                c.old_index(),
                c.new_index(),
                c.value().trim_end_matches(['\n', '\r']).to_string(),
            )
        })
        .collect();

    let is_change: Vec<bool> = raw
        .iter()
        .map(|(t, ..)| !matches!(t, ChangeTag::Equal))
        .collect();
    let keep: Vec<bool> = (0..raw.len())
        .map(|i| {
            if is_change[i] {
                return true;
            }
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

pub(super) fn gap_line(n: usize) -> Line<'static> {
    let plural = if n == 1 { "" } else { "s" };
    Line::from(Span::styled(
        format!("  ⋯ {n} unchanged line{plural}"),
        Style::default().fg(theme::FAINT),
    ))
}

pub(super) fn render_diff(old: &str, new: &str, path: &str, width: u16) -> Vec<Line<'static>> {
    let rows = hunk_rows(old, new);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if !path.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  ✎ ", Style::default().fg(theme::FAINT)),
            Span::styled(
                path.to_string(),
                Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    let ctx_num = Style::default().fg(theme::LINENO);
    let clip = |s: &str, w: usize| -> String {
        let t = s.trim_end_matches(['\n', '\r']);
        let n = t.chars().count();
        if n > w {
            let mut out: String = t.chars().take(w.saturating_sub(1)).collect();
            out.push('…');
            out
        } else {
            format!("{t:<w$}")
        }
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
                DiffRow::Gap(n) => {
                    lines.push(gap_line(*n));
                    continue;
                }
                DiffRow::Del(oi, t) => ("-", theme::DEL_FG, Some(theme::DEL_BG), *oi, t),
                DiffRow::Ins(ni, t) => ("+", theme::ADD_FG, Some(theme::ADD_BG), *ni, t),
                DiffRow::Ctx(_, ni, t) => (" ", theme::DIM, None, *ni, t),
            };
            let n = format!("{:>4}", num + 1);
            let text = clip(text, body_w);
            let mut rowst = Style::default().fg(fg);
            let mut numst = ctx_num;
            if let Some(bg) = bg {
                rowst = rowst.bg(bg);
                numst = numst.bg(bg);
            }
            lines.push(Line::from(vec![
                Span::styled(format!("  {n} "), numst),
                Span::styled(format!("{sign} {text}"), rowst),
            ]));
        }
        return cap_diff(lines);
    }
    let col = ((width as usize).saturating_sub(14)) / 2;
    let push_row = |lines: &mut Vec<Line<'static>>,
                    ln: Option<usize>,
                    left: Option<&str>,
                    rn: Option<usize>,
                    right: Option<&str>,
                    changed: bool| {
        let (lfg, lbg) = if changed && left.is_some() {
            (theme::DEL_FG, Some(theme::DEL_BG))
        } else {
            (theme::DIM, None)
        };
        let (rfg, rbg) = if changed && right.is_some() {
            (theme::ADD_FG, Some(theme::ADD_BG))
        } else {
            (theme::DIM, None)
        };
        let mut lst = Style::default().fg(lfg);
        let mut rst = Style::default().fg(rfg);
        if let Some(b) = lbg {
            lst = lst.bg(b);
        }
        if let Some(b) = rbg {
            rst = rst.bg(b);
        }
        let lnum = ln
            .map(|i| format!("{:>4}", i + 1))
            .unwrap_or_else(|| "    ".into());
        let rnum = rn
            .map(|i| format!("{:>4}", i + 1))
            .unwrap_or_else(|| "    ".into());
        let ltext = clip(left.unwrap_or(""), col);
        let rtext = clip(right.unwrap_or(""), col);
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
    let flush = |lines: &mut Vec<Line<'static>>,
                 dels: &mut Vec<(usize, String)>,
                 inss: &mut Vec<(usize, String)>| {
        let n = dels.len().max(inss.len());
        for i in 0..n {
            let d = dels.get(i);
            let ins = inss.get(i);
            push_row(
                lines,
                d.map(|(n, _)| *n),
                d.map(|(_, s)| s.as_str()),
                ins.map(|(n, _)| *n),
                ins.map(|(_, s)| s.as_str()),
                true,
            );
        }
        dels.clear();
        inss.clear();
    };
    for row in rows {
        match row {
            DiffRow::Del(oi, text) => dels.push((oi, text)),
            DiffRow::Ins(ni, text) => inss.push((ni, text)),
            DiffRow::Ctx(oi, ni, text) => {
                flush(&mut lines, &mut dels, &mut inss);
                push_row(
                    &mut lines,
                    Some(oi),
                    Some(&text),
                    Some(ni),
                    Some(&text),
                    false,
                );
            }
            DiffRow::Gap(n) => {
                flush(&mut lines, &mut dels, &mut inss);
                lines.push(gap_line(n));
            }
        }
    }
    flush(&mut lines, &mut dels, &mut inss);
    cap_diff(lines)
}

pub(super) fn cap_diff(mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if lines.len() > MAX_DIFF_LINES {
        let hidden = lines.len() - MAX_DIFF_LINES;
        lines.truncate(MAX_DIFF_LINES);
        lines.push(Line::from(Span::styled(
            format!("  … {hidden} more diff lines"),
            Style::default().fg(theme::FAINT),
        )));
    }
    lines
}

pub(super) fn draw_status(f: &mut Frame, model: &Model, area: Rect) {
    let mut left = vec![
        Span::styled("▌ ", Style::default().fg(theme::ACCENT)),
        Span::styled(
            "medha",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", model.model),
            Style::default().fg(theme::DIM),
        ),
    ];
    // Autonomy badge — always visible so the user knows how much runs without
    // asking. yolo is loud (bold WARN) since it auto-runs edits + shell.
    let (mode_txt, mode_style) = match model.autonomy {
        kernel::AutonomyLevel::Careful => ("careful", Style::default().fg(theme::DIM)),
        kernel::AutonomyLevel::Normal => ("normal", Style::default().fg(theme::DIM)),
        kernel::AutonomyLevel::Yolo => (
            "⚠ yolo",
            Style::default()
                .fg(theme::WARN)
                .add_modifier(Modifier::BOLD),
        ),
    };
    left.push(Span::styled(format!("  [{mode_txt}]"), mode_style));
    // While a form/approval is up the turn is PAUSED on the user, not working —
    // say so, or the running spinner makes it look like the agent ran off on its
    // own (it hasn't; it's blocked awaiting your answer).
    let awaiting_user = model.clarify.is_some() || model.pending_approval().is_some();
    if awaiting_user {
        let what = if model.clarify.is_some() {
            "waiting for your answer"
        } else {
            "waiting for your approval"
        };
        left.push(Span::styled(
            format!("  ⏸ {what}"),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
    } else if model.running {
        left.push(Span::styled(
            format!(
                "  {} {} · {}",
                spinner_frame(model.anim_frame),
                activity_label(model),
                elapsed_str(model)
            ),
            Style::default().fg(theme::WARN),
        ));
    }
    // Live "compacting…" indicator while a summarize pass calls the model.
    if model.compacting {
        left.push(Span::styled(
            format!("  {} compacting context…", spinner_frame(model.anim_frame)),
            Style::default()
                .fg(theme::WARN)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Live background-task indicator: an animated glyph + count, so the user sees
    // what's still running (a promoted `shell.exec`) even when no turn is active.
    // `/tasks` lists them; `task.kill` (or the model) stops them.
    let running_bg = model.bg_running();
    if running_bg > 0 {
        const SPIN: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
        let g = SPIN[(model.anim_frame as usize / 4) % SPIN.len()];
        let word = if running_bg == 1 { "task" } else { "tasks" };
        left.push(Span::styled(
            format!("  {g} {running_bg} bg {word}"),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let ctx = match model.ctx_pct {
        Some(pct) => format!("ctx {pct}%"),
        None => "ctx —".to_string(),
    };
    // Cost so far, when pricing resolved (P1-12); "~…est." marks an indicative
    // list price (self-hosted routes aren't billed it). No pricing → no line.
    let cost = match model.cost_usd {
        Some((usd, true)) => format!(" · ~${usd:.2} est."),
        Some((usd, false)) => format!(" · ${usd:.2}"),
        None => String::new(),
    };
    let mode = match model.reasoning.enabled {
        Some(true) => "on",
        Some(false) => "off",
        None => "default",
    };
    let visibility = if model.show_thinking {
        "shown"
    } else {
        "hidden"
    };
    let effort = crate::effort_label(model.reasoning.effort);
    let trace = model.reasoning_trace_label();
    let reasoning = format!("reasoning {mode} · {visibility} · {effort} · {trace}");
    // Only surface the streaming state when it's OFF — on is the norm and adds
    // noise to the status bar.
    let stream = if model.streaming { "" } else { " · stream off" };
    let hints = if model.running {
        "esc interrupt"
    } else {
        "/reasoning · /detail · /help"
    };
    let right = format!("{ctx}{cost} · {reasoning}{stream}   {hints}");
    // Pad in terminal cells (K14) so the right block stays right-aligned even
    // with wide glyphs in the left block.
    let left_w: usize = left
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let pad = (area.width as usize).saturating_sub(left_w + UnicodeWidthStr::width(right.as_str()));
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, Style::default().fg(theme::FAINT)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Lay the input box out in terminal CELLS (K14): rows wrap on display width
/// (CJK/emoji = 2 columns) and the returned cursor column is a cell offset, so
/// the terminal cursor lands on the right glyph, not two cells short.
pub(super) fn layout_input(text: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let cell_w = |c: char| c.width().unwrap_or(0);
    let chars: Vec<char> = text.chars().collect();
    let cur = cursor.min(chars.len());
    let mut rows: Vec<String> = vec![String::new()];
    let mut row_w = 0usize; // cell width of the row being built
    let (mut crow, mut ccol) = (0usize, 0usize);
    for (i, &ch) in chars.iter().enumerate() {
        if i == cur {
            crow = rows.len() - 1;
            ccol = row_w;
        }
        if ch == '\n' {
            rows.push(String::new());
            row_w = 0;
        } else {
            let cw = cell_w(ch);
            if row_w + cw > width && row_w > 0 {
                rows.push(String::new());
                row_w = 0;
                if i == cur {
                    crow = rows.len() - 1;
                    ccol = 0;
                }
            }
            rows.last_mut().unwrap().push(ch);
            row_w += cw;
        }
    }
    if cur >= chars.len() {
        crow = rows.len() - 1;
        ccol = row_w;
    }
    (rows, crow, ccol)
}

pub(super) fn input_text_width(outer_width: u16) -> usize {
    outer_width.saturating_sub(6).max(1) as usize
}
pub(super) fn input_rows(model: &Model, outer_width: u16) -> usize {
    if model.input.is_empty() {
        return 1;
    }
    layout_input(&model.input, 0, input_text_width(outer_width))
        .0
        .len()
}

pub(super) fn draw_input(f: &mut Frame, model: &Model, area: Rect) {
    let (accent, glyph) = if model.running {
        (theme::FAINT, "…")
    } else {
        (theme::ACCENT, "❯")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if model.input.is_empty() && !model.running {
        if let Some(setup) = &model.model_setup {
            let prompt = if matches!(
                model.picker.as_ref().map(|p| &p.kind),
                Some(PickerKind::ProviderPreset)
            ) {
                "Choose a provider from the list above"
            } else {
                setup.prompt()
            };
            let line = Line::from(vec![
                Span::styled(
                    format!("{glyph} "),
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(prompt, Style::default().fg(theme::FAINT)),
            ]);
            f.render_widget(Paragraph::new(line), inner);
            f.set_cursor_position(ratatui::layout::Position::new(inner.x + 2, inner.y));
            return;
        }
        if let Some(setup) = &model.search_setup {
            let prompt = if matches!(
                model.picker.as_ref().map(|p| &p.kind),
                Some(PickerKind::SearchProvider)
            ) {
                "Choose a web-search provider from the list above"
            } else {
                setup.prompt()
            };
            let line = Line::from(vec![
                Span::styled(
                    format!("{glyph} "),
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(prompt, Style::default().fg(theme::FAINT)),
            ]);
            f.render_widget(Paragraph::new(line), inner);
            f.set_cursor_position(ratatui::layout::Position::new(inner.x + 2, inner.y));
            return;
        }
        let line = Line::from(vec![
            Span::styled(
                format!("{glyph} "),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Ask medha to build, fix, or explain something…   ( / for commands · \\ + enter or ctrl+j for newline )",
                Style::default().fg(theme::FAINT),
            ),
        ]);
        f.render_widget(Paragraph::new(line), inner);
        f.set_cursor_position(ratatui::layout::Position::new(inner.x + 2, inner.y));
        return;
    }
    let tw = inner.width.saturating_sub(2).max(1) as usize;
    // `cursor` is a byte offset; layout_input positions by char index.
    let cursor_chars = model.input[..model.cursor.min(model.input.len())]
        .chars()
        .count();
    let display_input = if model
        .model_setup
        .as_ref()
        .is_some_and(ModelSetup::is_secret)
        || model
            .search_setup
            .as_ref()
            .is_some_and(SearchSetup::is_secret)
    {
        "•".repeat(model.input.chars().count())
    } else {
        model.input.clone()
    };
    let (rows, crow, ccol) = layout_input(&display_input, cursor_chars, tw);
    let lines: Vec<Line> = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let gutter = if i == 0 {
                Span::styled(
                    format!("{glyph} "),
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("  ")
            };
            Line::from(vec![
                gutter,
                Span::styled(row, Style::default().fg(theme::TEXT)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
    if !model.running {
        f.set_cursor_position(ratatui::layout::Position::new(
            inner.x + 2 + ccol as u16,
            inner.y + crow as u16,
        ));
    }
}

pub(super) fn draw_autocomplete(f: &mut Frame, model: &Model, input_area: Rect) {
    let matches = command_matches(&model.input);
    if matches.is_empty() {
        return;
    }
    let n = matches.len();
    let sel = model.ac_sel.min(n - 1);
    // Never let the menu grow taller than the space above the input box (it
    // would push the prompt off-screen). Reserve one row for the hint line and
    // one top margin, then window around the selection — same discipline as the
    // picker overlay.
    let capacity = (input_area.y as usize).saturating_sub(2).max(1);
    let visible = n.min(capacity).max(1);
    let start = if n <= visible {
        0
    } else {
        sel.saturating_sub(visible / 2).min(n - visible)
    };
    let end = (start + visible).min(n);

    let height = visible as u16 + 1; // + hint row
    let y = input_area.y.saturating_sub(height + 1);
    let area = Rect::new(input_area.x, y, input_area.width, height + 1);
    f.render_widget(ratatui::widgets::Clear, area);
    let mut lines: Vec<Line> = Vec::with_capacity(visible + 1);
    for (i, (c, d)) in matches[start..end].iter().enumerate() {
        let idx = start + i;
        if idx == sel {
            lines.push(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(theme::ACCENT)),
                Span::styled(
                    c.to_string(),
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {d}"), Style::default().fg(theme::DIM)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(c.to_string(), Style::default().fg(theme::TEXT)),
                Span::styled(format!("  {d}"), Style::default().fg(theme::FAINT)),
            ]));
        }
    }
    // Show how many are scrolled out of view, so a windowed menu isn't silent.
    let more = n - (end - start);
    let hint = if more > 0 {
        format!("  ↑↓ select · tab/enter accept · esc dismiss · +{more} more")
    } else {
        "  ↑↓ select · tab/enter accept · esc dismiss".to_string()
    };
    lines.push(Line::from(Span::styled(hint, Style::default().fg(theme::FAINT))));
    f.render_widget(Paragraph::new(lines), area);
}

pub(super) fn draw_picker(f: &mut Frame, picker: &Picker, input_area: Rect) {
    let labels = picker.kind.labels();
    let n = labels.len();
    // Fit the picker into the space actually available above the input box —
    // no magic row count. `input_area.y` is how many rows sit above the input;
    // reserve one for the title, keep one as a top margin. A long session list
    // then windows around the selection instead of overflowing the screen.
    let capacity = (input_area.y as usize).saturating_sub(2).max(1);
    let visible = n.min(capacity).max(1);
    let start = if n <= visible {
        0
    } else {
        picker.selected.saturating_sub(visible / 2).min(n - visible)
    };
    let end = (start + visible).min(n);

    let height = visible as u16 + 1; // + title row
    let y = input_area.y.saturating_sub(height);
    let area = Rect::new(input_area.x, y, input_area.width, height);
    f.render_widget(ratatui::widgets::Clear, area);

    // Title shows position (e.g. "3/27") when the list is windowed off-screen.
    let mut title = picker.kind.title().trim().to_string();
    if n > visible {
        title = format!("{title}  ({}/{n})", picker.selected + 1);
    }
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        format!("  {title}"),
        Style::default().fg(theme::FAINT),
    ))];
    for (offset, label) in labels[start..end].iter().enumerate() {
        let i = start + offset;
        if i == picker.selected {
            lines.push(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(theme::ACCENT)),
                Span::styled(
                    label.clone(),
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                format!("  {label}"),
                Style::default().fg(theme::TEXT),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Render the `clarify` question form as a bordered card above the input box:
/// the question up top, options as radio (single) / checkbox (multi) rows with
/// the recommended one starred, then an "Other" row that becomes an inline
/// editor when selected. Long content is pre-wrapped into physical terminal rows
/// and the card scrolls just enough to keep the focused row visible on short
/// terminals.
pub(super) fn draw_clarify(f: &mut Frame, state: &ClarifyState, input_area: Rect) {
    use ratatui::widgets::{Block, BorderType, Borders};

    let q = &state.questions[state.idx];
    let draft = &state.drafts[state.idx];
    let editing = state.entering_other;
    let multi_q = state.questions.len() > 1;

    let mut lines: Vec<Line> = Vec::new();

    // Tab row — shows EVERY question so the user sees there are several and can
    // switch. Current = accent ▸; answered = ✓; still-open = dim ○.
    if multi_q {
        let mut tabs: Vec<Span> = Vec::new();
        for (i, qq) in state.questions.iter().enumerate() {
            let label = if qq.header.trim().is_empty() {
                format!("Q{}", i + 1)
            } else {
                qq.header.trim().to_string()
            };
            let answered = !state.drafts[i].selected.is_empty() || state.drafts[i].other.is_some();
            let (mark, style) = if i == state.idx {
                (
                    "▸",
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
            } else if answered {
                ("✓", Style::default().fg(theme::TEXT))
            } else {
                ("○", Style::default().fg(theme::DIM))
            };
            tabs.push(Span::styled(format!("{mark} {label}   "), style));
        }
        lines.push(Line::from(tabs));
        lines.push(Line::from(Span::styled(
            "  ←→ switch questions",
            Style::default().fg(theme::FAINT),
        )));
        lines.push(Line::from(""));
    }

    // Question prompt — prominent, at the top of the question section. (Header is
    // in the tab row when there are several; inline it for a lone question.)
    let head = if multi_q || q.header.trim().is_empty() {
        String::new()
    } else {
        format!("[{}] ", q.header.trim())
    };
    lines.push(Line::from(Span::styled(
        format!("{head}{}", q.prompt),
        Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from("")); // breathing room

    // Options. While editing "Other", dim them so focus is on the text field.
    let options_start = lines.len();
    for (i, opt) in q.options.iter().enumerate() {
        let on = draft.selected.contains(&i);
        let marker = if q.multi_select {
            if on { "☑" } else { "☐" }
        } else if on {
            "◉"
        } else {
            "○"
        };
        let star = if opt.recommended { " ★" } else { "" };
        let desc = if opt.description.trim().is_empty() {
            String::new()
        } else {
            format!("  — {}", opt.description.trim())
        };
        let text = format!("{marker} {}{star}{desc}", opt.label);
        if editing {
            lines.push(Line::from(Span::styled(
                format!("  {text}"),
                Style::default().fg(theme::DIM),
            )));
        } else {
            lines.push(row_line(&text, state.cursor == i));
        }
    }

    // The "Other" row: an inline editor when active (shows the live buffer + a
    // block cursor), otherwise a normal selectable row echoing any saved text.
    let other_line = lines.len();
    if editing {
        let cursor = state.other_cursor.min(state.other_input.len());
        debug_assert!(state.other_input.is_char_boundary(cursor));
        let (before, after) = state.other_input.split_at(cursor);
        lines.push(Line::from(vec![
            Span::styled("▌ ✎ ", Style::default().fg(theme::ACCENT)),
            Span::styled(before.to_string(), Style::default().fg(theme::TEXT)),
            Span::styled("▏", Style::default().fg(theme::ACCENT)),
            Span::styled(after.to_string(), Style::default().fg(theme::TEXT)),
        ]));
    } else {
        let other_txt = match &draft.other {
            Some(t) if !t.is_empty() => format!("✎ Other: {t}"),
            _ => "✎ Other…".to_string(),
        };
        lines.push(row_line(&other_txt, state.cursor == state.other_row()));
    }

    if let Some(message) = &state.validation {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  ⚠ {message}"),
            Style::default()
                .fg(theme::WARN)
                .add_modifier(Modifier::BOLD),
        )));
    }

    // Hint line, context-dependent.
    let hint = if editing {
        "  type your answer · enter save · esc cancel".to_string()
    } else {
        let pick = if q.multi_select {
            "space toggle"
        } else {
            "space pick"
        };
        format!("  ↑↓ options · {pick} · enter submit · esc skip")
    };
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(theme::FAINT),
    )));

    // Card title shows progress across questions.
    let title = if state.questions.len() > 1 {
        format!(" clarify · {}/{} ", state.idx + 1, state.questions.len())
    } else {
        " clarify ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ))
        .padding(ratatui::widgets::Padding::horizontal(1));

    // Pre-wrap with the exact inner width (border + horizontal padding consume
    // four cells). The resulting row count is both rendering and layout truth.
    let inner_width = input_area.width.saturating_sub(4).max(1) as usize;
    let focus_logical = if editing {
        other_line
    } else {
        options_start + state.cursor
    };
    let mut physical = Vec::new();
    let mut focus_row = 0usize;
    for (i, line) in lines.into_iter().enumerate() {
        if i == focus_logical {
            focus_row = physical.len();
        }
        physical.extend(wrap_line(&line, inner_width));
    }

    // Never cover the composer/status on a short terminal. If the full card does
    // not fit above it, scroll the content so the active row remains reachable.
    let available_height = input_area.y;
    if available_height < 3 {
        return;
    }
    let desired_height = (physical.len() as u16).saturating_add(2);
    let height = desired_height.min(available_height);
    let y = input_area.y - height;
    let area = Rect::new(input_area.x, y, input_area.width, height);
    f.render_widget(ratatui::widgets::Clear, area);
    let inner = block.inner(area);
    let visible_rows = inner.height.max(1) as usize;
    let scroll = focus_row.saturating_sub(visible_rows.saturating_sub(1));
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(physical).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        inner,
    );
}

/// One selectable row, highlighted (accent + ▌) when it's the cursor.
fn row_line(text: &str, selected: bool) -> Line<'static> {
    if selected {
        Line::from(vec![
            Span::styled("▌ ", Style::default().fg(theme::ACCENT)),
            Span::styled(
                text.to_string(),
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(Span::styled(
            format!("  {text}"),
            Style::default().fg(theme::TEXT),
        ))
    }
}

/// View function — pure rendering from model
pub(super) fn view(f: &mut Frame, model: &mut Model) {
    let area = f.area();
    model.viewport_height = area.height as usize;

    let text_rows = input_rows(model, area.width.saturating_sub(4)) as u16;
    let box_h = text_rows.clamp(1, 8) + 2;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(box_h),
            Constraint::Length(1),
        ])
        .split(area);

    let pad_h = |area: Rect| {
        let pad = 2.min(area.width / 2);
        Rect {
            x: area.x + pad,
            width: area.width.saturating_sub(pad * 2),
            ..area
        }
    };

    draw_transcript(f, model, pad_h(chunks[0]));
    draw_input(f, model, pad_h(chunks[2]));
    draw_status(f, model, pad_h(chunks[3]));

    // While an approval card is pending it owns the keyboard — drawing the
    // autocomplete/picker under it would show interactive-looking menus that
    // don't respond ("stuck menu"). They come back once the card is answered.
    let gate_open = model.pending_approval().is_some() || model.clarify.is_some();
    if !gate_open
        && model.model_setup.is_none()
        && model.search_setup.is_none()
        && model.input.starts_with('/')
    {
        draw_autocomplete(f, model, pad_h(chunks[2]));
    }
    if !gate_open {
        if let Some(picker) = &model.picker {
            draw_picker(f, picker, pad_h(chunks[2]));
        }
    }
    // The clarify form owns the overlay space while it's up (like the approval card).
    if let Some(state) = &model.clarify {
        draw_clarify(f, state, pad_h(chunks[2]));
    }
}

pub(super) fn draw_transcript(f: &mut Frame, model: &mut Model, area: Rect) {
    // Scroll math must use the TRANSCRIPT pane height, not the full frame height
    // (set in view()), or the last lines — e.g. the approval options — get clipped
    // off the bottom even when auto-scrolled.
    model.viewport_height = area.height as usize;
    // Show the welcome splash only while the transcript is truly empty. Guarding
    // on `items.is_empty()` too means any pushed content (e.g. a `/skills` notice
    // run as the first action) always wins — the splash can never hide it.
    if model.welcome && model.items.is_empty() {
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
        let cx = RenderCtx {
            width: area.width,
            full_transparency: model.full_transparency,
            show_thinking: model.show_thinking,
            show_summary: model.show_summary,
            viz: &model.tool_viz,
        };
        let mut total = 0usize;
        for e in model.items.iter_mut() {
            e.ensure(&cx, vw);
            total += e.height;
        }
        // Approval card: pre-wrap into physical rows too (PART 3, inline in stream).
        model.approval_rows = if let Some(pending) = model.pending_approval() {
            let mut rows = render_approval(
                &pending.action,
                pending.detail.as_deref(),
                model.approval_sel,
            );
            // If more approvals are queued behind the current one, say so — so the
            // user knows to expect another prompt right after this one.
            if model.pending_approvals.len() > 1 {
                rows.push(Line::from(Span::styled(
                    format!(
                        "+{} more approval{} waiting",
                        model.pending_approvals.len() - 1,
                        if model.pending_approvals.len() > 2 {
                            "s"
                        } else {
                            ""
                        }
                    ),
                    Style::default().fg(theme::FAINT),
                )));
            }
            rows.iter()
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
    if model.auto_scroll {
        model.scroll_offset = model.max_scroll();
    }

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
            Span::styled(
                spinner_frame(model.anim_frame),
                Style::default().fg(theme::ACCENT),
            ),
            Span::styled(
                format!(" {}…", activity_label(model)),
                Style::default().fg(theme::DIM),
            ),
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

#[cfg(test)]
mod clarify_view_tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn state(prompt: &str, description: &str) -> ClarifyState {
        let (responder, _receiver) = tokio::sync::oneshot::channel();
        ClarifyState {
            questions: vec![kernel::Question {
                prompt: prompt.into(),
                header: "Architecture".into(),
                options: vec![
                    kernel::QOption {
                        label: "Alpha".into(),
                        description: description.into(),
                        recommended: true,
                    },
                    kernel::QOption {
                        label: "Beta".into(),
                        description: "A second valid choice".into(),
                        recommended: false,
                    },
                ],
                multi_select: false,
            }],
            idx: 0,
            drafts: vec![ClarifyDraft {
                selected: vec![0],
                other: None,
            }],
            cursor: 0,
            entering_other: false,
            other_input: String::new(),
            other_cursor: 0,
            validation: None,
            responder,
        }
    }

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn reconciliation_card_shows_both_versions_and_actions() {
        let lines = render_reconciliation(&serde_json::json!({
            "name": "cache-key",
            "previous": "use 'old-key'",
            "proposed": "use new-key",
        }));
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("cache-key"));
        assert!(text.contains("old-key"));
        assert!(text.contains("new-key"));
        assert!(text.contains("keep previous"));
        assert!(text.contains("merge"));
    }

    #[test]
    fn clarify_wraps_long_content_instead_of_truncating_it() {
        let mut terminal = Terminal::new(TestBackend::new(28, 34)).unwrap();
        let state = state(
            "Choose the architecture that best fits this deliberately narrow terminal",
            "This explanation must wrap and retain its important-tail",
        );
        terminal
            .draw(|frame| draw_clarify(frame, &state, Rect::new(0, 30, 28, 3)))
            .unwrap();

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("important-tail"));
        assert!(rendered.contains("narrow"));
        assert!(rendered.contains("terminal"));
    }

    #[test]
    fn clarify_short_card_stays_above_composer_and_keeps_focus_visible() {
        let mut terminal = Terminal::new(TestBackend::new(24, 10)).unwrap();
        let state = state(
            "A prompt long enough to consume several wrapped rows",
            "A long explanation that also wraps",
        );
        let input_area = Rect::new(0, 6, 24, 3);
        terminal
            .draw(|frame| draw_clarify(frame, &state, input_area))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Alpha"), "focused option remains visible");
        for y in input_area.y..buffer.area.height {
            for x in 0..buffer.area.width {
                assert_eq!(buffer[(x, y)].symbol(), " ", "card covered composer/status");
            }
        }
    }
}
